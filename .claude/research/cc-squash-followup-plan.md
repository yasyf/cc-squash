# cc-squash Follow-Up Plan

Created 2026-06-25, after landing the Layer 1-4 compaction-shortcut fixes on `main`
(commit `feat(proxy): squash tool output reversibly + configure/calibrate the engine`,
CI green). Two follow-ups, in priority order. F1 is a live-discovered P0 regression-class
bug; F2 is the `ccs-eval` substrate deferred from the main plan's Phase 5.

---

## F1 (P0) — The live `ccs run` squash engine is a no-op

### Context

The deterministic proof for the tool-output squash is strong (closed-loop integration
test produces the plan via real `stage_next` and applies it via real `intercept::run`;
292+ tests; CI-green incl. the two-process cold-start). But a **live** `ccs run -- claude -p`
tool-heavy session (reading 7 files incl. the ~50k-token build plan) revealed the engine
**never squashes anything end-to-end**:

- The relay works perfectly: 10× `POST /v1/messages`, all `decision=forward status=200`,
  intact SSE, no 401, context growing 99KB→346KB. Layer 1 / Exp C re-proven live, fail-open
  intact.
- **But: zero `L1 staged plan (shadow)` log lines** (logged at `info!`; the proxy filter is
  `info`, so this is not a logging artifact) and **`refs.db` has 0 rows** — L1 never reached
  `store.put`, and L2 never intercepted.

The session *was* recognized (`decision=forward` requires `inspect_for`'s
`state.sessions.contains_key` to be true — demux.rs:82), `econ` is built (`claude-sonnet-4-6`
is in `MODEL_ECONOMICS`), and `SessionEcon::new` inits `staging=false` / `intercept_enabled=true`
— so `forward_setup` *should* spawn `stage_next`. The unit and closed-loop tests can't catch
this because they call `stage_next`/`intercept::run` **directly**, mocking the Anthropic
boundary — they never exercise the relay's `forward_setup` wiring or the **real** summarizer
transport/auth.

### Most likely root cause

The off-path L1 task (`staging.rs::stage_next`) does not complete in the live flow. The
loop's `store.put` and the final `log_plan` both sit **after** the `decide()`/`fold()`
summarizer `await`s, so a hang or silent failure there produces exactly this signature (no
log, no refs). The summarizer POSTs to `state.upstream` with the captured session OAuth
(`capture_auth`, relay.rs:343) and the pinned `claude-sonnet-4-6`; the call has no observed
timeout, and it runs in a detached `tokio::spawn` whose panic would go to stderr (verify
whether that reaches `daemon.log`).

### Approach

