# cc-squash — Mechanism Follow-ups Decision Memo

**Scope.** Three follow-up mechanisms the user raised, each a potentially-simpler or complementary path to the settled streaming-proxy architecture. Each is pinned against the actual CC 2.1.183 binary (`/Users/yasyf/Code/cc-squash/.claude/research/extracted/2.1.183/bundle.js`, 152,934,641 bytes). Every load-bearing claim below was re-verified in this synthesis pass; byte offsets cited are from the live grep (small drift from the original carving offsets is expected and noted where it occurs).

**The settled baseline (do not relitigate).** A streaming API proxy at `ANTHROPIC_BASE_URL` is the only load-bearing mechanism for *mid-session, cache-preserving* squash. CC builds each `/v1/messages` request from an in-memory `sessionStore` (`Ppm` @144104849 dispatches to the in-memory `Vpm` when a sessionStore exists, falling back to the disk loader `e0l` only at cold boundaries). The `.jsonl` is a downstream mirror; no in-session command forces a mid-turn reload; a PreCompact hook cannot substitute the summary. Full prior memos: `transcript-reload-feasibility.md`, `cc-squash-architecture.md`.

---

## 1. Framing — the three mechanisms vs. the settled proxy

| # | Mechanism | One-line | Relation to settled proxy |
|---|-----------|----------|---------------------------|
| A | **Cooperative reload** | User quits + `claude --resume <abs.jsonl>` after cc-squash rewrites the on-disk transcript | **Complementary, weaker tier.** A *free* cache bust that happens anyway on restart; gives go-forward context reduction only — never warm-cache economics. The proxy stays load-bearing for mid-session work. |
| B | **In-memory manipulation** | Mutate the running process's message array / force a reload without editing disk | **Dead end.** Actively guarded by an anti-debug kill-switch; no signal/IPC/socket verb exists; would not even preserve memory if it worked. The proxy remains the *only* in-flight mechanism. |
| C | **Summarization-response proxy** | Intercept *only* CC's compaction-summarization `/v1/messages` call and return cc-squash's own structured summary | **A strict subset of the proxy — ships first.** Same `ANTHROPIC_BASE_URL` plumbing, one matchable request. Buys compaction *quality*, not cache economics. A genuine tier-0 v0 product. |

The throughline: **only the `ANTHROPIC_BASE_URL` proxy can touch a request before it is billed.** A (reload) operates on disk and only after a cold boundary that re-sends everything anyway; B cannot operate at all; C is the proxy, scoped to one request. So A and C are *additive tiers on the settled architecture*, and B is removed from the roadmap.

---

## 2. Cooperative reload

**Confidence: HIGH (confirmed-from-binary).** The single easiest user-cooperating protocol is **quit + `claude --resume <abs-path>.jsonl`** — *not* `/fork`.

### The exact protocol

1. **cc-squash rewrites the on-disk `.jsonl` in place**, using **content-rewrite-in-place** encoding: keep every record's `uuid` and `parentUuid`; shrink `message.content` to a summary + a retrieve-pointer. Preserve a non-empty message set and at least one reachable, timestamped leaf among the chained types `{user, assistant, attachment, system}`.
2. **The user quits CC and runs `claude --resume /abs/path/<sessionId>.jsonl`** (or `claude --resume <sessionId>`).

This re-enters the cold loader `sHo` → `nce`, which loads the **whole file** (no UUID-intersection filter), reuses the **same session id and file path**, and rebuilds the live in-memory array from the edited bytes.

### Why `--resume` over `/fork` (three reasons)

- **(a) Scriptable.** `--resume` is headless; `/fork` is an interactive command/keybinding only and prints a branch banner.
- **(b) Full edit expressivity.** `--resume` loads the entire file, so **content-rewrites, new-uuid inserts, and deletions all land** (subject only to the chain/leaf gate). `/fork` intersects disk records against the in-memory UUID set — `p=new Set(e.map((T)=>T.uuid))`, confirmed at **byte 142181767** (`createInterface({input:l,crlfDelay:1/0}),p=new Set(e.map((T)=>T.uuid))`) — so it survives **only in-place content rewrites of existing uuids**, silently **drops new-uuid inserts**, and **cannot express disk-only deletions**.
- **(c) Identity continuity.** `--resume` (without `--fork-session`) reuses `e.sessionId` and the same write-target file, so the cooperative loop is **idempotent** — cc-squash can re-edit the same file next cycle. `/fork` mints a **new sessionId + new `.jsonl` path** (`r=randomUUID()`, `i=tD(r)` in `Xrl`).

### Validity constraints on the edited file (enforced by `sHo`/`nce`)

