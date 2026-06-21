# cc-squash Architecture — Live, Continuous Cache-Economics Optimization

**Status:** Architecture (design + enumerated feasibility experiments). **This document SUPERSEDES §4 and §5 of `compaction-deep-dive.md`**, which assumed a one-shot compaction-*boundary* hook. The thesis here is different: cc-squash is an always-on optimizer that runs throughout a session, not a single hook that fires when Claude Code (CC) decides to compact. This document is self-contained; `compaction-deep-dive.md` remains valid for §0–§3 (how CC compaction works, the complaint clusters, the prior-art taxonomy) and is left untouched.

**Provenance tags used throughout:** `[PRIMARY]` = verified against a primary source (the carved CC binary `2.1.183/bundle.js`, the `platform.claude.com` docs, the `headroom` clone, or the local machine). `[INFERRED]` = a design conclusion drawn from primary evidence but not itself directly observed. `[DEFERRED-EXP]` = a claim whose confirmation is an enumerated experiment we are explicitly *not* running this turn (no live proxy, no real CC sessions, no FUSE mounting, no API spend).

Local anchors, all verified to exist:
- Carved CC binary: `/Users/yasyf/Code/cc-squash/.claude/research/extracted/2.1.183/bundle.js` `[PRIMARY]`
- headroom clone (prior art): `/Users/yasyf/Code/cc-squash/.claude/research/extracted/headroom/` `[PRIMARY]`
- Prior research being superseded (§4/§5): `/Users/yasyf/Code/cc-squash/.claude/research/compaction-deep-dive.md`
- Real transcripts: `/Users/yasyf/.cc-pool/accounts/acct-05/projects/-Users-yasyf-Code-cc-squash/*.jsonl` `[PRIMARY]`
- Target machine: macOS **26.4.1 (Tahoe)**, Darwin 25.4.0 `[PRIMARY — sw_vers]` → macFUSE FSKit userspace path, no kext.

---

## 0. Thesis, and how it differs from the boundary-hook design

cc-squash is **not** a compaction-boundary hook. It is a **live, continuous cache-economics optimizer**: a streaming proxy that sits on every `/v1/messages` request CC sends and, at every opportunity, scores the tradeoff between **keeping** a segment of context resident in the model's prompt cache versus **evicting/rewriting** it to reclaim tokens, headroom, and attention quality — and squashes (removes / rewrites / replaces-with-a-structured-reference) at **any position** in the conversation whenever the score says it is worth it. The superseded §4/§5 design assumed cc-squash only acts *at* CC's whole-history compaction boundary via `PreCompact`; this design relegates that boundary to a coarse fallback tier and makes the proxy the continuous engine. The one term that distinguishes cc-squash from all prior art (headroom, bioqa) is that **every squash must price in the cost of busting the Anthropic prompt cache** — a one-time cost that is a function of *edit position* (tail edits cheap, head edits expensive) and *cache warmth* (a squash during a cold/idle window is free) — weighed against recurring future savings plus quality gains, with squashes made **reversible** via a content-addressed store reachable through both an MCP `retrieve(ref_id)` tool and a FUSE filesystem path.

---

## 1. The cache-economics model

*(From the `cache_economics` module; cites the cache-economics investigation and the carved binary. All constants verified.)*

### 1.1 How prefix caching works, and the one rule that governs everything

Anthropic prompt caching matches on **exact byte prefixes**: the cache key is a cumulative hash of the rendered prompt from the very start up to and including the block carrying `cache_control`. A hit requires a 100% identical prefix; a single differing byte at or before the breakpoint produces a different hash and misses. `[PRIMARY — platform.claude.com prompt-caching]` Prefixes build in hierarchy order **tools → system → messages**, and the invalidation rule is the central constraint:

> "Changes at each level invalidate that level and all subsequent levels." `[PRIMARY]`

**Editing or removing any token invalidates the cached prefix from that point to the end of the prompt.** The entire suffix after the edit must be reprocessed, and to be cached again it must be re-written. This is why *position* is the dominant cost term for cc-squash.

### 1.2 Confirmed constants

| Fact | Value | Source |
|---|---|---|
| Cache-READ multiplier `r` | **0.1×** base input | pricing table `[PRIMARY]` |
| Cache-WRITE 5-min `w₅` | **1.25×** | pricing table `[PRIMARY]` |
| Cache-WRITE 1-hour `w₁ₕ` | **2.0×** | pricing table `[PRIMARY]` |
| **CC default write mult** | **2.0×** (1h) in REPL `auto_mode` | binary `function eio(){return P4e("auto_mode")?"1h":void 0}` — verified verbatim `[PRIMARY]` |
| Opus 4.8 base input | **$5/MTok** ($0.50 read / $6.25 5m-write / $10 1h-write / $25 out) | pricing `[PRIMARY]` |
| Min cacheable prefix (Opus 4.8 / Sonnet 4.x) | **1,024 tok** (Opus 4.5/4.6 & Haiku 4.5 = 4,096; Opus 4.7 = 2,048) | minimum-tokens table `[PRIMARY]` |
| Max explicit breakpoints / request | **4** (across tools+system+messages combined) | prompt-caching doc `[PRIMARY]` |
| Lookback window per breakpoint | **20 positions** | prompt-caching doc `[PRIMARY]` |
| TTL refresh | sliding, refreshed-on-use at **no cost** → measured from *last access*, not write | "refreshed for no additional cost each time the cached content is used" `[PRIMARY]` |
| Break-even | 5m cache pays off after **1** read; 1h after **2** reads | pricing `[PRIMARY]` |
| Below-minimum detection | both `cache_creation_input_tokens` and `cache_read_input_tokens == 0` ⇒ uncached, silent | prompt-caching doc `[PRIMARY]` |
| CC cache_control placement | rolling per-turn on last block (`i===o.length-1?n?{cache_control:Ete({ttl:r})}:{}`) + a system-block breakpoint gated by `systemPromptChanged` | binary — verified verbatim `[PRIMARY]` |
| Ground-truth token count | `count_tokens` endpoint available | binary `[PRIMARY]` |

The model treats `(base, write_mult, read_mult, min_floor)` as a **model-keyed frozen table** (`MODEL_ECONOMICS`), defaulting to Opus 4.8 / 1h-write because that is CC's resolved REPL config. `write_mult` is resolved per session from the observed env (`FORCE_PROMPT_CACHING_5M` ⇒ 1.25, else 2.0) **and corrected from observed billing** (§1.5).

### 1.3 The squash cost/benefit formula

For a candidate squash, with `b` = base $/tok, `w` = write mult (2.0 CC default), `r` = read mult (0.1):

- `p` — token offset of the **earliest** edited block (the position lever).
- `S_after` — tokens from `p` to end of prompt (the invalidated suffix).
- `T_removed` — **net** tokens removed = `original_tokens − (summary_tokens + pointer_tokens)`. This **must net out the resident pointer**, since the summary+ref stays cached and is re-read at `r` every turn.
- `N` — expected remaining turns this prefix stays live (§1.6).
- `cold` — `True` if the cache is past its live TTL.

**One-time bust cost** (charged on the *next* request after the squash):

```
bust = 0                                   if cold       # suffix re-billed anyway → squash is FREE
bust = S_after · b · (w − r)               otherwise     # suffix downgrades from a 0.1× read to a w-write
       (+ new_uncached_tokens · b · 1.0)                 # any genuinely-new bytes the rewrite introduces
```

The `(w − r)` term is the marginal penalty of turning a would-be cache READ into a cache WRITE. With CC's 1h auto_mode: `(2.0 − 0.1) = 1.9`.

**Recurring per-turn saving** (every future turn reusing the smaller prefix):

```
save_per_turn = T_removed · b · r
saving_total  = N · T_removed · b · r
```