1. **Instrumented reproduction** (don't guess): run the proxy with `RUST_LOG=ccs_proxy=debug`
   + `RUST_BACKTRACE=1`, capture proxy stdout **and** stderr, drive a minimal 1-file
   `ccs run -- claude -p` session, and determine whether `stage_next` spawns, hangs, or
   panics — and whether a detached-task panic surfaces in `daemon.log` at all. Add temporary
   `tracing::debug!` at `forward_setup` entry/exit, `stage_next` entry, and around each
   `decide`/`fold` await if needed.
2. **Fix per the evidence.** Likely: (a) add a wall-clock **timeout** to the off-path
   summarizer calls (a stalled call must never wedge L1 — it should fail-safe to Keep/prev and
   still `log_plan`); (b) confirm the standalone summarizer request carries the headers the
   first-party path needs (beta headers / `anthropic-version`) so the OAuth call actually
   succeeds; (c) ensure detached-task failures are logged (wrap `stage_next` so a panic/err is
   visible, not swallowed).
3. **Close the test gap.** Add a relay-level test that drives `serve()` through
   `forward_setup` → `stage_next` (summarizer mocked at the transport, but the **real**
   `forward_setup` wiring + a real `RefStore`), asserting `refs.db` gains a row and a staged
   plan is committed after turn 1 — the gap the direct-call tests structurally miss.
4. **Re-verify live**: a `ccs run` session shows `L1 staged plan` logs, `refs.db` rows, and on
   a later turn an L2 squash (or a logged economic `Hold`), with realized `cache_creation`
   tracking the predicted bust.

### Potential pitfalls

- Detached `tokio::spawn` swallows panics — instrument before assuming "didn't spawn."
- The summarizer auth may work for the main session but not for a raw standalone POST; this is
  an auth/headers issue, not necessarily a hang.
- Keep the cardinal invariant: a broken summarizer must degrade L1 to no-squash (fail-safe),
  never wedge the hot path or the relay.

### Workflow Plan

| Phase | Shape | Agents | Verification |
|---|---|---|---|
| Repro | pipeline | 1 subagent runs the instrumented live repro, reports the exact failure point | the failing await/spawn is identified with evidence from logs |
| Fix | pipeline → verify | 1 impl (timeout + auth + visibility) → 1 adversarial verify | `refs.db` gains rows in a live run; fail-safe holds under a stalled summarizer |
| Test gap | pipeline | 1 subagent adds the relay-level `serve`→`forward_setup`→`stage_next` test | the new test fails on the pre-fix code and passes after |

### Verification

Live `ccs run` session shows staging logs + refs.db rows + an L2 squash/Hold; the new
relay-level test exercises the real `forward_setup` wiring; `cargo test --all` + clippy green.

---

## F2 — `ccs-eval` substrate + live A/B (deferred Phase 5)

### Context

The engine's retention/quality claims are not yet code-backed; only `ccs shadow` capture
exists. This is the "both, phased" follow-on chosen during the main plan. **Do F1 first** —
there is no point measuring quality on an engine that doesn't activate live.

### Approach (per the main build plan §7 "Parallel substrate" + §10)

1. **Shadow-log schema** — append-only, content-addressed: original request + computed plan +
   would-be rewrite + actual upstream `usage`/response + correlation keys +
   `compact_boundary`/`compactMetadata` markers. (`ccs shadow on` capture already exists.)
2. **`ccs replay <log-dir>`** — reconstruct paired fixtures split at genuine
   `compact_boundary`; run the 4-rung ladder (No-Compaction oracle / cc-squash / CC-builtin /
   FIFO floor); the zero-LLM retention **precision + recall + F1** gate; paired stats
   (McNemar/Wilcoxon, session-level cluster bootstrap, Holm/BH).
3. **Tier-1 CI gate** — zero-LLM salience-needle + adversarial-survival, blocks every PR.
4. **`PREREGISTRATION.md`** — fix the metrics before any headline number.
5. **Live A/B** (Exp D + AB-oracle) — realized `cache_creation` vs prediction; cc-squash's
   quality gap < CC-builtin's at materially lower cache cost.

New crate `ccs-eval` (+ `statrs`), per the build plan's crate roster (lines 119, 364, 489-490).

### Workflow Plan

| Phase | Shape | Agents | Verification |
|---|---|---|---|
| Schema + replay | pipeline | 1 per stage (schema → replay → retention gate) | replay reconstructs fixtures; precision **and** recall reported |
| Tier-1 gate | pipeline | 1 subagent wires the zero-LLM CI gate | the gate blocks a seeded salience-loss regression |
| Live A/B | pipeline | 1 subagent runs the paired A/B once F1 is fixed | objective task-success scored, paired stats, pre-registered |

### Verification

`ccs replay` reports precision/recall/F1; the Tier-1 gate runs in CI; the live A/B yields a
pre-registered headline number.

---

## Out of scope (unchanged from the main plan)

Layer 5 (on-disk transcript durability mirror: `ccs-transcript` + `cc-transcript-core` +
hooks sidecar) and Layer 6 (FUSE) remain unbuilt future phases, not shortcuts in done work.
