# cc-squash Follow-Up Plan

Created 2026-06-25, after landing the Layer 1-4 compaction-shortcut fixes on `main`
(commit `feat(proxy): squash tool output reversibly + configure/calibrate the engine`,
CI green). Two follow-ups, in priority order. F1 is a live-discovered P0 regression-class
bug; F2 is the `ccs-eval` substrate deferred from the main plan's Phase 5.

---

## F1 (P0) — The live `ccs run` squash engine is a no-op — RESOLVED

**Status: FIXED.** Root cause was a wire-parse rejection, not the summarizer hang
hypothesized below. Found by instrumented live capture; fix + regression tests landed.

### Symptom (as observed live)

A live `ccs run` tool-heavy session forwarded perfectly (every `POST /v1/messages`
`decision=forward status=200`, context 99KB→346KB) yet produced **zero `L1 staged plan`
logs and an empty `refs.db`** — L1 never staged, L2 never intercepted.

### Actual root cause (observed, not inferred)

A throwaway probe at `relay.rs serve` dumped each inbound `/v1/messages` body and its
`parse_body` verdict. Every real body failed serde with:

> `unknown variant `system`, expected `user` or `assistant` at line 1 column N`

Claude Code injects a message with **`role: "system"`** *inside* the `messages[]` array
(the SessionStart-hook / deferred-tools reminder; `content` is a plain string), distinct
from the top-level `system` prompt field. The wire `Role` enum had only `User`/`Assistant`,
so `serde_json::from_slice` rejected the whole body. That **one** failure cascaded
uniformly: `stage_next`'s `parse_body` failed (silent early-return → no L1), and
`body_model`'s `parse_body` failed → model resolved to `"unknown"` → `economics_for`→None →
L2 disabled. One bug, the entire signature.

The original "off-path summarizer `decide()`/`fold()` hang/fail" hypothesis was **wrong as
the cause** — it was unreachable, because parsing failed first. (It is a real *fail-open*
path; see the secondary finding below.) The deepest enabling defect was **silence**: all
three `stage_next` early-returns and the `init_econ` `"unknown"` fallback discarded their
reason, which is why a live session + multiple agents were needed to corner it.

### As-built fix (landed)

1. **Parse fix** — `crates/ccs-policy/src/wire.rs`: added `System` to `enum Role`
   (snake_case). `crates/ccs-policy/src/segment.rs`: a `Role::System` arm pushes a
   `SegmentKind::System` segment with `source_uuids = [message_id(i)]`,
   `is_true_human = false`, `pinned = false`, current `generation` (no increment) — so the
   injected reminder is a normal message-indexed squash candidate. No `Any`-widening; the
   closed-variant discipline is intact. Verified the full real captured body now parses
   (`system` was the *only* non-conforming field) and flows byte-safe through
   segment → candidate → content-address match → `squash_targets` (catch-all StringContent
   target) → splice (role preserved; only `content` replaced) → `check_roles` gate.
2. **Observability** — `warn!` at `stage_next`'s parse-fail and at `init_econ`'s
   `model="unknown"` fallback (a silent economics-disable now screams); `debug!` at the
   no-candidate / poisoned-lock / ref-store-put-failed returns. The engine can never again
   silently no-op undiagnosed.
3. **RAII staging guard** — `StagingGuard` (Drop) released on *every* `stage_next` exit
   (return/error/panic in the detached task), replacing the manual `staging.store(false)`
   sites. Removes the single-shot-wedge fragility; latest-wins claim semantics unchanged.
4. **Regression tests** — `ccs-policy/tests/d1_segmentation.rs`
   (`system_role_message_in_messages_array_parses_and_segments`: a `role:"system"` body must
   parse + segment as a non-true-human `System` segment) and `ccs-proxy/tests/staging.rs`
   (`stage_next_stages_body_containing_system_role_message`: full `forward_setup`→`stage_next`
   with a real `RefStore` + mocked summarizer stages a plan and lands a ref). Both were
   confirmed to FAIL on pre-fix source (the exact `unknown variant system`) and pass after.

### Verification (done)

- Live (rebuilt, committed binary): `L1 staged plan` logs now fire every turn, candidates
  grow 4→12, **no** parse-fail / `model=unknown` / skip warnings, model resolves, fail-open
  intact. The engine engages end-to-end.
- `cargo test --all` + `cargo clippy --all -- -D warnings` green.

### Secondary finding (now visible post-fix; NOT a no-op regression)

With parsing fixed, `staged=0` persisted in back-to-back live runs because the off-path
summarizer returned **`429 Too Many Requests`** (`ccs_summarizer::decision`/`folder` warn:
"content decision failed/working-state fold failed; keeping…") — a rate limit from running
several live sessions in quick succession, each firing one `decide()` per candidate plus a
`fold()`. The code handles this correctly: a 429 fail-opens to `Keep`/prior (the cardinal
invariant holds). That `staging` *does* produce a ref when the summarizer responds is proven
deterministically by `stage_next_stages_body_containing_system_role_message` (mocked
summarizer). A live `staged≥1` observation just needs a run outside the rate-limit window.
Future hardening worth considering (out of scope here): throttle/serialize off-path
summarizer calls and/or back off on 429 so L1 does not burst the account.

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