**Squash iff** `saving_total + Q > bust`, where `Q` ≥ 0 is headroom/attention-quality value (raises priority of squashes that reclaim headroom near CC's auto-compact line or remove low-salience noise that degrades attention). Solving for the break-even horizon (cold=False):

```
N* = S_after · (w − r) / (T_removed · r)        # turns to break even
   = 19 · S_after / T_removed                    # with w=2.0, r=0.1   →  (w−r)/r = 19
```

(Under `FORCE_PROMPT_CACHING_5M` the coefficient is `(1.25 − 0.1)/0.1 = 11.5`, a lower bar — see §7.1.)

**The three regimes the scorer encodes directly** `[INFERRED from PRIMARY constants]`:
- **Tail squash** (`S_after` small): `N*` tiny → almost always worth it; flush immediately.
- **Cold-cache squash** (`bust=0`): unconditionally worth it for any `T_removed > 0`.
- **Head squash while warm** (`S_after` ≈ whole window): needs `T_removed` large *and* `N` large → usually **hold** unless `Q` is high or a bust is already forced.

### 1.4 Position and TTL effects → three scheduling levers

1. **Position.** `S_after` grows as `p` moves toward the head; strongly prefer the latest viable edit position.
2. **Batching.** Because invalidation runs from `min(p_i)` to end **regardless of how many blocks change**, K squashes at `p₁<…<p_K` cost the **same** suffix-reprocess as ONE squash at `min(p_i)`. Accumulate pending squashes and flush them as a single suffix rewrite; low-value tail squashes ride along for free on a bust a head squash already forces, or are deferred.
3. **Cold-cache timing.** The TTL is a *sliding* window refreshed on use, so it is measured from `last_request_ts`. Define:

```
P_alive(idle, ttl) = clamp(1 − idle/ttl, 0, 1)      # linear decay; 0 ⇒ certainly cold
cold = (idle ≥ ttl)                                  # hard gate: bust term collapses to 0
```

`P_alive` softens the bust for the uncertain middle (`bust_expected = P_alive · bust`), so a head squash becomes progressively cheaper as the session idles toward TTL (3600 s in auto_mode 1h; 300 s under `FORCE_PROMPT_CACHING_5M`). **This is the idle-gap signal headroom *defines but leaves gated* — `prefix_tracker.py:215-234` exposes the idle property and a `#856 P3b` comment says it "feeds this to the net-cost gate as an idle signal," but the only consumer is `content_router.py`'s `n_override` path, which is itself behind an env flag (`HEADROOM_NET_COST_P_ALIVE`) and otherwise defaults `n=1.0` (no decay).** `[PRIMARY — headroom prefix_tracker.py:215-234 + content_router.py n_override]` cc-squash wires it as a first-class, always-on lever. Two further free-bust windows: a **model switch** (Opus↔Sonnet busts the whole cache) and a **CC native-compaction boundary** (whole-history rewrite already busts the entire prefix) — flush all pending squashes into either.

### 1.5 Ground truth over estimates

Per-response `cache_creation_input_tokens` (write) and `cache_read_input_tokens` (read) are read back every turn (headroom `prefix_tracker.update_from_response` is the borrowable skeleton, reimplemented async). cc-squash uses them to (a) measure the **actual** cached-prefix length, (b) calibrate its `count_tokens`/char estimates, (c) detect a squash that **over-busted** (realized write > predicted), and (d) **infer the resolved `w`** (1.25 vs 2.0) from observed billing rather than trusting the env guess. A sudden `cache_creation==0 AND cache_read==0` is an alarm that caching silently disengaged (§1.7) and triggers auto-revert.

### 1.6 Estimating `N`

`N` is the soft, uncertain input and enters only the *warm* branch. Estimate from session history (EWMA of turns-per-session for this project, floored). The scorer is **asymmetric** about `N` uncertainty: it requires break-even at a discounted `N_p25` (25th-percentile remaining turns) to clear, so a squash fires only when it pays off even in a short remaining session. Cold-cache and forced-bust squashes ignore `N` entirely (they are free).

### 1.7 Breakpoint strategy and guardrails

cc-squash sits in the proxy and sees CC's *already-assembled* request with CC's own breakpoints. The economics module **prescribes** placement; the proxy **applies** it:

- **Place the breakpoint at the END of cc-squash's stable rewritten prefix** (the summary+pointer block) — never on volatile content, or every turn misses. (bioqa `CachePass` discipline; headroom PR-B7 "the system prompt is the cache hot zone.")
- **Respect the 4-breakpoint cap and 20-position lookback.** The stable prefix must stay within 20 blocks of its breakpoint to be re-found after a turn grows. Over budget ⇒ drop the **earliest** hints first (bioqa `cap_cache_hints(4)`).
- **Min-floor guard.** Refuse any squash whose post-edit cacheable prefix would fall **below the model floor** (1,024 tok Opus 4.8) — below it caching silently disengages and every turn pays `1.0×`, a ~10× recurring blowup vs `0.1×`. Verify post-flush via the next response's usage fields.
- **Sticky pointer/tool block** (headroom PR-B7 "sticky-on"): once a retrieve tool or pointer block exists, keep it byte-stable every turn — flipping it on/off itself busts the cache.

### 1.8 What to hold (the negative space prior art under-encodes)

The scorer must be willing to return `Hold`: deep-prefix segment + warm cache + few turns left (`bust > saving_total + Q`); sub-floor risk; imminent model switch / native compaction (await the free bust); a ref whose `access_count` keeps climbing (re-injecting it every turn defeats the squash).

---

## 2. The continuous scoring & eviction policy

*(From the `cc_squash.policy` module; cites bioqa with file:line.)*

### 2.1 The always-on controller — three nested loops

cc-squash mirrors bioqa's two-tier split (proactive per-context Tier-1 + reactive whole-request Tier-2) plus the cold-cache timing axis, but **never blocks the live turn**:

| Loop | Trigger | Shape | bioqa analog |
|---|---|---|---|
| **L0 — observe** | every `/v1/messages` egress | synchronous, **read-only**: segment the body, refresh `CacheState` from the prior response's `usage`, compute the cheap pressure estimate, recompute breakpoints | `update_from_response` + `CachePass.boundaries` |
| **L1 — score & schedule** | every egress, *off the critical path* | async (anyio TaskGroup): score each segment, run `ContentDecision` for top candidates, **stage** a `SquashPlan` whose actions land on a *future* request | `enqueue_compactions` (kicks off at end-of-render, result lands next render) `[PRIMARY — context.py:1077]` |
| **L2 — flush** | when the staged plan's NPV clears the bar **at flush time** | synchronous on egress: apply staged actions to *this* body, then bust once | `default_compact`, but incremental + reversible |

The L1/L2 split is the whole point: an LLM rewrite on the hot path stalls the user's turn, and the *right* moment to flush a head-of-prompt squash is not "now" but the cheapest-bust window (tail edit, or cache cold past TTL). bioqa establishes exactly this discipline — summarization runs at end-of-render and is consumed on a *later* render, never the current one (`enqueue_compaction → _schedule_sync → defer_attribute_update`, `[PRIMARY — bioqa context.py:956-994]`).

**Pressure signal.** L1 aggressiveness scales with bioqa's `OVER_TOKEN_BUDGET` analog: a cheap running estimate vs a *soft* fraction-of-window threshold, evaluated every egress, sitting **well below** CC's hard auto-compact line (`effective_window − 13000` `[PRIMARY — prior research]`). bioqa hints `OVER_TOKEN_BUDGET` when just-added tokens exceed `max_tokens//2` where `max_tokens = 0.8·window` `[PRIMARY — bioqa llm.py:117-121, 223-228, 418-421]`. Below the soft threshold the controller idles (only free cold-cache squashes flush); above it, it tightens the fresh window by one turn and lowers the NPV bar.

### 2.2 Segmentation — deriving scoreable units from the wire body

CC's body is a flat `messages: list[{role, content}]`; bioqa scores over `LLMContextGeneration` checkpoints, which CC lacks, so we **derive** them. A `Segment` is the **largest contiguous run that can be independently rewritten without breaking API validity** — a `tool_use`/`tool_result` pair is one segment (never split), a whole assistant turn + its tool results is the unit. Segments carry their **byte offset** in the rendered prefix (position term) and their **generation index** (user-turn ordinal). The last segment is always pinned verbatim (bioqa's `contexts[-1]` rule — the volatile current turn stays outside any cached prefix).

### 2.3 Salience scoring — eligibility = freshness × salience-pin

A segment is a *candidate* iff it is **stale** (before the fresh boundary `gen[-2]`, tightened to `gen[-1]` under pressure — bioqa `RenderPass.fresh`, `[PRIMARY — bioqa render/base.py:48-58]`) **and** not salience-pinned **and** not the current turn.

```
candidate(seg) = is_stale(seg, fresh_boundary) and not seg.pinned and not seg.is_current
```

