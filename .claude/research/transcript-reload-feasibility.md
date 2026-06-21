# Transcript-Reload & Compaction-Replace Feasibility — CC 2.1.183

**Decision memo for cc-squash.** Can two *lightweight* mechanisms (cheaper than an
ANTHROPIC_BASE_URL proxy) deliver live, cache-economics-aware squashing of a running
Claude Code session? Verdicts are pinned against the carved binary
`/Users/yasyf/Code/cc-squash/.claude/research/extracted/2.1.183/bundle.js`
(GIT_SHA `9d251abdbce0c0a6190d290add83634e0ab481f6`, BUILD_TIME `2026-06-18T23:04:10Z`,
version literal `2.1.183` embedded at the request assembler, byte 139779034).

Every load-bearing claim carries a confidence tag:
`confirmed-from-binary` / `confirmed-from-docs` / `inferred` / `unverified`.

---

## 1. The question

cc-squash wants to rewrite the live message stream CC sends to the API — strategic
per-segment removal / rewriting / structured-references — weighed against the cost of
busting Anthropic's prompt cache (editing any token invalidates the cache from that
point to the end of the prompt). An API proxy (`ANTHROPIC_BASE_URL`) is the heavyweight
option that gives full control of the wire stream. This memo chases two lighter
mechanisms:

- **(1) Transcript-file rewrite + force reload.** cc-squash edits the on-disk
  `.jsonl` transcript, then forces CC to rebuild its conversation from that file
  *mid-session*. Why it would beat a proxy: zero network interposition, no TLS/cost
  plumbing — just a file edit plus a trigger. **The decisive unknown:** does any
  in-session command or signal (`/rewind`, `/fork`, `/resume`, `/clear`, `/compact`,
  a signal, a file-watcher) make CC re-read the `.jsonl` and rebuild its in-memory
  message array, so our edits land? Or does CC hold the conversation in memory and
  only re-read at boundaries (startup / `--resume` / `--continue`)?

- **(2) Compaction-replace.** Trigger CC's native compaction but run *our* logic
  *instead of* CC's summarizer, so our output *becomes* the replacement conversation.
  Why it would beat a proxy on cache: native compaction replaces the *whole* history
  with a summary → total prefix bust from message 0. If *we* control the replacement,
  we keep the early prefix byte-identical and rewrite only the tail → the prompt cache
  survives up to the divergence point. **The decisive unknown:** can a PreCompact hook
  (or any surface) *substitute* the compacted history verbatim with our content (not
  merely augment it)? The env var `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP` *sounds* like
  it might let PreCompact skip/replace something — pin down exactly what.

**Bottom line up front.** Both lightweight mechanisms are **DEAD** against CC 2.1.183.
The conversation that builds the next API request lives entirely in an in-memory React
array; the `.jsonl` is a one-directional write-mirror, re-read only at load boundaries
(startup / `--resume` / `--continue` / `/fork` / SDK), never in place mid-turn. And the
PreCompact hook can only *steer* or *block* native compaction — never substitute its
output, which is sourced exclusively from the model's summarization API call. The
prefix-preserving, cache-aware squash cc-squash wants requires **owning the outbound
message stream** — the API proxy is the load-bearing mechanism. (Two consolation
prizes for the proxy design are surfaced in §4.)

---

## 2. Can we force a mid-session reload from disk?

**No.** No in-session command or OS signal makes a live interactive CC rebuild its
conversation by re-reading the on-disk `.jsonl`. `[confirmed-from-binary]`

### Why: the request is a pure function of the in-memory array

The next API request is assembled by `v6(e)` (byte **139779034**). It destructures
`messages: r` from its argument — the **in-memory array** — and maps `cache_control`
onto each block before building the HTTP body `{model, max_tokens, system, messages, tools, …}`.
There is **zero** `readFile`/`createReadStream` anywhere in `v6`. The transcript is
written downstream of the in-memory loop (`insertMessageChain`/`Ece` →
`appendEntry` → `createWriteStream`/`appendFileSync`) and never read back within a turn.
`[confirmed-from-binary]` (Angles C, B)