Re-verified this pass:

- **≥1 message record** — else `jne("No messages found in JSONL file","no_messages")` (string @144480500).
- **≥1 reachable timestamped leaf** whose uuid survives the `parentUuid` reachability walk — else `jne("No valid conversation chain found in JSONL file","no_chain")` (string @144480610). `leafUuids` is **derived** by `nce` (reachability walk over child-less records), not read from disk — you cannot forge it; you must leave a record that ends up child-less after the walk.
- **Chained types limited to `{user, assistant, attachment, system}`** — `b6` confirmed verbatim at **byte 144446804**: `function b6(e){return e.type==="user"||...}`. Every other record type (mode, permission-mode, last-prompt, file-history-snapshot, content-replacement, …) is sidecar metadata that never enters the conversation chain.
- **File < 256 MiB** (`too_large`), **valid JSON per line**.
- **A single broken `parentUuid` link is tolerated** — `Nge`/`zgm` reattach by nearest-earlier same-`isSidechain` timestamp (`tengu_chain_timestamp_fallback`). A **fully orphaned leaf is not** tolerated (`no_chain`).
- **5 MiB hazard.** If the squashed file exceeds 5 MiB **and** contains a `compact_boundary`, `nce`'s `i_m` reverse-scanner returns only the post-boundary tail, **skipping edits before the last boundary**, unless `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP=1` forces a full read. cc-squash must keep the file < 5 MiB, place edits after the last boundary, or set that flag when driving `--resume`.

Failure is **loud**: a chain-breaking edit fires `tengu_session_resumed{success:false}`, prints "Unable to load transcript from file", and exits. It never silently corrupts.

### Free-bust cache framing (confirmed)

A process restart / `--resume` is the **cold loader path** (`Ppm` dispatches to the disk arm only when there is **no** live `sessionStore`), so **the entire prefix is re-sent cold regardless of whether we squashed.** The reload is therefore a **free cache bust**: the squash adds **zero marginal cache cost** on top of the restart that is already happening. The only realized effect is the **smaller go-forward context** — fewer tokens re-billed on this cold send and on every subsequent turn.

**Consequence for positioning:** this tier is a **context-reduction lever only, never a cache-economics lever.** The proxy remains the sole mechanism that can preserve a warm prefix mid-session.

### Structured complement: `content-replacement` records

`nce` accumulates `{type:'content-replacement', sessionId|agentId, replacements:[{tool_use_id, content}]}` and the resume restore applies them (`fXn` → `V6e`); `/fork` carries old-session replacements forward. These express **tool_result swaps by `tool_use_id` only** — not arbitrary user/assistant text — so they cleanly cover the **giant-tool-output** case while content-rewrite-in-place covers everything else. Useful complement, not a full encoding.

---

## 3. In-memory manipulation

**Confidence: HIGH (confirmed-from-binary). Verdict: NOT VIABLE — dead end.**

No in-memory route is viable for production CC. Every path is actively guarded, structurally absent, or self-defeating.

**The single strongest blocker — an anti-debug kill-switch.** At main entry, immediately after `main_tsx_imports_loaded` (@148245192), CC runs `if(Yqm())process.exit(1)` (**byte 148245218**). `Yqm` was re-extracted verbatim this pass:

```js
function Yqm(){let e=F3(),t=process.execArgv.some((r)=>{if(e)return/--inspect(-brk)?/.test(r);else return/--inspect(-brk)?|--debug(-brk)?/.test(r)}),n=je.NODE_OPTIONS&&/--inspect(-brk)?|--debug(-brk)?/.test(je.NODE_OPTIONS);try{return!!global.require("inspector").url()||t||n}catch{return t||n}}
```

So `Yqm` returns true if **any** inspector is active — `--inspect(-brk)` in `execArgv`, `--inspect/--debug` in `NODE_OPTIONS`, **or a live `inspector.url()`**. Enabling an inspector terminates the very process you wanted to mutate. (`F3()` returns `true` unconditionally, so the guard is not behind any flag.)

Layered on top, each independently fatal:

- **(a) Inspector attach + eval — BLOCKED at three layers.** Production CC is a Bun `--compile` standalone (`$bunfs` marker present; `Bun.embeddedFiles`), which does **not** honor `BUN_INSPECT` — **`rg -c 'BUN_INSPECT'` returns 0 matches** (re-confirmed: exit 1, no output). Even if an inspector were forced, `Yqm`'s `process.exit(1)` kills CC. And there is **no `process.on('SIGUSR1')` handler**, so Node's SIGUSR1→open-inspector escape hatch is gone. Defeating this needs a **binary patch** — violates READ-ONLY-RE and breaks on every version bump.
- **(b) Signal-triggered reload — NOT VIABLE.** CC handles only SIGCONT (TUI redraw after Ctrl-Z), SIGHUP/SIGINT/SIGTERM/SIGQUIT (graceful shutdown). None re-read disk or touch the sessionStore; there is no SIGUSR1/2 handler at all.
- **(c) IPC / socket / pipe — NOT VIABLE for state injection.** `process.on('message')` carries only `token_update`/`auth_401_result` (auth) and child LSP/MCP JSON-RPC; it is not externally connectable (requires being the forking parent). The 0600 unix-socket MCP server (`handleMcpClient`) is the claude-in-chrome browser bridge — inbound frames become `{type:'tool_request',method,params}` routed through the normal MCP tool gate; **no registered tool calls `setMessages` or reloads**. The bg-spare `claimSock` hands job metadata to a *separate* spare process. `ws://localhost:8765` is an *outbound* OAuth bridge client, not a listener.
- **(d) Raw lldb heap-poke — INFEASIBLE and self-defeating.** Moving, GC-managed JSC heap; no stable address/symbol for the sessionStore array; minified locals have no debug symbols; lldb is hopeless without an inspector (forbidden by `Yqm`). And the only in-process "reload" primitive, `execRelaunch`/`xAp` (@139491693), is a full `spawn(...,{stdio:'inherit'})` child fork that rebuilds sessionStore **cold** and exits the parent — it **destroys the live memory it was meant to preserve.**

**Fragility of the least-bad route (patched-inspector): CATASTROPHIC.** Requires binary mutation, breaks every CC release, races a GC heap with no stable symbols, ToS-adverse.

**Recommendation: abandon ANGLE 2 entirely.**

---

## 4. Summarization-response proxy

**Confidence: HIGH (confirmed-from-binary). Verdict: VIABLE — the most promising minimal-viable path.**

Intercepting **only** CC's compaction-summarization call at `ANTHROPIC_BASE_URL` and returning cc-squash's own structured summary is viable on every load-bearing point.

### (a) The compaction request is reliably distinguishable at the wire

**Primary matcher (high confidence): the last user message contains the literal marker `CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.`** This string occurs in **exactly 2 places in the entire 150 MB bundle** (re-confirmed: `rg -a -c` returns `2`), both being the compaction prompt builders (`E$i` @~136216361 and `IRn` @~136220397), and `IRn`'s output is the **final user message** of the request. The marker is present in **both** compaction branches (default cache-sharing and fallback).

**Not usable as discriminators** (each re-checked):
- `querySource:"compact"` — only **2 occurrences total**, and it **never reaches the HTTP body** (telemetry / OpenTelemetry span attribute / cache-TTL selection only). The body builder `z` (@~144354359) has no `querySource` field.
- `metadata.user_id` — `QRe()` produces a session-uniform JSON of `{device_id, account_uuid, session_id}`, **identical for every call** in the session.
- Beta headers — not compaction-specific.

**Secondary corroboration:** `max_tokens <= 20000` (`Akt`), `tool_choice` absent, and — **only for the fallback branch** — `system[] == "You are a helpful AI assistant tasked with summarizing conversations."`. Note the **default branch keeps the full normal system prompt** (to reuse the existing cache via `tengu_compact_cache_prefix`, default true), so the proxy **must not** match on the system prompt for the default path — only the last-user-message marker is reliable.

### (b) Response shape is trivial

Emit a normal streaming assistant message whose text contains `<summary>...</summary>`. CC's `kRn`/`HRn` finders key on the literal `<summary>`, and `gkd()` extracts the inner `<summary>` content (stripping any `<analysis>`). The `isCompactSummary:true` / `isVisibleInTranscriptOnly:true` markers are added **by CC** (`Ln({...})` wrapping our raw text), **not by the proxy**.

### (c) The proxy can fully synthesize — no upstream call required

Nothing in the parse path needs a real model turn. CC just decodes standard SSE: `message_start` → `content_block_start(text)` → `content_block_delta(text_delta)` → `content_block_stop` → `message_delta(+usage)` → `message_stop`, and extracts text. **Caveat (confirmed):** CC explicitly anticipates proxies — a malformed/empty response triggers "API returned an empty or malformed response (HTTP …) — check for a proxy or gateway intercepting the request" (string @144725302). So the synthesized stream **must be well-formed and non-empty**, with a plausible `usage` block.

### What it controls vs. does not control