`pinned` is true when the segment carries a **live** (`superseded_by is None`) `Constraint` / `Decision` / `InFlightWork` record from the prior-research `WorkingState` salience model. **Constraints are preserved verbatim** (never even summarized — CC's own Variant-B prompt says "security constraints verbatim"). This is where the salience-typed `WorkingState` plugs directly into the live controller. **Fail-safe rule:** when salience is uncertain for a segment, treat it as pinned (never evict) — this directly defends against the C1 context-loss failure cc-squash exists to prevent.

### 2.4 The cache-aware NPV score (the novelty over bioqa)

Every bioqa mechanism answers *"is this segment stale/redundant/summarizable?"*. cc-squash answers the same **and then** *"does the recurring saving beat the one-time bust from this position, given current cache warmth?"*. bioqa owns its array and re-renders for free → it has **no bust term**. We keep headroom's cached-prefix tracking from `usage.cache_read_input_tokens + cache_creation_input_tokens` and its provider economics table, and **replace headroom's single position-agnostic test** (`should_force_compress`: `savings_fraction > read_discount`, `[PRIMARY — headroom prefix_tracker.py]`) **with the position-aware NPV of §1.3**:

```
NPV(N) = N · save_per_turn + Q − bust          # bust priced by EDIT POSITION; 0 when cold
flush when NPV(N̂) > 0 at flush time
```

### 2.5 The lossy ladder, made cache-aware

Once a segment is *selected*, the **strategy** is bioqa's `ContentDecision` 4-way (`[PRIMARY — bioqa util/agents/context.py:22-214]`), priority `truncate > summarize > reversible-ref > keep`, **but cache-cost-folded** — a `keep` is sometimes correct purely because the segment sits deep in the cached prefix and `bust > saving` even where bioqa would summarize. bioqa's automated-`compress` rung (LLMLingua-style; dropped per prior research `S5-F4`) is **replaced by the reversible-ref rung** — the only lossless rung, because the original is retrievable.

```
Strategy ladder (least → most lossy, each gated by NPV against the bust):
  Keep            verbatim; chosen when pinned, or bust > saving (deep prefix), or <256 chars
  Truncate        keep inclusive line-ranges (ContentDecision ranges_to_keep)      — cheapest lossy
  Summarize       LLM-condensed text, 30–50% length, ≤2048 tok, rejected if longer  — bioqa summarize
  ReversibleRef   summary + structured pointer; ORIGINAL stored, retrievable        — lossless-by-retrieval  ← cc-squash default lossy
  Drop            no pointer (irreversible)                                          — FALLBACK tier only
```

Dispatch is **pattern-matched** (mirrors bioqa's `match decision:`). **Pre-gates ported verbatim from bioqa:** content `< 256` chars → `Keep` (LLM not even called); a summary that would exceed the original → `Keep` (`result_longer_than_input`). **Tool-pair integrity:** `Truncate`/`Drop` never severs a `tool_use`/`tool_result` pair (bioqa `droppable_pair_ids`).

**ReversibleRef is the default lossy strategy** (where bioqa defaults to summarize) because it alone satisfies the reversibility contract. The resident pointer costs `T_pointer · b · r` per turn, **netted out of `save_per_turn`**.

**Tier-2 fallback (`Drop`).** When the *real* outgoing request still overflows the hard target (`window − max_output − 1024`, bioqa floor 256, `[PRIMARY — bioqa requester.py:216-219]`), run bioqa's `default_compact` ladder (`strip_reasoning → drop_tool_pairs → drop_oldest`, `[PRIMARY — bioqa compaction.py:26-90]`) — but route droppable content through ReversibleRef first, so even the fallback stays recoverable. **Resolved open question** ("must every squash be reversible?"): **yes**, except the terminal `drop_oldest` rung, which only sheds content already stored as a ref.

### 2.6 What the policy explicitly does NOT do

It does not call the LLM (delegates `ContentDecision` to an agent module) or write the store (delegates to `refs`) — it only *decides*. It does not reorder turns (bioqa `OrderingPass` reorders only data runs, `[PRIMARY — bioqa ordering.py:14-37]`; mid-session a reorder busts the cache for no structural gain → boundary-only / skip). It does not run the proxy or hooks.

---

## 3. The reversible-reference subsystem

*(From the `cc_squash.refs` module; cites headroom and the FUSE/`fusekit` investigation.)*

### 3.0 The "fusekit" finding (read first)

**The library the user named "fusekit" does not exist as an async-native FUSE toolkit.** `[PRIMARY — WebSearch + WebFetch]` Literal matches are two dead C++ Google-Code exports named `fusekit` and an unrelated PyTorch ML-testbench named `fusekit` on PyPI. **Treat "fusekit" as the user's working label for "the FUSE mounting layer"** and substitute the right binding per platform (§3.4). This should be confirmed with the user before locking the dependency.

### 3.1 Boundaries

```
squash engine (proxy egress)                  retrieval (proxy ingress + out-of-band)
        │ store.put(original, src) -> RefId            ▲ retrieve(ref_id) / Read(path)
        ▼                                              │
  ┌──────────────────────────── cc_squash.refs ───────────────────────────┐
  │  RefStore  (aiosqlite, content-addressed, durable, project-scoped)      │
  │     ├─ Placeholder.render()  → text block injected into messages[]      │
  │     ├─ RetrieveTool          → MCP tool cc_squash_retrieve              │
  │     └─ RefFuse  (FuseBackend Protocol: FusepyBackend | Pyfuse3Backend)  │
  │            → <mount>/refs/<ref_id>.txt  (read-only, lazy)               │
  └────────────────────────────────────────────────────────────────────────┘
```

`RefStore.put` is the **single writer**; `RefStore.materialize` is the **single reader** — the MCP tool handler and the FUSE `read()` callback both funnel through it (repo's one-persistence-codepath rule).

### 3.2 Content-addressed store

`RefId = NewType("RefId", str)` holding `"sha256:<64-hex>"` of the **original UTF-8 bytes**. **Full 64-hex, not headroom's truncated 24-hex marker** (`[PRIMARY — headroom tool_injection.py: "24-hex-char hash (SHA-256, 96 bits)"]`): headroom truncates because *its* store is in-memory/ephemeral (collision risk resets on restart); cc-squash's store is **durable and cross-session**, so a truncation collision would silently return the *wrong* original from a *different* session — a correctness bug, not a cache miss. Content-addressing gives **free dedup** (bioqa's `dedupe_key` over the payload hash, `[PRIMARY — bioqa deduplication.py:70-75]`, generalized from exact-dedup to lossy-squash).

Backed by **aiosqlite** (async-native per STYLEGUIDE; headroom uses blocking `sqlite3`, reimplemented here on the same schema and hygiene). One table, project-scoped DB beside the transcripts. PRAGMAs ported verbatim from headroom's hard-won config (`[PRIMARY — headroom sqlite backend]`): `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`, and **chmod `0o600`** on the db + `-wal` + `-shm` (originals contain file contents and command output).

`RefRecord` is **frozen+slots with NO blob and NO mutable counters** — it is the immutable identity+shape (`ref_id, byte_len, token_estimate, source_uuid, session_id, kind, created_at`). Mutable accounting (`last_access_at, access_count, pinned`) lives in **SQLite columns**, so a frozen record never drifts from what was stored, and the lazy-stat path (FUSE `getattr` reads `byte_len` without loading the blob) falls out structurally. `token_estimate` is computed once at `put()` with a Claude-tokenizer-aware estimator (not a GPT char-proxy) because the scorer needs it every turn but the blob must not be loaded every turn; it feeds the bust math (`T_removed`/`S_after`) and is calibrated against the usage fields the proxy reads back.

### 3.3 Placeholder — the in-context reversible handle

A **plain `{"type":"text","text":…}` content block** (survives `.jsonl` round-trips; user/assistant entries nest `message.content` as a string or block array, `[PRIMARY — transcript head]`). Self-describing, both affordances inlined so the model needs no tool docs and a single ingress regex re-finds live refs (bioqa's `<context_ref>` pattern, `[PRIMARY — bioqa deduplication.py:35-47]`, fused with headroom's hash marker):

```
[cc-squash: squashed segment · ref=sha256:ab12…ef90 · ~2050 tokens · 8210 bytes]
<one-paragraph salience-preserving summary of what was here>
Pull the full original if you need it:
  • retrieve("sha256:ab12…ef90")                       ← MCP tool, works everywhere
  • Read(".cc-squash/refs/sha256-ab12…ef90.txt")       ← if the FUSE mount is up
```

`REF_MARKER = re.compile(r"ref=(sha256:[0-9a-f]{64})")` re-finds live refs for sticky-on (§3.5) and GC reachability (§3.6). The FUSE filename swaps `:`→`-` (legal-but-awkward in shells), advertised in the placeholder so the model never transforms it.

### 3.4 Library selection (platform-split behind one Protocol)

- **macOS (the user's target):** `fusepy`/`refuse` over **macFUSE** — the only binding supporting macFUSE. **Synchronous** (subclass `fuse.Operations`, mount `FUSE(ops, mountpoint, foreground=True, ro=True)`). The blocking mount loop is the **single sanctioned `to_thread` exception in the whole codebase** (`anyio.to_thread.run_sync` to host the loop; the `read()` callback hops back with `anyio.from_thread.run` to hit aiosqlite). Documented exception to STYLEGUIDE § Async, justified because **no async-native macFUSE binding exists** — the rule's premise (an async driver exists) does not hold.
- **Linux (CI/containers):** `pyfuse3`, async-native (Trio; anyio runs a trio backend). **Not available on macOS** (no FUSE 3; pyfuse3 issues #19/#29/#32). `[PRIMARY — WebSearch]`

**macOS version friction:** target is **Tahoe 26.4.1** `[PRIMARY — sw_vers]`, where macFUSE's **FSKit backend runs fully in user space — no kext, no recovery-mode reboot** (the frictionless path). On Sequoia 15 and earlier the kext approval gate makes FUSE opt-in. Detect OS at startup. **macFUSE itself is currently ABSENT on this machine** `[PRIMARY — /Library/Filesystems/macfuse.fs missing]`, so first run must detect-and-degrade, never assume the mount.

### 3.5 Dual retrieval surface over one store

**(a) MCP tool `cc_squash_retrieve` (load-bearing, FUSE-independent).** Mirrors headroom's `headroom_retrieve` contract (`[PRIMARY — headroom tool_injection.py]`) with cc-squash's id form; `inputSchema` takes `ref_id` (required) and optional `query` (BM25 search-within so a 50k-token original can be partially retrieved — port headroom's `BM25Scorer`, `[PRIMARY — headroom compression_store.py]`). On a miss returns the **recovery hint** (headroom's `CCR_MISS_MESSAGE`, `[PRIMARY — compression_store.py]`): *"original no longer stored — if it was a file Read, re-read the file; if command output, re-run it."* **This is the primary surface because it needs no mount, no macFUSE, no kext, no sandbox grant** — the subsystem must be fully functional on `retrieve()` alone.

**(b) FUSE path `Read("<mount>/refs/<ref_id>.txt")` (ergonomic, opportunistic).** A read-only virtual tree; `read()` calls the same `materialize`; `getattr()` reads `byte_len` from `RefRecord` **without loading the blob** (lazy stat — listing and the initial `stat` are free; only `read()` charges). Behind `FuseBackend` Protocol with `FusepyBackend`/`Pyfuse3Backend` selected at startup. **Mount at `<project>/.cc-squash/refs`** (inside the project dir, `.gitignore`d): CC's Seatbelt profile already grants read inside the project, and the FUSE-investigation's central finding is that sandbox access is **path-governed, not FUSE-governed** `[PRIMARY/corroborated]` — a mount under an allowed path reads; outside yields "Operation not permitted."

**Degradation contract (hard requirement):** FUSE up → both surfaces live, placeholder advertises both lines. FUSE absent → MCP `retrieve()` only, placeholder **omits** the `Read(...)` line (never advertise a path that will 404 — that teaches the model a dead affordance and wastes a turn). The placeholder renderer takes `fuse_up: bool`.

### 3.6 Lazy materialization, cache interaction, and GC

**Lazy materialization is structural.** Squashing writes only summary+pointer into the stream and the blob into the store; original tokens are charged ONLY on actual `retrieve()`/`Read`, re-entering as a **fresh tail-position block** (`S_after ≈ 0`, so it busts nothing behind it — costs ~`T_removed · b · 1.0` once, then re-caches). `access_count` flows back to the **scorer** as the anti-thrash signal headroom's `record_access` tracks but never feeds into a decision (`[PRIMARY — headroom compression_store.py]`) — a ref pulled repeatedly means "stop squashing this segment."

**Sticky-on / hot-zone discipline** (headroom PR-B7, ported verbatim, `[PRIMARY — tool_injection.py]`): once any ref exists, `cc_squash_retrieve` stays in `body["tools"]` on **every** subsequent request (flipping the tool list busts the whole cache via the tools→system→messages hierarchy); placeholders live **only in the messages tail, never in the system hot zone**.

**GC — durable but bounded.** The invariant is a **correctness rule, not hygiene:** *never delete a blob whose ref still appears in any on-disk transcript* (deleting a referenced ref makes a past placeholder unrecoverable mid-task — the exact failure the subsystem prevents). Mark-and-sweep: (1) scan live `*.jsonl` + the in-flight stream with `REF_MARKER` for reachable ref_ids; (2) a row is eligible only if `ref_id ∉ reachable AND not pinned AND now − created_at > grace_window`; (3) LRU-evict eligible rows by `last_access_at` under a size/age cap. Content-hash keys make GC **safe to run concurrently with squashing** (`put` is idempotent on content; mark re-reads transcripts each pass). Runs off the critical path. An optional `pin` (MCP `cc_squash_pin`) is added only on demonstrated need (STYLEGUIDE § API Design).

**Cross-session persistence.** The DB (and, when up, the mount) are project-keyed and on disk, so a placeholder written this session is resolvable next session, and refs survive `--resume`, `SessionStart` re-injection, and CC's native compaction. The store is the **durable layer beneath** the volatile live-message mechanism — exactly the resume/post-compact niche the mechanism investigation scoped transcript-rewrite into.

### 3.7 Why this is a strict superset of headroom

| Concern | headroom | cc-squash `refs` |
|---|---|---|
| Reversibility | TTL-bounded (30 min default), original silently expires `[PRIMARY — compression_store.py]` | **Durable**, content-addressed; original only leaves on GC of an *unreferenced* ref |
| ref id | truncated 24-hex SHA-256 marker | full 64-hex SHA-256 (durable cross-session ⇒ no collision) |
| Retrieval surface | MCP tool + HTTP only; **no FUSE** (`rg` zero matches) | **MCP tool + FUSE path** over one store |
| Storage I/O | blocking `sqlite3` + lock | **aiosqlite** (async-native) |
| Access-count feedback | tracked, never used | **fed back to the scorer** (anti-thrash) |
| Scope | reactive tool-output compression only | any segment the proxy evicts |

---

## 4. The interception mechanism

*(From the `mechanism-and-experiments` module; supersedes §4/§5 of compaction-deep-dive.md. Every claim re-verified against the carved binary. A dedicated deep-dive on force-reload and compaction-replace — the binary evidence behind §4.4 and §4.6 — lives in `transcript-reload-feasibility.md`.)*

### 4.1 The requirement and the decisive evidence chain

cc-squash needs **write access to the live message array CC sends to `/v1/messages`**, with three non-negotiables: (1) fine-grained any-position rewrite, (2) continuous operation, (3) `cache_control` control. Evidence chain, every token confirmed with `rg -a` over `bundle.js`:

| # | Claim | Verified token `[PRIMARY]` |
|---|---|---|
| E1 | CC posts via the standard `@anthropic-ai` SDK | `this._client.post("/v1/messages?beta=true",{body:o,…})` |
| E2 | `ANTHROPIC_BASE_URL` is the SDK base URL; default upstream `api.anthropic.com` | `baseURL:e=sT("ANTHROPIC_BASE_URL")` ; `baseURL:e\|\|"https://api.anthropic.com"` |
| E3 | Requests are streaming binary (proxy must terminate + re-emit SSE) | `stream:!0,__binaryResponse:!0` |
| E4 | First-party gate affects auth/beta-header attachment for custom base URLs | `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` (**7 occurrences**) |
| E5 | `cache_control` is computed **in-process at request-build time** | `i===o.length-1?n?{cache_control:Ete({ttl:r})}:{}` |
| E6 | CC uses the **1-hour** cache TTL in `auto_mode` ⇒ default bust write-mult **2.0×** | `function eio(){return P4e("auto_mode")?"1h":void 0}` |
| E7 | **Live requests are NOT built from the `.jsonl`** — loader prefers in-memory `sessionStore`; file-parse gated on resume/continue | `function Ppm(e,t){if(t?.sessionStore)return Vpm(…);return e0l(…)}` ; `(t?.resume\|\|t?.continue)&&t?.sessionStore` |
| E8 | The `.jsonl` is a downstream **mirror** (sessions run fine `persistSession:false`) | `persistSession` guard: *"requires local writes to mirror from"* |
| E9 | The in-memory `sessionStore` is the live accumulator the request is built from | `function Vpm(e,t,n){…await e.load({projectKey:r,sessionId:t…})}` |
| E10 | Hooks **cannot** any-position rewrite: `hookUpdatedInput` is PreToolUse-only; SessionStart/UserPromptSubmit inject `additionalContext` strings only; PreCompact stdout dropped | `hookUpdatedInput` (PreToolUse scope) + prior report §1.5/§3.1 |

**The single most important fact:** `cache_control` lives *only* in the in-process request body (E5) and **never** in the `.jsonl`. Therefore only a mechanism that sees the outgoing `/v1/messages` request can manage cache economics at all — which alone eliminates candidates (3) and (4) from owning the central constraint.

### 4.2 Capability matrix (●●● full / ◐ partial / ○ none)

| Axis | (1) API Proxy | (2) Hybrid proxy+hooks | (3) Trigger-and-intercept | (4) Transcript-file rewrite |
|---|---|---|---|---|
| **Any-position rewrite** | ●●● full messages+system+tools every POST | ●●● (via proxy) | ○ whole-history prose only; PreCompact can't emit a rewritten array | ○ mid-turn: in-memory array wins (E7/E8) |
| **Continuous** | ●●● every turn | ●●● every turn | ○ boundary-only; "trigger often" multiplies cost | ○ only at re-read points (`--resume`/`--continue`/SessionStart/post-compact) |
| **`cache_control` control** | ●●● sole owner | ●●● (via proxy) | ○ busts the **entire** prefix every fire (cache-pessimal) | ○ breakpoints absent from the file (E5) |
| **Reversibility support** | ●●● swap→summary+ref on egress, expand on ingress | ●●● + hook recovery plane | ◐ rides CC's summarizer, lossy prose, no structured ref | ◐ persists summary+pointer for the *next* resume only |
| **Invasiveness** | ◐ streaming reverse proxy + auth/SSE/first-party gate | ◐ proxy + decoupled hooks | ●●● env-vars + one hook | ●●● just edits a file |
| **Version-robustness** | ●●● public Anthropic wire format | ●●● wire format + documented hook contract | ○ minified internals (`Eho`/`Hqn`/`Vut`) | ○ internal `.jsonl` schema |

**Robustness ranking:** proxy (public wire format) ≫ hooks (documented contract) ≫ transcript-rewrite (internal schema) ≫ trigger-and-intercept (minified internals).

### 4.3 RECOMMENDATION

> **Build the continuous squash engine as a streaming API proxy at `ANTHROPIC_BASE_URL` (candidate 1), with CC hooks as a decoupled control + recovery sidecar (candidate 2 hybrid). Candidate 3 (trigger native compaction) is a cache-pessimal coarse *boundary fallback tier* only. Candidate 4 (transcript-file rewrite) is refuted as a continuous engine and demoted to a *resume/post-compact persistence tactic*.**

**Rationale.** The proxy is the *only* mechanism satisfying all three non-negotiables simultaneously (any-position + continuous + cache_control), and the most version-robust. It has **confirmed working prior art**: headroom is exactly this shape — `ANTHROPIC_BASE_URL` set to a local `http://127.0.0.1:{port}` proxy (`[PRIMARY — headroom cli/wrap.py + e2e/wrap/run.py]`), an `httpx.AsyncClient` data plane (`[PRIMARY — headroom proxy/handlers/anthropic.py]`), routes forwarding to `api.anthropic.com`. The streaming/auth/tool_use plumbing is a **fork target, not a research project**.

**Plane split (decoupled, per the repo's "side-effects in listeners" rule):**
- **Data plane = the proxy.** Every `/v1/messages`: read CC's breakpoints; run the §1–§2 scorer; swap evicted segments for `summary + ref_id` blocks; re-place breakpoints (≤4, ≥1024 tok) at the new squash boundaries; expand any `retrieve()` on ingress; read back `usage` as ground truth.
- **Recovery + control plane = hooks.** `SessionStart(matcher:'compact')` re-injects the inverted continuation directive + structured `WorkingState` brief + CLAUDE.md/Skill paths (the *only* injection surface reaching the model — E10); `PreCompact` `blockedBy` defers a native compaction to a safe point.
- **Reversible store = one content-addressed backend** (§3) under two read surfaces.

**Non-obvious load-bearing detail (inherit or self-defeat):** any custom `ANTHROPIC_BASE_URL` **MUST also force `ENABLE_TOOL_SEARCH=true`** (`[PRIMARY — present in bundle.js, 21 occurrences; headroom cli/wrap.py "Set … so Claude Code keeps deferring tools"; GH #746]`). Otherwise CC stops deferring MCP/system tool schemas, materializes all of them into context, breaks sub-agents, and **self-triggers the very compaction cc-squash exists to prevent.** Bake it into `build_install_env()` as a non-optional field; assert its presence at proxy startup.

### 4.4 Adjudicating the user's two hypotheses

**Hypothesis A — "make hooks-only work continuously by triggering native compaction often."** **REJECTED as the engine; usable only as a last-resort boundary fallback.** Three independent reasons: (1) **Granularity** — native compaction is whole-history (`isCompactSummary:true`), no any-position control. (2) **Cache cost** — each fire busts the *entire* prefix at the 1h write multiplier (2.0×, E6); triggering it *often* **multiplies** worst-case cost, the opposite of what the cost model wants. (3) **Hook reach** — PreCompact can only `blockedBy` or merge `newCustomInstructions`, never emit a fine-grained rewritten array, and its stdout is dropped (E10). *Where it earns a place:* a fallback tier when the proxy is absent, to ride CC's own summarizer rather than reimplementing summarization, and to defer a forced boundary via `blockedBy`. This is bioqa's Tier-2 lossy ladder analog — correct as a last resort, wrong as the engine.

**Hypothesis B — "would rewriting the transcript file work?"** **REFUTED for mid-turn live use; valid only as a resume/post-compact persistence tactic.** CC builds each live request from an in-memory `sessionStore` (E9 `Vpm`), the loader prefers it over the file (E7 `Ppm` short-circuit), and the file-parse path is gated on `(resume||continue)&&sessionStore` (E7). Sessions run correctly with `persistSession:false` and the guard text calls disk writes a thing the adapter *"mirrors from"* (E8) → the `.jsonl` is a downstream mirror. Consequently **mid-turn edits to the `.jsonl` do not change the bytes CC renders into the next prefix**; they land only at `--resume`/`--continue`/`SessionStart`/post-compact loads. Additionally **`cache_control` is not in the file at all** (E5), so this mechanism can *never* control cache economics even where it bites. *Where it's still useful:* a persistence tactic for what survives a resume (write the durable summary+pointer there so a `--resume` rebuilds the squashed state), respecting the `compact_boundary` fast-skip unless `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP`. **Honesty note:** this verdict is **strong static inference** `[INFERRED from PRIMARY E7–E9]`, not yet empirically observed — **Experiment A (§6) is the explicit, highest-value confirmation; static evidence predicts the rewrite is ignored mid-turn.**

**The force-reload question (`/rewind`, `/fork`, signals) — does any in-session command rescue candidate 4?** Because a transcript rewrite only bites when CC re-reads the file, the natural follow-up is whether any command/signal can *force* that re-read mid-session. Chased exhaustively against the binary (full memo: `transcript-reload-feasibility.md`); the answer is **no clean mid-turn reload exists** `[PRIMARY]`. `/rewind` (`e7t @147807831`) is a pure in-memory `Tc.current.slice` + `setMessages` with **no fs read** (it ignores external file edits); `/clear` mounts a fresh empty conversation; `/branch` spawns a separate entity; `/resume` switches to a different conversation via the cold path; and **no file-watcher exists** on the transcript (`watchFile` is git-only). The one partial exception is **`/fork`** (`Xrl`/`Qrl @142183586`), which *does* re-read disk — but it switches session identity and keeps a disk record only if its UUID is already in the in-memory set (`p=new Set(e.messages.map(uuid))`), so only **UUID-preserving content edits** survive (new-UUID inserts are dropped) and it **cannot be driven headlessly**. The only reliable land path is a **process relaunch** (`execRelaunch`/`xAp @139491693`, or a fresh `--resume <abs.jsonl>`), which drops TUI state and needs a valid leaf-uuid chain (else `sHo` throws `jne`). **Net:** force-reload does not rescue candidate 4 for live use — it independently re-confirms the proxy is load-bearing (Experiments A/F scope the resume tier this still serves).

### 4.5 Residual mechanism open questions (resolved during experiments)

- **Breakpoint coexistence** — does the proxy *strip-and-replace* CC's breakpoints or *compose* with them? Determines whether the bust model is computed against CC's caching or cc-squash's own. The recommendation is **strip-and-replace** so cc-squash fully owns the economics, but the exact count CC emits into the final 4-cap request was not traced (Exp D answers it). `[DEFERRED-EXP]`
- **Wrote-then-immediately-invalidated transient** — pricing docs describe steady-state, not busting a block written-but-not-yet-read; the model approximates it as `S_after·(w−r)` (Exp D probes it). `[DEFERRED-EXP]`
- **Hidden retrieval sub-loop** — if cc-squash expands `retrieve()` on ingress with a server-side continuation (like headroom's `CCRResponseHandler`, max 3 rounds), it could desync CC's streaming/turn accounting; prefer a **single-shot ingress expansion** (inject the original as a normal `tool_result`, no extra API rounds) unless a live test proves the multi-round loop transparent. `[DEFERRED-EXP]`

### 4.6 Native content-replacement records (a cache-preserving complement, not a substitute)

A genuinely useful primitive surfaced while chasing the force-reload question `[PRIMARY — transcript-reload-feasibility.md]`: CC has a **native content-replacement record** system (`insertContentReplacement @144461611`, applier `B$d @137627496`) that swaps a `tool_result`'s content **per `tool_use_id`**, gated by the `tengu_hawthorn_steeple` flag, and **re-applies the replacement on every reload**. This is exactly the keep-prefix/rewrite-tail shape cc-squash wants for its single most common squash target — an oversized `tool_result` (a file read, a command dump) — and CC already persists and re-applies it natively.

Two ways cc-squash exploits it `[INFERRED → DEFERRED-EXP, Experiment G]`:
- **As the structured persistence format for the resume tier.** Authoring a `{type:"content-replacement", sessionId, agentId, replacements:[{tool_use_id, content}]}` record on disk makes a squashed tool-output survive `--resume` *and* re-apply via `B$d` on reload — the CC-native, schema-stable version of the "persistence tactic" §4.4 scopes candidate 4 into, and strictly better than rewriting the `.jsonl` body (which the loader may ignore). This is how a proxy-squashed `tool_result` is made durable across a restart without fighting CC's own format.
- **As a marker to avoid double-squashing.** Where CC *itself* replaces a large `tool_result`, the proxy must recognize the replacement so it neither re-squashes already-shrunk content nor expands a CC-owned placeholder it doesn't manage.

It does **not** replace the proxy: it is `tool_result`-only, applied at reload (not mid-turn), and carries no `cache_control` — so it cannot own the continuous cache economics. It is the cleanest on-disk complement for the resume/persistence tier and a strong reason the reversible store (§3) should be able to emit CC-native replacement records as one of its persistence backends.

### 4.7 Mechanism follow-ups — a v0 to ship first, a free-bust reload tier, and a confirmed dead end

Three further mechanisms were chased to ground against the binary (full memo: `mechanism-followups.md`); two earn a place in the roadmap, one is closed.

**(a) Tier-0 v0 — the summarization-response proxy [ship this first].** The full continuous engine (§4.3) and a *much smaller* intervention share the same plumbing: a proxy at `ANTHROPIC_BASE_URL`. CC's compaction summary comes from one distinguishable `/v1/messages` call, matchable on the wire by the literal `CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.` in the **last user message** — **exactly 2 occurrences in the whole bundle, both compaction builders** `[PRIMARY]` (`querySource` and `metadata.user_id` are NOT on the wire and don't discriminate; corroborate with `max_tokens ≤ 20000` and `tool_choice` absent). The proxy can **fully synthesize** the response (never call upstream), emitting an SSE assistant message whose text wraps the summary in `<summary>…</summary>` (`kRn`/`HRn`/`gkd` key on that literal; an empty/malformed stream trips CC's "check for a proxy or gateway" error `@144725302`, so it must be non-empty with a plausible `usage` block). This lets cc-squash **run its own compaction logic** — inject a structured `WorkingState`, preserve constraints/user directives verbatim — as a low-risk product that ships *before* any continuous rewriting, validates the entire proxy/SSE/auth harness end-to-end, and yields the interception code the continuous engine reuses. **Honest limits** `[PRIMARY]`: it controls only the summary *text* — not `messagesToKeep` (CC owns the kept tail), not the prompt cache (`boundaryMarker` sits at index 0 of `c_e` `@141856036`, so native compaction busts the whole prefix regardless), and **not C5**: the "continue … without asking the user any further questions" directive is added CC-side by `UOt @136227206` *after* the summary, so it survives a perfect summary and must still be countered by `SessionStart(matcher:'compact')` re-injection (§4.3) or by the full proxy rewriting the post-compaction messages. Tier-0 buys compaction *quality*, nothing for cache economics — but it is the right first ship. Validated by Experiment V0 (§6) + memo E1–E2/E4–E5.

**(b) Cooperative-reload tier — a free-bust context-reduction lever.** If the user will cooperate with a manual reload, the easiest durable squash is: cc-squash **rewrites the `.jsonl` in place** (keep every `uuid`+`parentUuid`, shrink `message.content` to summary+pointer), then the user **quits and runs `claude --resume <abs.jsonl>`** — **not `/fork`**. `--resume` cold-loads the whole file via `sHo`→`nce` (no UUID-intersection), so rewrites, inserts, *and* deletions all land; it reuses the same `sessionId`+write-target (idempotent), is scriptable, and fails loudly (`jne` → "Unable to load transcript"). `/fork` is strictly worse (UUID-intersection `p=new Set(e.messages.map(uuid))` survives only in-place rewrites, mints a new session, interactive-only). Validity gates `[PRIMARY]`: ≥1 message; ≥1 reachable timestamped leaf in `{user,assistant,attachment,system}` (`b6 @144446804`); file < 256 MiB; valid JSON; a single broken `parentUuid` tolerated (timestamp fallback) but an orphaned leaf is not; for a > 5 MiB file with a `compact_boundary`, set `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP=1` or keep edits after the last boundary. **Cache framing:** `--resume` is the cold path, so the full prefix re-sends cold regardless — the reload is a **free bust** (another entry in §1.4's free-bust set), and this tier is a **context-reduction lever only, never warm-cache economics**. It is the durable complement to the proxy: the proxy manages the *live* warm cache; a cooperative `--resume` resets the floor for free between sessions.

**(c) In-memory manipulation — confirmed dead end.** Overwriting the in-memory array or forcing a reload by manipulating the running process is closed `[PRIMARY]`: CC ships an **anti-debug kill-switch** `if(Yqm())process.exit(1)` (`@148245218`) that fires on any `--inspect`/`--debug` (in `execArgv` or `NODE_OPTIONS`) or a live `inspector.url()`; the standalone has no `BUN_INSPECT`, no `SIGUSR1` inspector escape hatch, no signal handler that re-reads disk, and IPC carries only auth verbs. `execRelaunch` itself cold-spawns a child and exits the parent (destroying the very memory it would preserve). This **reinforces** that the `ANTHROPIC_BASE_URL` proxy is the sole in-flight mechanism — there is no shortcut around it.

---

## 5. Data model, algorithm & Python skeleton

This is a **SKELETON**, not a finished implementation: signatures, frozen dataclasses, and the control-flow shape that the build must follow. It matches repo style (Python 3.13+, `from __future__ import annotations`, frozen+slots, `NewType` branding, pattern-matched dispatch, async-native I/O, functional style, no defensive coding). Bodies are elided or stubbed where the logic is specified elsewhere in this doc.

### 5.1 Module layout

```
cc_squash/
  economics.py     # MODEL_ECONOMICS, ModelEconomics, CacheState, CacheUsage — the cost constants + warmth (§1)
  segment.py       # Segment, SegmentKind, segment_prompt(), fresh_boundary() — derived scoreable units (§2.2)
  salience.py      # Salience, Constraint, Decision, InFlightWork, WorkingState, is_pinned() (§2.3)
  strategy.py      # Strategy ADT (Keep/Truncate/Summarize/ReversibleRef/Drop) + ladder priority (§2.5)
  score.py         # SquashCandidate, SquashBatch, bust_cost/recurring_saving/break_even/npv, select_strategy (§1.3, §2.4)
  controller.py    # Controller / SquashController — L0 observe / L1 schedule / L2 flush; SquashDecision (§2.1)
  breakpoints.py   # BreakpointPlan, plan_breakpoints(), cap_cache_hints(4) — owns cache_control on egress (§1.7)
  refs/            # the reversible-reference subsystem (§3)
    ids.py records.py store.py placeholder.py mcp.py
    fuse/{__init__,fusepy_backend,pyfuse3_backend}.py
  proxy/           # data plane (candidate 1) — httpx streaming reverse proxy at ANTHROPIC_BASE_URL (§4)
    server.py handlers.py install.py
  squash/          # off-critical-path candidate GENERATION (anyio): bioqa 4-way ContentDecision → SquashCandidate
  hooks/           # control/recovery sidecar: SessionStart(matcher:'compact'), PreCompact blockedBy (§4.3)
```

### 5.2 Branded primitives and the economics table

```python
from __future__ import annotations

from dataclasses import dataclass
from enum import Enum, StrEnum
from typing import NewType

ModelId   = NewType("ModelId", str)    # e.g. "claude-opus-4-8"
RefId     = NewType("RefId", str)      # "sha256:<64-hex>" of original bytes
MessageId = NewType("MessageId", str)  # transcript message uuid


@dataclass(frozen=True, slots=True)
class ModelEconomics:
    base_input: float        # $/token
    write_mult: float        # 2.0 (1h auto_mode) | 1.25 (5m forced)
    read_mult: float         # 0.1
    min_cache_floor: int     # 1024 Opus 4.8 / Sonnet 4.x


MODEL_ECONOMICS: dict[ModelId, ModelEconomics] = {
    ModelId("claude-opus-4-8"): ModelEconomics(5e-6, 2.0, 0.1, 1024),
    # ... Sonnet 4.6/4.5 (3e-6, 2.0, 0.1, 1024); Haiku 4.5 (1e-6, 2.0, 0.1, 4096)
}
```

### 5.3 CacheState — warmth, TTL, cold gate

```python
@dataclass(frozen=True, slots=True)
class CacheUsage:
    cache_creation_input_tokens: int
    cache_read_input_tokens: int
    input_tokens: int


@dataclass(frozen=True, slots=True)
class CacheState:
    cached_prefix_tokens: int
    last_request_ts: float
    assumed_ttl_s: float           # 3600.0 auto_mode | 300.0 forced-5m
    model: ModelId
    breakpoints: tuple[int, ...]   # observed cache_control offsets (CC's, before strip-and-replace)

    def idle_seconds(self, now: float) -> float:
        return now - self.last_request_ts

    def p_alive(self, now: float) -> float:
        return max(0.0, min(1.0, 1.0 - self.idle_seconds(now) / self.assumed_ttl_s))

    def is_cold(self, now: float) -> bool:
        return self.idle_seconds(now) >= self.assumed_ttl_s
```

### 5.4 Segment, Salience, WorkingState

```python
class SegmentKind(StrEnum):
    USER_TURN = "user_turn"; ASSISTANT_TURN = "assistant_turn"; TOOL_PAIR = "tool_pair"
    SYSTEM = "system"; TOOLS = "tools"


@dataclass(frozen=True, slots=True)
class Segment:
    index: int
    kind: SegmentKind
    byte_offset: int           # position in the rendered prefix — the position lever
    token_estimate: int
    generation: int            # user-turn ordinal — the freshness boundary
    pinned: bool               # carries a live Constraint/Decision/InFlightWork
    is_current: bool           # last segment — always verbatim
    source_uuids: tuple[MessageId, ...]


class Salience(Enum):
    CONSTRAINT = 1; DECISION = 2; IN_FLIGHT = 3


@dataclass(frozen=True, slots=True)
class Constraint:
    text: str
    source_message: MessageId
    superseded_by: MessageId | None = None   # bi-temporal: live iff None


@dataclass(frozen=True, slots=True)
class Decision:
    text: str
    rationale: str
    planned: bool
    superseded_by: MessageId | None = None


@dataclass(frozen=True, slots=True)
class InFlightWork:
    task: str
    last_safe_point: str
    open_files: tuple[str, ...] = ()
    skill_paths: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class WorkingState:
    constraints: tuple[Constraint, ...] = ()
    decisions: tuple[Decision, ...] = ()
    in_flight: InFlightWork | None = None
```

### 5.5 Strategy ADT (pattern-matched dispatch)

```python
@dataclass(frozen=True, slots=True)
class LineRange:
    start: int
    end: int


@dataclass(frozen=True, slots=True)
class Keep: ...
@dataclass(frozen=True, slots=True)
class Truncate:
    ranges: tuple[LineRange, ...]
@dataclass(frozen=True, slots=True)
class Summarize:
    content: str
@dataclass(frozen=True, slots=True)
class ReversibleRef:
    ref: RefId
    summary: str
@dataclass(frozen=True, slots=True)
class Drop: ...                      # fallback tier only; never in the continuous loop

Strategy = Keep | Truncate | Summarize | ReversibleRef | Drop
LADDER_PRIORITY: tuple[type, ...] = (Truncate, Summarize, ReversibleRef, Keep)
PRE_GATE_MIN_CHARS = 256
```

### 5.6 The cost/benefit functions and strategy selection

```python
@dataclass(frozen=True, slots=True)
class Cost:
    dollars: float
    tokens: int


@dataclass(frozen=True, slots=True)
class SquashCandidate:
    earliest_offset: int     # p
    suffix_tokens: int       # S_after — tokens from p to end
    net_removed: int         # T_removed = original − (summary + pointer)
    quality_gain: float      # Q (>=0), $-equivalent
    ref_id: RefId
    strategy: Strategy


@dataclass(frozen=True, slots=True)
class SquashBatch:
    candidates: tuple[SquashCandidate, ...]

    def head_offset(self) -> int:
        return min(c.earliest_offset for c in self.candidates)

    def suffix_tokens(self) -> int:
        # S_after at the head-most pending edit (batching: K edits cost one bust at min p_i)
        return max(c.suffix_tokens for c in self.candidates)

    def total_removed(self) -> int:
        return sum(c.net_removed for c in self.candidates)


def bust_cost(batch: SquashBatch, cache: CacheState, econ: ModelEconomics, *, now: float) -> Cost:
    if cache.is_cold(now):
        return Cost(0.0, 0)
    suffix = batch.suffix_tokens()
    dollars = suffix * econ.base_input * (econ.write_mult - econ.read_mult) * cache.p_alive(now)
    return Cost(dollars, suffix)


def recurring_saving(batch: SquashBatch, econ: ModelEconomics, n_turns: float) -> Cost:
    removed = batch.total_removed()
    return Cost(n_turns * removed * econ.base_input * econ.read_mult, removed)


def break_even_turns(batch: SquashBatch, cache: CacheState, econ: ModelEconomics, *, now: float) -> float:
    if cache.is_cold(now):
        return 0.0
    return batch.suffix_tokens() * (econ.write_mult - econ.read_mult) / (batch.total_removed() * econ.read_mult)


def select_strategy(seg: Segment, decision: ContentDecision, cand: SquashCandidate,
                    econ: ModelEconomics, *, cold: bool, remaining_turns: float) -> Strategy:
    # cache-cost fold: a Keep is correct when NPV<=0 even if the LLM said "summarize"
    one = SquashBatch((cand,))
    npv = recurring_saving(one, econ, remaining_turns).dollars + cand.quality_gain \
        - bust_cost(one, CACHE_FROM(cold), econ, now=NOW()).dollars
    if seg.pinned or npv <= 0 or len(SEG_TEXT(seg)) < PRE_GATE_MIN_CHARS:
        return Keep()
    match decision.choice:
        case "truncate": return Truncate(decision.ranges)
        case "summarize": return Summarize(decision.summary)
        case "compress":  return ReversibleRef(cand.ref_id, decision.summary)   # bioqa compress → reversible-ref
        case "keep":      return Keep()
```

### 5.7 SquashDecision and the continuous controller loop

```python
class HoldReason(StrEnum):
    SUB_FLOOR = "sub_floor"; WARM_DEEP = "warm_deep"; AWAIT_COLD = "await_cold"
    AWAIT_MODEL_SWITCH = "await_model_switch"; REF_HOT = "ref_hot"

class FreeBustTrigger(StrEnum):
    COLD = "cold"; MODEL_SWITCH = "model_switch"; NATIVE_COMPACTION = "native_compaction"


@dataclass(frozen=True, slots=True)
class BreakpointPlan:
    positions: tuple[int, ...]   # <=4, each at the END of a stable rewritten prefix, within 20-lookback


@dataclass(frozen=True, slots=True)
class Flush:
    batch: SquashBatch
    breakpoint_plan: BreakpointPlan
    predicted_bust: Cost
    predicted_saving: Cost
@dataclass(frozen=True, slots=True)
class Hold:
    reason: HoldReason
@dataclass(frozen=True, slots=True)
class RideFreeBust:
    batch: SquashBatch
    trigger: FreeBustTrigger

SquashDecision = Flush | Hold | RideFreeBust


@dataclass(frozen=True, slots=True)
class _Status:        # NamedTuple-style: each field maps one-to-one onto §1.8's hold/flush rules
    cold: bool
    sub_floor: bool
    warm_clears: bool
    free_bust_imminent: bool


class Controller:
    # slots; holds the MODEL_ECONOMICS view + EWMA(N) + live CacheState

    def decide(self, prompt: PromptState, pending: SquashBatch, *, now: float) -> SquashDecision:
        if pending.candidates == ():
            return Hold(HoldReason.WARM_DEEP)            # nothing staged
        econ, cache = self._econ, self._cache
        status = _Status(
            cold=cache.is_cold(now),
            sub_floor=self._post_squash_below_floor(prompt, pending, econ),
            warm_clears=self._npv(pending, cache, econ, now=now) > 0,
            free_bust_imminent=self._free_bust_imminent(now),
        )
        match status:
            case _Status(sub_floor=True):                     return Hold(HoldReason.SUB_FLOOR)
            case _Status(cold=True):                          return RideFreeBust(pending, FreeBustTrigger.COLD)
            case _Status(free_bust_imminent=True):            return RideFreeBust(pending, FreeBustTrigger.MODEL_SWITCH)
            case _Status(warm_clears=True):                   return Flush(pending, self._plan(prompt), *self._pred(pending, cache, econ, now=now))
            case _:                                           return Hold(HoldReason.WARM_DEEP)

    def observe(self, usage: CacheUsage, *, now: float) -> None:
        # ground-truth calibration (§1.5): cached-prefix len, infer w, detect over-bust, alarm on 0/0
        ...

    def plan_breakpoints(self, prompt: PromptState) -> BreakpointPlan: ...
```

### 5.8 The off-critical-path scheduler and egress flush (anyio)

```python
import anyio


class SquashController:
    """L0 observe (sync, read-only) / L1 schedule (async, off-path) / L2 flush (sync, on egress)."""

    def observe(self, body: AnthropicMessagesBody, usage: CacheUsage, *, now: float) -> CacheState:
        ...  # L0: refresh CacheState, recompute breakpoints, cheap pressure estimate

    async def schedule(self, segments: tuple[Segment, ...], state: CacheState,
                       econ: ModelEconomics, *, now: float) -> SquashBatch:
        # L1: score candidates off the critical path; the LLM ContentDecision call is delegated
        async with anyio.create_task_group() as tg:
            results: list[SquashCandidate] = []
            for seg in segments:
                if self._candidate(seg, state):
                    tg.start_soon(self._score_one, seg, state, econ, results)   # off-path
        return SquashBatch(tuple(results))

    def should_flush(self, plan: SquashBatch, state: CacheState, econ: ModelEconomics, *, now: float) -> bool:
        return self._npv(plan, state, econ, now=now) > 0 or state.is_cold(now)

    def flush(self, body: AnthropicMessagesBody, plan: SquashBatch) -> AnthropicMessagesBody:
        # L2: apply actions (pattern-matched), recompute cache_control once at min(p_i)
        out = body
        for cand in plan.candidates:
            match cand.strategy:
                case Keep():                          continue
                case Truncate(ranges=ranges):         out = _filter_lines(out, cand, ranges)
                case Summarize(content=content):      out = _substitute(out, cand, content)
                case ReversibleRef(ref=ref, summary=s): out = _swap_for_pointer(out, cand, ref, s)
                case Drop():                          out = _drop(out, cand)        # fallback only
        return _reposition_breakpoints(out, plan)
```

### 5.9 The reversible store (async-native, one read/write codepath)

```python
@dataclass(frozen=True, slots=True)
class RefRecord:
    ref_id: RefId
    byte_len: int             # drives FUSE st_size WITHOUT loading the blob (lazy stat)
    token_estimate: int
    source_uuid: str
    session_id: str
    kind: SegmentKind
    created_at: float
    # mutable accounting (last_access_at, access_count, pinned) lives in SQLite columns, not here


@dataclass(frozen=True, slots=True)
class Materialized:
    ref_id: RefId
    text: str
    token_estimate: int
    access_count: int         # flows back to the scorer as the anti-thrash signal


class RefStore:
    @classmethod
    async def open(cls, project_dir: Path) -> RefStore:
        ...  # resolve cc_squash_refs.db beside transcripts; WAL/synchronous=NORMAL/busy_timeout=5000; chmod 0o600

    async def put(self, *, original: bytes, source_uuid: str, session_id: str, kind: SegmentKind) -> RefRecord:
        ...  # SOLE writer; INSERT ... ON CONFLICT(ref_id) DO UPDATE (content-addressed dedup)

    async def materialize(self, ref_id: RefId) -> Materialized | None:
        ...  # SOLE read path (MCP tool AND FUSE read() both call this); bumps access_count+last_access_at; None ⇒ recovery hint

    async def gc(self, *, reachable: set[RefId], grace_seconds: float, max_bytes: int) -> int:
        ...  # never delete a ref present in any live transcript (correctness invariant)
```

### 5.10 Mapping table — design lever → skeleton symbol

| Design lever (§) | Skeleton symbol |
|---|---|
| Bust cost = `S_after·b·(w−r)`, 0 when cold (§1.3) | `score.bust_cost` |
| Break-even `N* = 19·S_after/T_removed` (§1.3) | `score.break_even_turns` |
| Cold gate + `P_alive` decay (§1.4) | `CacheState.is_cold` / `CacheState.p_alive` |
| Batching: K edits → one bust at `min(p_i)` (§1.4) | `SquashBatch.head_offset` / `.suffix_tokens` |
| Freshness eligibility × salience pin (§2.3) | `segment.fresh_boundary` + `salience.is_pinned` |
| Cache-aware lossy ladder (§2.5) | `strategy.*` + `score.select_strategy` (`match`) |
| Hold negative space (§1.8) | `Controller.decide` `match _Status(...)` |
| Breakpoint placement (§1.7) | `breakpoints.plan_breakpoints` / `cap_cache_hints` |
| Reversible dual surface (§3.5) | `RefStore.materialize` (sole path) + `refs/mcp.py` + `refs/fuse/` |
| Ground-truth calibration (§1.5) | `Controller.observe(CacheUsage)` |

---

## 6. Feasibility experiments (DO NOT RUN this turn)

Ordered by value. All live experiments require an interactive TTY (auto-compaction does not fire in `-p`/print/SDK mode `[PRIMARY — prior research S1-F16]`) and are explicitly **not run this turn** (no live proxy, no real CC sessions, no FUSE mounting, no API spend). **Experiment A is the single highest-value diagnostic** (it settles candidate 4); **Experiment V0 is the ship-first build target** — the Tier-0 summarization-proxy (§4.7a) that validates the whole proxy/SSE/auth harness with one matchable request.

| # | Experiment | Method | Success criterion | Rough cost |
|---|---|---|---|---|
| **V0** ★ | **Tier-0 summarization-proxy** (ship-first; validates the harness, §4.7a) | Passthrough-logging proxy at `ANTHROPIC_BASE_URL` (`ENABLE_TOOL_SEARCH=true`); trigger `/compact`; confirm exactly one request carries `CRITICAL: Respond with TEXT ONLY` in the last user message (zero false pos/neg across the session); then short-circuit it with a synthesized `<summary>…</summary>` SSE stream | Compaction call is uniquely matchable **and** a fabricated summary is accepted (post-compact message has `isCompactSummary:true`, no "proxy or gateway" error) | ~1–2 hr, ~minimal API |
| **A** ★ | **Mid-turn transcript-rewrite** (settles Hypothesis B) | Interactive CC session, produce a distinctive `tool_result`; while idle, edit that `.jsonl` record to a sentinel; send a turn forcing a reference; capture the outgoing request (via Exp C pass-through proxy or `count_tokens` delta) | **Sentinel in next request** ⟹ file rebuild (cand. 4 viable); **ORIGINAL in next request** ⟹ in-memory array wins (cand. 4 dead mid-turn). **Prediction: original sent.** | ~30 min, ~near-zero API |
| B | Confirm + characterize headroom's proxy (fork target) | Read `proxy/server.py` + `proxy/handlers/anthropic.py` (streaming) + `cli/wrap.py` line-by-line; document `ANTHROPIC_BASE_URL` wiring, SSE pass-through, auth (`x-api-key` vs OAuth Bearer), first-party gate | A documented working proxy skeleton (auth verified) to fork | ~1–2 hr, zero API |
| C | Streaming-proxy round-trip (prove the data plane) | Minimal `httpx` reverse proxy at `ANTHROPIC_BASE_URL` pass-throughing verbatim (set `ENABLE_TOOL_SEARCH=true`); point CC at it; multi-tool interactive turn; watch the first-party gate | Identical behavior, intact SSE, no 401, tool calls round-trip | ~½ day, ~$1–2 API |
| D | `cache_control` observability + rewrite (validate bust-position term) | Log breakpoints across consecutive turns; then move/drop one breakpoint and measure realized `cache_creation_input_tokens` vs `cache_read_input_tokens` | We can read CC's breakpoints AND our edits change realized cache cost as predicted (tail cheap, head expensive); resolves strip-vs-compose + breakpoint census + the wrote-then-invalidated transient | ~½ day, ~$3–5 API |
| E | `SessionStart(matcher:'compact')` re-injection reach | Interactive TTY: force compaction (low `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`); hook emits a unique sentinel via `hookSpecificOutput.additionalContext`; check recovered context | Sentinel reaches the model (re-verifies E10 on v2.1.183) | ~30 min, ~$1 API |
| F | Resume-time transcript-rewrite (scope where cand. 4 *does* bite) | Edit `.jsonl` while CC stopped, then `--resume` (respect `compact_boundary` fast-skip unless `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP`) | Edits survive into the resumed in-memory array ⟹ valid persistence tactic | ~30 min, ~near-zero API |
| **G** | Content-replacement records as the persistence primitive (§4.6) | Trip CC's tool-result threshold (or enable `tengu_hawthorn_steeple`) so CC writes a `content-replacement` entry; `--resume` and confirm `B$d` swaps the big `tool_result` by `tool_use_id` on reload with bytes before the first replaced block unchanged; then author a record directly on disk and confirm CC re-applies it | CC re-applies our on-disk replacement on resume, prefix bytes intact ⟹ structured persistence tier viable | ~30 min, ~near-zero API |
| **Calibration set** (economics — needs recorded usage, no live spend) | | | | |
| Cal-w | Calibrate the write multiplier `w` | Read `cache_creation_input_tokens` across a real REPL session, reconcile against billed cost | Confirms CC's resolved TTL is 1h (`w=2.0`) vs 5m (`w=1.25`) for *this* user's plan/route | ~recorded, ~minimal API |
| Cal-P | Calibrate `P_alive` decay + sliding TTL | Idle a warm session for measured gaps (60/180/280/360 s for 5m; minutes-scale for 1h), record `cache_read` drop-off | Fits the real eviction curve; confirms sliding-from-last-access; validates the cold-cache window + linear-decay assumption | ~½ day, ~minimal API |
| Cal-tok | Token-estimate error | Compare `chars/3.5` proxy and `count_tokens` against billed `input_tokens` for sampled segments | Bounds proxy error; sets the threshold below which a borderline candidate must be re-scored with `count_tokens` | recorded, zero API |
| Cal-floor | Min-floor disengage detection | Deliberately squash a prefix below 1,024 tok | Next response returns `cache_creation==0 AND cache_read==0` (silent uncached), validating the alarm + auto-revert | recorded, ~minimal API |
| Cal-gen | Net generation break-even | Measure token+latency cost of the off-path summarizer (e.g. Haiku 4.5) per squashed segment | Sets the minimum (segment_size, age, cache-warmth) at which an LLM-driven squash pays for itself net of its own generation cost *plus* the bust it triggers | recorded |
| **Offline policy set** (deterministic, CI-runnable, zero API) | | | | |
| Pol-replay | Policy-chooses-well | Replay recorded sessions split at real `compact_boundary` markers; feed each turn's body + real `usage` into the controller; assert the `SquashPlan` | (a) never evicts a live `Constraint`/`Decision`/`InFlightWork`; (b) never flushes a head edit while warm + `N̂` small; (c) flushes tail + cold-cache edits; golden-labelled load-bearing entities (paths like `cli.py`/`pyproject.toml` that provably reappear post-boundary) are pinned | CI, zero API |
| Pol-npv | NPV-vs-ground-truth | Per turn, compare predicted `S_after`/bust from the proxy estimate vs realized `cache_creation` the next response billed | Estimator error bounded; over-bust detector fires when realized > predicted | recorded, zero API |
| Pol-batch | Batching-is-free (pure) | Synthetic layouts; assert batched `flush_offset == min(p_i)` and a single bust = `bust(S_after(min p_i))`, not the sum; low-value tail squashes defer then ride free | Lever 2 encoded correctly | pure |
| Pol-cold | Cold-cache-free-flush (pure + fake clock) | Mocked `CacheState` past `ttl_s` ⇒ `bust_cost==0` and L2 flushes the entire staged plan; inside TTL ⇒ head edits stay staged; toggle `write_mult` 5m/1h ⇒ `N*` changes | Lever 3 (the headroom-gated idle gate) is live | pure |
| Pol-ladder | Strategy-ladder dispatch + pre-gates (pure, parameterized) | Fixture `ContentDecision`+`SquashCandidate` → `select_strategy`; assert `Keep()` for <256 chars, `Keep()` when `npv<=0` even if `summarize`, `Truncate` for ranges, `ReversibleRef` for `compress`, tool-pairs never split | Dispatch + pre-gates correct | pure |
| **FUSE set** (from the FUSE investigation) | | | | |
| E-fuse-1 | `fusepy` ro-mount materializes a blob on macFUSE | Install macFUSE on Tahoe 26.4.1, `FusepyBackend` mounts `<project>/.cc-squash/refs` ro, store a known blob, `Read`/`cat` the FUSE path | Byte-identical readback (confirms FSKit userspace, no kext) | ~½ day, zero API |
| **E-fuse-2** ★ | **Sandboxed CC Read of the mount** (highest FUSE risk) | From inside an actual CC tool-execution sandbox, `Read` a path under the mount placed inside the project dir | Returns the materialized original ⟹ dual surface viable; "Operation not permitted" ⟹ FUSE `Read` line permanently omitted, `retrieve()` sole surface | ~½ day, zero API |
| E-fuse-3 | Lazy stat | Instrument `materialize` call count; `ls -l`/stat the FUSE dir without reading bytes | ZERO `materialize` calls on `getattr` (served from `byte_len`); only `read()` triggers it | zero API |
| E-fuse-4 | Cross-session mount lifecycle + crash recovery | Mount/store/unmount across two sessions; assert refs resolve in session 2 from disk; then `SIGKILL` the mount, assert stale-mount cleanup (`umount -f`/`diskutil unmount`) lets a fresh mount succeed | Persistence + no stuck mountpoint after crash | zero API |
| **Refs set** | | | | |
| E-ref-1 | Round-trip (core) | `put(original)` → `Placeholder.render()` → `REF_MARKER` re-extracts → `materialize` returns byte-identical; two identical `put`s → one row; miss → recovery hint not exception | All pass on a tmp aiosqlite DB | zero API |
| E-ref-2 | GC reachability invariant | Transcript carrying ref A's marker, none for B; `gc(reachable=…)` sweeps B, keeps A past its age cap; add then remove A's marker, assert grace-window timing | Never-delete-a-referenced-blob invariant holds | zero API |
| **Downstream A/B** (deferred — no live spend; enumerate only) | | | | |
| AB-oracle | Oracle-vs-compacted downstream | Three-arm paired design (≥10 seeds) from the SAME pre-compaction prefix + model snapshot + sampling params: Arm-oracle (no-squash full context), Arm-A (CC built-in), Arm-B (cc-squash continuous); same blind LLM judge (Recall/Artifact/Continuation/Decision probes) | Paired McNemar (binary) + Wilcoxon signed-rank (graded) with paired-bootstrap BCa CIs, lead with the conservative randomization p-value; proves the policy beats CC's baseline on real task success | **deferred — API credits** |

---

## 7. Open questions & risks

### 7.1 Mechanism / cache
- **Breakpoint coexistence is unresolved from static evidence.** The binary confirms CC's rolling per-turn `cache_control` (E5) plus a `systemPromptChanged`-gated system breakpoint, but **how many survive into the final 4-cap request**, and whether cc-squash **strips-and-replaces vs composes** with CC's breakpoints, was not traced. The whole bust model assumes cc-squash fully owns `cache_control` on egress. Resolved by Exp D. `[DEFERRED-EXP]`
- **The "wrote-then-immediately-invalidated" transient is unpriced.** The model approximates a busted prior-READ-now-WRITE as `S_after·(w−r)`; busting a block written-but-not-yet-read makes the original write sunk and a fresh write paid — pricing docs describe only steady-state. Exp D. `[DEFERRED-EXP]`
- **`w=2.0` may be wrong for this user.** CC's 1h cache is gated on `Ro()`/`isUsingOverage`/a statsig allowlist; if the route resolves to 5m, `w=1.25` and the break-even coefficient drops to `(1.25−0.1)/0.1 = 11.5` so `N* = 11.5·S_after/T_removed` (a lower bar than 19). Mitigated by reading back `cache_creation` billing / the resolved env flag (Cal-w). `[DEFERRED-EXP]`
- **Cold-cache timing depends on an *assumed* TTL, not an observed one.** The first cold-flush per idle gap is a calibrated gamble (the `cache_read==0` confirmation arrives only *after* the decision). Mitigated by Cal-P.
- **SSE pass-through fidelity + the first-party auth gate (E4)** are the hardest proxy parts; mis-buffering desyncs tool calls, mishandling `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` yields 401s/stripped beta headers. Exp B/C verify before any rewriting.
- **`ENABLE_TOOL_SEARCH` omission = self-defeat** (CC materializes all tool schemas, self-triggers compaction). Non-optional field; assert at startup.

### 7.2 Policy / scoring
- **`N̂` (expected remaining turns) is the softest input** and gates real spend on warm-branch head squashes; no prior art estimates it well (bioqa open question). Bias conservative (break-even at `N_p25`) and lean on the position/cold levers, which don't depend on `N̂`.
- **char-proxy vs Claude-tokenizer drift** can flip a borderline candidate's sign; mitigated by using `count_tokens` for the *final* flush + breakpoint placement (Cal-tok bounds the boundary).
- **Squash generation spends tokens to save tokens.** The off-path summarizer's own cost+latency must be netted against `saving_total` + the bust it triggers, or small/young segments look worth-squashing when they are not (Cal-gen).
- **Min-floor disengage is silent and catastrophic** (~10× recurring cost, no error) if the post-squash prefix-length estimate is wrong; the `observe()` loop must treat `cache_creation==0 AND cache_read==0` as an alarm and auto-revert (Cal-floor / Pol-replay).
- **Salience extraction is upstream and out of this module's scope but gates correctness.** If extraction misses a live `Constraint`, the controller may evict it — the exact C1 failure. The pin gate must **fail safe**: when salience is uncertain, treat the segment as pinned.

### 7.3 Reversible store / FUSE
- **FUSE-read inside CC's Seatbelt sandbox (E-fuse-2) is the single highest-risk unknown.** A profile could deny non-allowlisted mount *types*, not just paths. Until proven, `retrieve()` is the only guaranteed surface and the FUSE line is gated behind a verified mount+read self-test at startup. `[DEFERRED-EXP]`
- **`fusepy` is synchronous**, forcing the one `to_thread` boundary and a thread→loop hop (`anyio.from_thread.run`) into aiosqlite — a re-entrancy hazard if the loop is busy. Fallback: a dedicated sync SQLite reader on the FUSE thread (separate WAL connection), at the cost of a second read codepath (a deliberate, documented exception only if needed).
- **macFUSE is absent and is a heavyweight install** even on Tahoe's userspace path. The product **cannot hard-depend on it**; the FUSE extra and mount are strictly opportunistic. Risk: over-investing in FUSE vs the load-bearing MCP surface.
- **Storing full original blobs in SQLite grows the DB fast** on sessions that squash many large tool outputs. Need a per-blob max (above which the segment is not squashed-to-store) and a hard DB size ceiling that pauses squashing when hit, beyond the LRU GC.
- **GC reachability vs squash→persist race.** A ref reachable only via the proxy's in-memory stream (squashed this turn, not yet flushed to `.jsonl`) must survive: the `grace_window` must **exceed the max squash→persist latency**, verified empirically (E-ref-2), and the mark set must include the in-flight stream. A mis-timed sweep deleting a just-squashed ref before its placeholder persists is unrecoverable.
- **Over-retrieval can net-increase spend.** If the model pulls many originals back, the subsystem can cost more than never squashing; mitigated by the `access_count`→scorer anti-thrash feedback and the `query=` partial-retrieval bias, but the blacklist threshold is unknown and needs tuning against real sessions.

### 7.4 Cross-cutting
- **Token estimate stored at `put()` can drift** from real Claude tokenization, biasing every squash; treat the estimator as a tunable reconciled against usage fields, not a constant.
- **Interface boundary discipline:** `refs` mints the placeholder block, the proxy decides *when* to flush it into a single bust. A mis-coordination (placeholders the proxy doesn't batch) multiplies bust cost — the boundary must stay explicit.
- **"fusekit" dependency unconfirmed.** The user's named library doesn't exist; confirm the intended binding (fusepy / refuse / pyfuse3) before locking it (§3.0).
