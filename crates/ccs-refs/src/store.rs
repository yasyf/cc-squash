//! `RefStore` — the single tokio-rusqlite actor: sole writer (`put`), sole Rust
//! reader (`materialize`), plus `retrieve` and `gc`. One connection, never a
//! second; Layer 6's Go RO-CAS host opens `mode=ro` as a separate process. The
//! schema is additive-only once that host reads it.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashSet;
use std::path::Path;

use ccs_core::{estimate_chars_proxy, MessageId, RefId, SegmentKind, SessionId, TokenCount};
use tokio_rusqlite::{params, Connection, OptionalExtension};

use crate::bm25;
use crate::hash::content_address;
use crate::record::{Materialized, RefError, RefRecord, RetrieveResult};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS refs (
  ref_id TEXT PRIMARY KEY, original BLOB NOT NULL, byte_len INTEGER NOT NULL,
  token_estimate INTEGER NOT NULL, source_uuid TEXT NOT NULL, session_id TEXT NOT NULL,
  kind TEXT NOT NULL, created_at REAL NOT NULL, last_access_at REAL NOT NULL,
  access_count INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0);";

const PRAGMAS: &str =
    "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;";

/// The content-addressed reversible store.
///
/// `put` is the sole writer and `materialize` the sole Rust reader; both funnel
/// through one connection. Construct with [`RefStore::open`].
pub struct RefStore {
    connection: Connection,
}

impl RefStore {
    /// Open (creating if needed) the refs database at `db_path`.
    ///
    /// Applies the WAL PRAGMAs, authors the schema, and best-effort chmods the
    /// db file to `0600`.
    pub async fn open(db_path: impl AsRef<Path>) -> Result<RefStore, RefError> {
        let db_path = db_path.as_ref().to_owned();
        let connection = Connection::open(&db_path)
            .await
            .map_err(tokio_rusqlite::Error::from)?;
        connection
            .call(|conn| {
                conn.execute_batch(PRAGMAS)?;
                conn.execute_batch(SCHEMA)?;
                Ok(())
            })
            .await?;
        chmod_0600(&db_path);
        Ok(RefStore { connection })
    }