**Controls — the summary TEXT.** Inject a structured `WorkingState` block (open tasks, file/edit ledger, decisions), preserve user constraints/security directives **verbatim** (a deterministic builder *guarantees* what CC's prompt merely *requests*), and drop CC's lossy free-text. The proxy even sees the **entire conversation** in the request body (`messages[]` = full `forkContextMessages` + the summarizer user message), so cc-squash runs its own compaction over the real transcript.

**Does NOT control (sets the tier's ceiling):**

1. **`messagesToKeep`** — CC owns the retained tail.
2. **The C5 "continue without asking" directive.** `UOt()` (@~136226538) wraps our summary CC-side: it prepends "This session is being continued…" and, when `isContinuation` is true, appends "Continue the conversation from where it left off without asking the user any further questions…" (string @136227206). This is added **around** our summary, so **summary-only interception cannot remove it.** Fixing C5 needs the kept-message-rewrite engine, not summary interception.
3. **Prompt cache.** `c_e` was re-extracted verbatim: `function c_e(e){return[e.boundaryMarker,...e.summaryMessages,...e.messagesToKeep,...e.attachments,...e.hookResults]}` (@141856036). `boundaryMarker` at **index 0** ⇒ a brand-new prefix ⇒ the first post-compaction request is a **full cache bust regardless of the summary.** Tier-0 buys compaction *quality*, nothing for cache economics.

### Value as a minimal-viable v0

**Genuine tier-0 product.** It is low-risk, isolated to **one matchable request**, requires **no mid-turn reload**, and **validates the `ANTHROPIC_BASE_URL` proxy plumbing end-to-end** — exactly the harness the full continuous engine then reuses. It ships *before* the continuous engine and de-risks it.

*(Do not confuse with the SDK-native compaction beta — `betas:['compact-2026-01-12']`, `x-stainless-helper:'compaction'` @130753797. CC's actual auto/manual compaction does **not** use that server-side helper; it uses the `Vut`→`Sel`→`XH`/`odt` fork path. The matcher won't collide.)*

---

## 5. How these update the architecture

**Net effect: two new tiers, one dead end.**

```
cc-squash architecture (updated)
│
├─ TIER 0  "Better Compaction"  ........ summarization-response proxy   [Angle 3 — SHIP FIRST]
│     • intercept the one request carrying `CRITICAL: Respond with TEXT ONLY…`
│     • return a structured, constraint-preserving <summary>…</summary>
│     • BUYS: compaction quality; verbatim security/user-directive preservation
│     • DOES NOT: change messagesToKeep, remove C5, or preserve cache
│     • value: validates proxy plumbing end-to-end; reusable interception harness
│
├─ TIER 1  "Free-bust persistence"  .... cooperative reload            [Angle 1 — OPT-IN]
│     • cc-squash rewrites .jsonl in place (content-rewrite-in-place, keep uuid+parentUuid)
│     • user quits + `claude --resume <abs.jsonl>`  (NOT /fork)
│     • BUYS: smaller go-forward context at a cache bust that happens anyway (free)
│     • DOES NOT: preserve a warm prefix (cold loader re-sends everything)
│     • value: zero-marginal-cost context reduction; idempotent loop; loud-fail safety
│
├─ TIER 2  "Continuous engine"  ........ streaming proxy, full         [SETTLED — load-bearing]
│     • live per-segment rewrite + cache-aware breakpoint management (MTm cache_control)
│     • the ONLY mechanism for mid-session, cache-preserving squash
│     • C5 + messagesToKeep control come "for free" here (rewrites the assembled stream)
│     • built on TIER-0's interception harness + structured-summary builder
│
└─ DEAD END  in-memory manipulation  ... [Angle 2 — REMOVED]
      • anti-debug kill-switch `if(Yqm())process.exit(1)`; no BUN_INSPECT; no signal/IPC verb;
        execRelaunch forks cold and exits the parent. No route survives. Do not pursue.
```

Key reframings the binary forces:

- **Cooperative reload is a context lever, not a cache lever.** Because the reload is the cold loader path, the squash is "free" but only shrinks go-forward tokens. It is a *persistence/context* tier, never warm-cache economics.
- **Summary-proxy is tier-0 of the same proxy, not a separate product.** It ships first precisely because it is the smallest slice of the settled architecture and de-risks the plumbing.
- **The C5 directive and `messagesToKeep` are out of reach until Tier 2.** They are added/owned CC-side around the summary; only the engine that rewrites the *assembled* stream can touch them.

---

## 6. Recommended experiments (ordered by value)

All are read-only / near-zero credits unless noted. None attach a debugger to a live CC, send signals to a live CC, or mutate the binary.

| # | Experiment | Method | Success criterion | Cost |
|---|-----------|--------|-------------------|------|
| **E1** | **Compaction matchability** (Angle 3, highest value — unblocks tier-0) | Passthrough logging proxy at `ANTHROPIC_BASE_URL` that forwards every `/v1/messages?beta=true` upstream and tees request bodies to disk. Run one real CC session; trigger `/compact`. | Exactly **one request per compaction** carries `CRITICAL: Respond with TEXT ONLY`, it sits in the **last user message**; `metadata.user_id` is byte-identical to non-compaction calls; `max_tokens <= 20000`; `tool_choice` absent. Record which branch fired (system == "helpful AI assistant…" ⇒ fallback; full system ⇒ default). | ~minimal (1 real compaction's API spend) |
| **E2** | **Discriminator robustness** (Angle 3) | From E1's capture, scan all bodies. | **Zero false positives** (no non-compaction request contains the marker) and **zero false negatives** (every compaction attempt, including PTL retries, carries it). Confirm the SDK-native `x-stainless-helper: compaction` path does **not** appear in CC's normal flow. | ~0 (reuses E1 capture) |
| **E3** | **`--resume` cold-load-edit primitive** (Angle 1, highest value — unblocks tier-1) | From a short scripted/`-p` session, produce a `.jsonl`. With the process exited, programmatically (a) rewrite an existing message's content keeping `uuid`+`parentUuid`, (b) insert a new-uuid record with a fixed parent chain, (c) delete a record re-stitching `parentUuid`. Resume each via `claude --resume <abs.jsonl>`; dump the rebuilt first/last messages via a SessionStart hook **before** any API call (or point `ANTHROPIC_BASE_URL` at a stub). | Rewrite **+ insert + deletion all present**; `tengu_session_resumed{entrypoint:'file',success:true}`. Orphaning the leaf ⇒ `no_chain` + "Unable to load transcript from file" + exit; emptying messages ⇒ `no_messages`. | ~0 (stub upstream) |
| **E4** | **Response-shape synthesis** (Angle 3) | Interceptor short-circuits a matched request (never forwards upstream) and emits a fabricated SSE stream spelling `<summary>…structured WorkingState…</summary>`. Feed to CC. | No "check for a proxy or gateway" error; post-compaction in-memory message carries `isCompactSummary:true` with our text inside the "This session is being continued…" wrapper; `gkd` lifts the `<summary>` inner content. | ~0 (mock, no upstream) |
| **E5** | **C5 + cache negative controls** (Angle 3 — proves the ceiling) | After a synthesized compaction, inspect the assembled conversation and the first post-compaction `usage`. | The "Continue … without asking the user any further questions" directive is **still present** (proves `UOt` adds it independently); `cache_read_input_tokens` ≈ 0 / `cache_creation` dominates (proves `boundaryMarker`-at-0 busts cache). | ~1 post-compaction request |
| **E6** | **`/fork` UUID-intersection rule** (Angle 1) | In a live session, externally edit the `.jsonl` three ways (in-place rewrite; new-uuid insert; deletion), then run `/fork`. | **Only** the in-place rewrite survives (with rewritten content); the insert is **dropped**; deletion lands only if that uuid also left the live array; fork mints a new sessionId + new `.jsonl` path + branch banner. Pins why `--resume` > `/fork`. | ~0 |
| **E7** | **Broken-`parentUuid` tolerance & 5 MiB skip** (Angle 1) | (a) Break one intermediate `parentUuid` (parent still present by timestamp) vs. orphan the leaf; resume each. (b) Build a >5 MiB `.jsonl` with a `compact_boundary`, edit a record before it, resume with/without `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP=1`. | (a) Single broken link loads with `tengu_chain_timestamp_fallback`; orphaned leaf ⇒ `no_chain`. (b) Pre-boundary edit **skipped** without the flag, **picked up** with it. Establishes the edit-integrity envelope. | ~0 |
| **E8** | **Anti-debug kill-switch on a sacrificial binary** (Angle 2 — confirm the dead end, never on CC) | `bun build --compile` a 2-line throwaway; run it under `--inspect` / `BUN_INSPECT=1` to demonstrate a compiled standalone does not open an inspector port the way `bun run` does. | Reproduces "no BUN_INSPECT in standalone" on a sacrificial binary; never touch CC. Closes Angle 2 by demonstration. | ~0 |

**Sequencing.** Run **E1 → E2 → E4 → E5** (the tier-0 critical path; ship "Better Compaction" once green) in parallel with **E3 → E6 → E7** (the tier-1 path). E8 is a one-off confirmation of the Angle-2 dead end and can run anytime. E1 and E3 are the two highest-value unblockers and should go first.