The dispatch that *chooses* memory vs. disk makes this explicit:
`Ppm(e,t) → if(t?.sessionStore) return Vpm(...) else return e0l(...)` (byte
**144104874**). The disk loader (`e0l → ube → Qdm → ZIl(Zdm())`, equivalently `nce`
at byte **144497154**) is the **fallback arm**, taken only when no live `sessionStore`
exists — i.e. startup, `--resume`, `--continue`, SDK `getMessages`. A live interactive
session always has a populated `sessionStore`, so it never reaches the disk loader.
`[confirmed-from-binary]` (Angles B, C)

There is **no file-watcher on the transcript.** The only `watchFile` usage watches git
metadata (`HEAD`, `config`, `refs/heads/<branch>`) for the branch indicator (class
`Ies`, byte **132143313**); chokidar is vendored but never pointed at a `.jsonl`. CC
tails its own transcript with an incremental write-cursor (`prevOffset` /
`resetSessionFilePointer`), never a watcher. No `SIGCONT`/`SIGWINCH`/`SIGHUP`/`SIGUSR`
handler re-reads the transcript. `[confirmed-from-binary]` (Angles A, B)

### Per-command verdicts (ordered: best lightweight candidate first)

There is **no clean "refresh the current conversation from disk" primitive.** The
closest lever is `/fork`, which *does* re-read disk — but it switches session identity
and drops anything not already in the in-memory UUID set (see below). Ranked by how
close each gets:

| Surface | Re-reads `.jsonl`? | Effect on the **current** live array | Verdict | Confidence |
|---|---|---|---|---|
| **`/fork`** (`branchAndResume` `Qrl` → `createFork` `Xrl` → `e.resume(s,f,'fork')`, byte 142183586) | **Yes** — `createReadStream` + `readline` over the source file | Builds a **new** session (`serializedMessages`) and switches onto it via `e.resume`. **But** `createFork` keeps a disk line `S` only `if (b6(S) && !S.isSidechain && p.has(S.uuid))` where `p=new Set(e.messages.map(T=>T.uuid))` — it *intersects* with the in-memory UUID set and copies record **content verbatim** from the file. | **Edits land only if you preserve UUIDs.** UUID-preserving content rewrites survive a fork; newly-inserted UUIDs are dropped, and pure-disk deletions land only if the UUID also left the in-memory chain. New `sessionId`/file; user-visible "you are now in the new branch"; cannot be invoked headlessly (interactive command/keybinding). Not a transparent live swap. | `confirmed-from-binary` |
| **`/rewind`** (aliases `checkpoint`, `undo`; handler `e7t`, byte 147807831) | **No** | Pure in-memory slice: `In=Tc.current; mo=In.lastIndexOf(St); wo=In.slice(0,mo); Ll(wo)` (setMessages). Telemetry `tengu_conversation_rewind{preRewindMessageCount,messagesRemoved,rewindToMessageIndex}` confirms index-based array truncation. File restore (if chosen) comes from a **separate** backup store at `<configDir>/file-history/<session>/<sha256@vN>` keyed by message UUID — **not** the transcript. **Your `.jsonl` edits are ignored.** `supportsNonInteractive:!1` → opens an interactive overlay (`open_message_selector`), selection returns via a separate channel. | **DEAD** for the transcript-rewrite plan. (One caveat: the "Restore **and fork**" variant routes through the fork path above and *would* re-read disk; plain "Restore" is in-memory-only. See Experiment E5.) | `confirmed-from-binary` |
| **`/clear`** (aliases `reset`, `new`) | **No** | Mounts a **fresh empty** conversation under a new id (`{type:'conversation_reset',newConversationId:randomUUID()}`), runs SessionStart(`clear`), re-seeds CLAUDE.md. The old session is only resumable via `/resume` (separate-process flow) — never reloaded in place. | **DEAD** — only ever yields an empty transcript. | `confirmed-from-binary` |
| **`/branch`** | No | Creates a separate conversation branch at this point — snapshots/inherits current state into a new entity. Does not rebuild the live array from disk. | **DEAD** for live reload. | `confirmed-from-binary` |
| **`/resume`** (alias `continue`) | **Yes** | Reads messages from disk (`Bge`/`Ate`) and calls `t.resume?.(sessionId,messages,source)` — but it **switches to a (possibly different) conversation**. To reload *your edited current* transcript you would re-open it as a resume = the cold-load path, not a live in-array refresh. | Not a clean "refresh current conversation" primitive (it's a switch). | `confirmed-from-binary` |
| **`/compact`** | No (it summarizes, doesn't reload) | Runs CC's native summarizer → `{type:'compact',compactionResult}`. This is the behavior mechanism (2) wants to *replace*, not a reload. By itself it busts the prefix. | Not a reload. (Relevant to §3.) | `confirmed-from-binary` |
| **`execRelaunch`** (`xAp`, byte 139491693) | **Yes** (via the new process) | Spawns a fresh `claude` with inherited argv (`spawn(cmd,[...prefixArgs,...process.argv.slice(2)],{stdio:'inherit'})`) and exits the old process — a true **cold restart**. So `--resume`/`--continue` carry over and the edited file is re-read by `loadTranscriptFromFile`. | **WORKS — but it is a hard reboot**, not a live mid-session reload. Loses all TUI/in-memory state, screen, spinner. | `confirmed-from-binary` |
| **OS signals / focus / resume / IPC** | No | `SIGWINCH` = resize/redraw; `SIGHUP`/`SIGCONT` = background-PTY bridge (`onResume(G){l=G}` just flushes buffered keystrokes). None re-read the `.jsonl`. No IPC reload signal exists. | **DEAD.** | `confirmed-from-binary` |
| **Transcript file-watcher** | N/A | Absent. `watchFile` is git-only; chokidar is never pointed at the transcript. | **DEAD** — external edits are never auto-detected. | `confirmed-from-binary` |

### The single disk→conversation rebuild path

The one function that rebuilds the conversation array from a `.jsonl` is
`loadTranscriptFromFile` (`sHo`) / its core parser `nce` (byte **144497154**). It
parses messages/summaries/`fileHistorySnapshots`/`leafUuids`, applies `parentUuid`
tombstone rewrites, accumulates `content-replacement` records, stitches the
`parentUuid` chain across `compact_boundary` (`epm`, via
`compactMetadata.preservedMessages{uuids,anchorUuid}` /
`preservedSegment{headUuid,anchorUuid,tailUuid}`), and walks the leaf→root chain to
produce the ordered list. Its callers cluster at the session-list / resume / history UI
(bytes 131126679+) and the CLI `--resume <abs-path.jsonl>` startup branch (guarded by
`isTranscriptFileResumeArg`/`IKn`; emits `tengu_session_resumed{entrypoint:'file'}`).
**There is no mid-session caller on the active session id.** `[confirmed-from-binary]`
(Angles A, C)

The cold-load path validates a conversation chain and **throws** (`jne`
`'no_chain'`/`'no_messages'`) if an edit breaks the parent/uuid chain. Any disk-rewrite
strategy — even the cold one — must preserve a valid leaf-uuid chain. `[confirmed-from-binary]`

**Confidence note (escalated to an experiment):** the four investigations agree the
disk loader is unreachable mid-session, evidenced by the `Ppm` `sessionStore`-vs-`e0l`
dispatch and an exhaustive read of watch/signal call sites. This is **strongly**
confirmed-from-binary, but "exhaustive absence" of a reload trigger is inherently a
negative claim — Experiment **E5** (enumerate every `nce`/`e0l`/`Ppm` caller and prove
none fires from an interactive keypress/hook/signal while a `sessionStore` exists) is
the belt-and-suspenders pass before the design fully commits.

---

## 3. Can we replace the compaction output verbatim?

**No.** A PreCompact hook (or any surface) **cannot substitute** CC's compacted
history. It can only (a) *steer* the model's summary via prompt injection, or (b)
*block* compaction entirely. `[confirmed-from-binary]` (Angle D)

### The compacted history is assembled from exactly two content slots

`c_e(e) = [e.boundaryMarker, ...e.summaryMessages, ...e.messagesToKeep, ...e.attachments, ...e.hookResults]`
(byte **141856061**, verified directly). This is the *only* assembler of the
replacement history. `summaryMessages` and `messagesToKeep` are the only content slots;
**neither is sourced from hook output.** `[confirmed-from-binary]`

- `summaryMessages` is built in **exactly one place** (`_kd`) from the model's
  summarization API response: `let l=HRn(i.messages); … UOt(l,…,{isCompactSummary:!0})`,
  where `HRn(i.messages)` extracts the assistant text returned by the summarizer call
  `XH({…querySource:'compact'})`. No hook value flows into it. `[confirmed-from-binary]`
- `messagesToKeep` is the **verbatim preserved tail** — native compaction *already*
  keeps a tail (`WAo` records `preservedSegment{headUuid,anchorUuid,tailUuid}` over the
  kept messages, byte 141856248). But everything **before** the boundary is replaced by
  the single summary message, so the byte prefix from message 0 is **always destroyed**.
  `[confirmed-from-binary]`

### What the PreCompact hook can and cannot do

The PreCompact dispatcher `lJ({trigger,customInstructions},signal)` returns **only**
`{newCustomInstructions, userDisplayMessage, blockedBy}` (byte ~141855865). There is no
`summary`/`replacementMessages`/`compactedHistory` field. `[confirmed-from-binary]`

- **Steer (prompt injection only):** `newCustomInstructions` = concatenated trimmed
  stdout of succeeded non-blocked hooks. It is consumed in exactly 3 places, and every
  one feeds it into the **summarizer prompt** (`o=qho(o,_.newCustomInstructions)` →
  `IRn(n)` prompt text in `_kd`), never the output. So a PreCompact hook can only steer
  the model's summary, never *be* it. `[confirmed-from-binary]`
- **Block (abort only):** a hook that exits non-zero / `blocked` populates `blockedBy`;
  `w6n` then `throw new j6("Compaction blocked by PreCompact hook: "+blockedBy)` and
  aborts the whole compaction. Blocking aborts; it never substitutes. `[confirmed-from-binary]`
- **No structured output:** the `hookSpecificOutput` discriminated union enumerates every
  hook event allowed to return structured JSON (PreToolUse, UserPromptSubmit,
  SessionStart, Setup, Stop, … — 20 events, bytes ~139634481–139639248). **PreCompact
  and PostCompact are absent.** A PreCompact hook cannot return
  `additionalContext`/`decision`/`hookSpecificOutput` at all. `[confirmed-from-binary]`
- **PostCompact** (`R0e`) fires *after* the summary exists, receives the finished
  `compact_summary` text, and returns only `{userDisplayMessage}` — a pure notifier.
  `[confirmed-from-binary]`

This is consistent with the prior finding that **PreCompact hook stdout is dropped**
(debug log only) for context purposes — only SessionStart/UserPromptSubmit stdout
reaches model context. `[confirmed-from-binary, corroborated by prior research]`

### CRITICAL CORRECTION: `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP` is NOT a hook lever

Its name misleads. It is read **inside the transcript-file loader** (`Qdm`/`nce`),
gating a large-file read optimization — *not* the PreCompact hook. Verified directly:

```
async function Qdm(e,t){
  try{
    if(t>dbe && !st(process.env.CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP))
      return (await dtn(e,t)).postBoundaryBuf;   // streaming byte-offset reader, post-boundary only
    return await XIl.readFile(e);                 // full read
  }catch{return null}
}
```

`dbe = 5242880` (5 MiB). With the flag **unset** (default) and file size > 5 MiB, CC
uses the streaming reverse-scanner (`dtn`/`i_m`) that finds the last `compact_boundary`
and parses only the **post-boundary tail**, skipping the dead pre-compaction prefix.
Setting the flag forces the full `readFile`+parse. So "precompact skip" = a
**transcript-LOAD optimization** (don't re-parse the dead pre-compact prefix), with
**nothing** to do with substituting PreCompact hook output for the summary. Do not
over-read the name. `[confirmed-from-binary]` (Angles B, C, D — unanimous)

### Disable-and-own native compaction (and the cache implication)

cc-squash *can* fully prevent CC's prefix-busting compaction via env flags:

- `DISABLE_AUTO_COMPACT=1` — gate `Yw()` stops the auto threshold loop (`Eho`/`l3p`)
  **and** the reactive 413 path (`qAo`, which requires `Yw()`). CC will never auto-fire
  compaction. `[confirmed-from-binary]`
- `DISABLE_COMPACT=1` — broader: additionally kills the manual `/compact`
  (`isEnabled:()=>!je.DISABLE_COMPACT`) and short-circuits `Eho`
  (`if(je.DISABLE_COMPACT)return{wasCompacted:!1}`). Also repurposes
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS` as the window when set. `[confirmed-from-binary]`

**But CC exposes no in-process callback fired at the would-be compaction point.** So
even after disabling native compaction, cc-squash must **re-implement its own
token-threshold detection** (watch transcript size / usage, or — better — observe it at
the proxy) to know *when* to squash, and must **own the outbound stream** to apply the
prefix-preserving rewrite. `[confirmed-from-binary + inferred]`

**Cache implication, spelled out:** native compaction is structurally incapable of
preserving the prompt cache — it always prepends one summary message before the kept
tail, so the byte prefix from message 0 is rewritten and the cache is busted at the next
request, regardless of hooks. The "keep the early prefix byte-identical, rewrite only
the tail" economics is **only** achievable by intercepting the outbound message stream
itself (the proxy). `[confirmed-from-binary]`

---

## 4. Recommended mechanism

**The findings collapse the proxy-vs-lightweight calculus decisively toward the proxy.**
Both lightweight mechanisms are dead for *live* squashing:

- Mechanism (1) — live `.jsonl` rewrite + force reload — **cannot take effect live**.
  Every in-session surface operates on the in-memory array (`Tc.current` ←
  `sessionStore`) or a separate file-history backup tree; the disk loader is reachable
  only at cold boundaries. Disk edits land only across a **process restart**
  (`execRelaunch` or fresh `--resume <abs.jsonl>`), which loses TUI state and requires
  a valid leaf-uuid chain. The single partial live exception — `/fork` — switches
  session identity, drops non-UUID-preserved edits, and can't be driven headlessly.
- Mechanism (2) — compaction-replace — **cannot substitute** the summary. The PreCompact
  hook only steers or blocks; `summaryMessages` comes solely from the model's API call;
  `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP` is a loader knob, not a hook lever. Native
  compaction always busts the prefix.

**Recommendation, in priority order:**

1. **API proxy (`ANTHROPIC_BASE_URL`) — the load-bearing mechanism.** It is the *only*
   way to rewrite the live wire stream with full control of the cache-divergence point:
   keep the prefix byte-identical, rewrite only the tail, and reorder/remove/restructure
   segments with cache-aware economics. The "heavyweight" label is the price of the only
   surface that actually works live. Pair it with `DISABLE_AUTO_COMPACT=1` (and likely
   `DISABLE_COMPACT=1`) so CC never injects its own prefix-busting summary underneath us,
   and run cc-squash's threshold detection on the traffic the proxy already sees.

2. **Ride CC's own native rewrite primitives via the proxy / persisted records (strong
   consolation prize, surfaced by Angles B & C).** CC *already* does live, persisted,
   cache-economical content rewriting:
   - **`content-replacement`** transcript records
     (`{type:'content-replacement',sessionId,agentId,replacements:[{tool_use_id,content}]}`;
     writer `insertContentReplacement` byte ~144461611; applier `B$d` swaps a
     `tool_result`'s content **by `tool_use_id`** byte ~137627496; selection `N$d`
     targets the largest results to a token budget; gated by feature flag
     `tengu_hawthorn_steeple`). It rewrites *only* targeted blocks **in the in-memory
     array** while leaving every other message object byte-identical, and re-applies on
     reload via `nce`. **This is the exact "keep the early prefix, rewrite the tail"
     economics cc-squash is chasing** — the prompt cache survives up to the first
     replaced block. `[confirmed-from-binary]`
   - **`tombstone`** QueryEvent — a message-removal primitive. `[confirmed-from-docs]`
   - **`compact_boundary` parent-chain stitching** (`epm`) — CC keeps the early prefix
     and re-parents the tail across a boundary, so a custom replacement that keeps the
     prefix byte-identical is structurally compatible with CC's own loader.
     `[confirmed-from-binary]`

   **Caveat:** `content-replacement` expresses per-`tool_use_id` swaps on `tool_result`
   content — it does **not** natively express arbitrary user/assistant *text* rewrites,
   and writing these records is internal API. So it's a powerful *complement* to the
   proxy (cheap, native, cache-preserving for the common "giant tool output" case), not
   a full replacement for it. It is also flag-gated and may be off by default.

3. **PreCompact-blocked, cold-reload fallback (coarse).** If a hard reboot is ever
   acceptable: block native compaction (PreCompact `blocked`) or disable it, rewrite the
   `.jsonl` preserving the leaf-uuid chain, then `execRelaunch` / `--resume`. Accept the
   TUI reset. This is the only path that lets a pure file-rewrite strategy land at all,
   but it is not seamless and not live.

**Decisive call:** build cc-squash around the **API proxy** as the primary live
mechanism, with **`content-replacement`-style records as a cache-preserving
complement** for tool-output-heavy segments, and treat the cold-reload path as a coarse
fallback only. Do **not** invest in `/rewind`-based or PreCompact-substitution designs —
the binary forecloses both.

---

## 5. Feasibility experiments to confirm

Ordered by value. All are read-only / zero-API-credit unless noted. (The
transcript-rewrite-then-`/rewind` test is *not* first, because the binary already shows
`/rewind` is in-memory-only with high confidence — E2 is a cheap behavioral
*confirmation*, not a live candidate.)

**E1 — Prove the cold-reload-edit primitive end-to-end (cheapest, highest value).**
*Method:* (a) produce a transcript `.jsonl` from a short scripted/`-p` session; (b) with
the process exited, programmatically edit the tail messages, **preserving every
`parentUuid` link and keeping types in `{user,assistant,progress,system,attachment}`**;
(c) resume with `claude --resume <abs.jsonl>` and dump the first/last few message
contents via a **SessionStart/UserPromptSubmit hook** (whose stdout reaches context) —
*before* any API call. *Success:* the edited tail is present and
`tengu_session_resumed{entrypoint:'file',success:true}` fires; deliberately breaking the
chain triggers the `jne('no_chain'/'no_messages')` guard. *Cost:* ~0 credits (inspect
the rebuilt array via the hook, or point `ANTHROPIC_BASE_URL` at a stub). This isolates
"do edits land on cold load" from "can we force it live" and validates the only
supported file-rewrite land path.

**E2 — Confirm `/rewind` is in-memory-only behaviorally.** *Method:* in a live session,
externally append a sentinel assistant message to the active `.jsonl`, then invoke
`/rewind` and select a point. *Success:* the sentinel **never** appears, and the
post-rewind message count equals the in-memory slice index (matches
`rewindToMessageIndex`); CC's next `recordTranscript` append reflects the *unedited*
in-memory state, overwriting the divergence. *Cost:* ~0 new API turns for the rewind
itself. Confirms `/rewind` ignores disk.

**E3 — Validate `content-replacement` as the cc-squash cache-preserving primitive.**
*Method:* trip CC's own tool-result threshold (force a large tool output so CC writes a
`content-replacement` entry — or enable the `tengu_hawthorn_steeple` gate if exposed),
then `--resume` and confirm the big `tool_result` is replaced by the stub on reload
(`B$d` applied). Then test the inverse: **author** a record directly on disk in the same
shape (`{type:'content-replacement',sessionId,agentId,replacements:[{tool_use_id,content}]}`)
and confirm CC re-applies it on reload, **with the prefix bytes before the first
replaced block unchanged**. *Success:* cc-squash can ride `content-replacement` records
to rewrite chosen segments while preserving the cache. *Cost:* ~0 credits (reload +
inspect). Also dump a real hawthorn-compacted transcript from older/large sessions under
`~/.cc-pool` and diff the message before/after via the `B$d` reducer to nail the exact
schema.

**E4 — Confirm the `DISABLE_PRECOMPACT_SKIP` / 5 MiB threshold semantics (static
harness).** *Method:* build a >5 MiB synthetic `.jsonl` with a `compact_boundary` near
the end; over the carved `Qdm`/`dtn`/`i_m`/`nce`, assert the streaming reader returns
only the post-boundary tail, and that `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP=1` forces the
full `readFile` path. *Success:* confirms it is a load-time read strategy, not a hook
lever. *Cost:* 0 (static/unit over the de-minified slice).

**E5 — Static exhaustiveness pass: no live disk-reload trigger exists.** *Method:*
enumerate every caller of `nce`/`e0l`/`Ppm` (bytes 131126679–131947860 + the
`--resume`/`conversation_reset`/`resume` codepaths) and classify each by whether the
session id passed is `kt()` (current) or a selected historical one, and whether it can
fire from an interactive keypress / hook / signal while a `sessionStore` exists.
Separately, `rg -a -b -o '\.watch(File)?\([^)]{0,80}' bundle.js` and confirm every watch
target is a config/CLAUDE.md/git/IDE-lock path, never the session `.jsonl`. *Success:*
no in-place live reload caller on the active session id. *Cost:* 0. The belt-and-
suspenders check before committing the design to the proxy route.

**E6 — (only if a relaunch UX is acceptable) Verify `execRelaunch` as a forcing
function.** *Method:* trigger a relaunch-bearing flow (e.g. a setup command path) or
re-exec, and confirm the child re-reads the edited `.jsonl`. *Success:* establishes the
"hard reboot to reload" fallback. *Cost:* ~0 credits; drops TUI state — skip if seamless
live reload is a hard requirement.

---

### Appendix — key handler offsets (CC 2.1.183, `bundle.js`)

| Symbol | Role | Byte |
|---|---|---|
| `v6` | API request assembler (in-memory `messages` param, no file read) | 139779034 |
| `Ppm` | dispatch: `sessionStore` (live) vs `e0l` (disk fallback) | 144104874 |
| `nce` / `e0l` | the **only** disk→conversation rebuild (load boundaries only) | 144497154 / 144060225 |
| `sHo` (`loadTranscriptFromFile`) | CLI `--resume` parse (guarded by `IKn`) | def @144480101; `--resume` caller @148226680 |
| `Qdm` | transcript read; `DISABLE_PRECOMPACT_SKIP` byte-skip (`dbe`=5 MiB) | (within nce region); `dbe` @132089098 |
| `e7t` | `/rewind` handler: `Tc.current.slice` + `Ll(setMessages)`, **no fs read** | 147807831 |
| `fPo` (`MessageSelector`) | rewind picker; fed `messages:tl` (in-memory) | comp @145355024; instantiation @147829346 |
| `Xrl`/`Qrl` (`createFork`/`branchAndResume`) | `/fork`: re-reads disk, intersects in-memory UUID set, `e.resume` | 142183586 |
| `c_e` | post-compaction assembler `[boundary,...summary,...keep,...attach,...hookResults]` | 141856061 |
| `_kd` | builds `summaryMessages` from model API response (`HRn`) only | compaction.js:3188 |
| `lJ` | PreCompact dispatcher → `{newCustomInstructions,userDisplayMessage,blockedBy}` | ~141855865 |
| `w6n` | throws on `blockedBy` (PreCompact block) | 141855730 |
| `hookSpecificOutput` union | 20 events, **PreCompact/PostCompact absent** | 139634481–139639248 |
| `Yw` | auto/reactive compaction gate (`DISABLE_AUTO_COMPACT`/`DISABLE_COMPACT`) | compaction.js:748 |
| `insertContentReplacement` / `B$d`/`N$d` | native per-`tool_use_id` content rewrite (`tengu_hawthorn_steeple`) | 144461611 / 137627496 |
| `WAo` | preserved-tail builder (`preservedSegment`/`preservedMessages`) | 141856248 |
| `epm` | `compact_boundary` parent-chain stitching on load | 144056210 region |
| `xAp` (`execRelaunch`) | full cold restart (respawn `claude` w/ inherited argv) | 139491693 |
| `Ies` | git-only `watchFile` (HEAD/config/refs) — **no transcript watcher** | 132143313 |