    /// Store `original`, content-addressed and idempotent.
    ///
    /// The sole writer. Two byte-identical puts collapse to one row via
    /// `ON CONFLICT(ref_id) DO NOTHING`; the returned record names that row.
    pub async fn put(
        &self,
        original: &[u8],
        source_uuid: &MessageId,
        session_id: &SessionId,
        kind: SegmentKind,
        now: f64,
    ) -> Result<RefRecord, RefError> {
        let record = RefRecord {
            ref_id: content_address(original),
            byte_len: original.len() as u64,
            token_estimate: estimate_chars_proxy(&String::from_utf8_lossy(original)),
            source_uuid: source_uuid.clone(),
            session_id: session_id.clone(),
            kind,
            created_at: now,
        };
        let row = (
            record.ref_id.as_str().to_owned(),
            original.to_owned(),
            record.byte_len,
            record.token_estimate.get(),
            record.source_uuid.as_str().to_owned(),
            record.session_id.as_str().to_owned(),
            record.kind.to_string(),
            record.created_at,
        );
        self.connection
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO refs
                       (ref_id, original, byte_len, token_estimate, source_uuid,
                        session_id, kind, created_at, last_access_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                     ON CONFLICT(ref_id) DO NOTHING",
                    params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7],
                )?;
                Ok(())
            })
            .await?;
        Ok(record)
    }

    /// Materialize the original for `ref_id`, bumping its access accounting.
    ///
    /// The sole Rust reader. Returns `None` on a miss. `now` stamps
    /// `last_access_at`, so the caller controls the LRU clock.
    pub async fn materialize(
        &self,
        ref_id: &RefId,
        now: f64,
    ) -> Result<Option<Materialized>, RefError> {
        let id = ref_id.clone();
        Ok(self
            .connection
            .call(move |conn| {
                conn.query_row(
                    "UPDATE refs SET access_count = access_count + 1, last_access_at = ?2
                     WHERE ref_id = ?1
                     RETURNING original, token_estimate, access_count",
                    params![id.as_str(), now],
                    |r| {
                        Ok(Materialized {
                            ref_id: id.clone(),
                            text: String::from_utf8_lossy(&r.get::<_, Vec<u8>>(0)?).into_owned(),
                            token_estimate: TokenCount(r.get::<_, u32>(1)?),
                            access_count: r.get::<_, u64>(2)?,
                        })
                    },
                )
                .optional()
            })
            .await?)
    }

    /// Retrieve `ref_id` *within the scope of `session_id`*, optionally
    /// BM25-searched-within by `query`.
    ///
    /// The scope is enforced in the same atomic statement that bumps the access
    /// accounting: a ref minted under another session never matches, so a
    /// cross-session request is an indistinguishable [`RetrieveResult::Miss`]
    /// and never bumps `access_count`. The caller renders the recovery hint on a
    /// miss rather than erroring.
    pub async fn retrieve(
        &self,
        ref_id: &RefId,
        session_id: &SessionId,
        query: Option<&str>,
        now: f64,
    ) -> Result<RetrieveResult, RefError> {
        let id = ref_id.clone();
        let session = session_id.as_str().to_owned();
        let hit: Option<(String, u64)> = self
            .connection
            .call(move |conn| {
                conn.query_row(
                    "UPDATE refs SET access_count = access_count + 1, last_access_at = ?3
                     WHERE ref_id = ?1 AND session_id = ?2
                     RETURNING original, access_count",
                    params![id.as_str(), session, now],
                    |r| {
                        Ok((
                            String::from_utf8_lossy(&r.get::<_, Vec<u8>>(0)?).into_owned(),
                            r.get::<_, u64>(1)?,
                        ))
                    },
                )
                .optional()
            })
            .await?;
        Ok(match hit {
            None => RetrieveResult::Miss,
            Some((text, access_count)) => RetrieveResult::Hit {
                text: match query {
                    Some(q) => bm25::search_within(&text, q),
                    None => text,
                },
                access_count,
            },
        })
    }

    /// Mark-and-sweep GC: evict eligible refs over a size cap, oldest-accessed
    /// first. Returns the number of rows deleted.
    ///
    /// A row is eligible iff its ref is NOT in `reachable`, is not pinned, and
    /// has aged past `grace_seconds`. Eligible rows are deleted oldest-first by
    /// `last_access_at` until the non-reachable live total would fall to
    /// `max_bytes`; grace-protected and reachable bytes are never evicted, so the
    /// true on-disk total can stay above the cap when young or live data dominates.
    /// INVARIANT: a ref in `reachable` is never deleted.
    pub async fn gc(
        &self,
        reachable: &HashSet<RefId>,
        grace_seconds: f64,
        max_bytes: u64,
        now: f64,
    ) -> Result<usize, RefError> {
        let reachable: HashSet<String> = reachable.iter().map(|r| r.as_str().to_owned()).collect();
        Ok(self
            .connection
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT ref_id, byte_len, created_at, last_access_at
                     FROM refs WHERE pinned = 0",
                )?;
                let candidates: Vec<(String, u64, f64, f64)> = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, u64>(1)?,
                            r.get::<_, f64>(2)?,
                            r.get::<_, f64>(3)?,
                        ))
                    })?
                    .collect::<Result<_, _>>()?;

                // Superset of what the scan below subtracts: every non-reachable
                // candidate, whereas victims are only the eligible (also past-grace)
                // subset — so `live_bytes >= sum(victims)` and the unchecked
                // `*remaining -= byte_len` can never underflow.
                let live_bytes: u64 = candidates
                    .iter()
                    .filter(|(id, ..)| !reachable.contains(id))
                    .map(|(.., byte_len, _, _)| byte_len)
                    .sum::<u64>();

                let mut eligible: Vec<&(String, u64, f64, f64)> = candidates
                    .iter()
                    .filter(|(id, _, created_at, _)| {
                        !reachable.contains(id) && now - created_at > grace_seconds
                    })
                    .collect();
                eligible.sort_by(|a, b| a.3.total_cmp(&b.3).then(a.0.cmp(&b.0)));

                let victims: Vec<&str> = eligible
                    .iter()
                    .scan(live_bytes, |remaining, (id, byte_len, _, _)| {
                        (*remaining > max_bytes).then(|| {
                            *remaining -= byte_len;
                            id.as_str()
                        })
                    })
                    .collect();

                for id in &victims {
                    conn.execute("DELETE FROM refs WHERE ref_id = ?1", [id])?;
                }
                Ok(victims.len())
            })
            .await?)
    }
}

fn chmod_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
