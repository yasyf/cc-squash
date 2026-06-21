# Claude Code Compaction Deep Dive — Design Input for `cc-squash`

> **Status:** INTERNAL design brief. Synthesizes five research strands (S1 reverse-engineered CC internals; S2 third-party tools + Anthropic baselines; S3 corroborated user complaints; S4 academic/industry compression prior art; S5 bioqa transferable subsystems). Every substantive claim cites a finding `global_id` (e.g. `[S1-F2]`, `[S4:llmlingua2]`, `[S5-F3]`). Confidence tags are preserved verbatim from the source strands and never upgraded.
>
> **Confidence vocabulary:** S1/S4/S5 use `high` / `confirmed`. S2/S3 use `confirmed-stable` (cross-version/docs-anchored), `confirmed-single-version` (one snapshot only), `inferred` (README-level, not code-verified), `unverified`, `high`/`medium`/`low`. A finding tagged anything below `confirmed-stable`/`high` is **not** a settled fact and is flagged inline.

---

## Table of Contents

0. [Executive Summary](#0-executive-summary)
1. [How Claude Code Compaction Works Today](#1-how-claude-code-compaction-works-today)
2. [What Is Broken — Complaint Clusters C1–C5](#2-what-is-broken--complaint-clusters-c1c5)
3. [What Others Do](#3-what-others-do)
4. [What We Should Build (`cc-squash`)](#4-what-we-should-build-cc-squash)
5. [Open Questions & Runtime-Confirm Backlog](#5-open-questions--runtime-confirm-backlog)

---

## 0. Executive Summary

- **The algorithm in one breath.** CC gates auto-compaction through `Yw()` (enabled?) → `l3p()` (eligible?) → the async master loop `Eho()`, which fires when live tokens cross `compactThreshold = effective_window − 13000`, then either runs the **reactive** path (`Hqn`/`DRn`/`_kd`) or the **classic** path (`Vut`). Both replace the live message history with a single LLM-written prose summary (`isCompactSummary:true`) and write an on-disk `compact_boundary` marker the transcript loader resumes from `[S1-F1, S1-F2, S1-F6, S1-F8, S1-F12]`.
- **The core defect.** Compaction is one lossy prose pass over the whole conversation. It carries **no schema** that privileges live user constraints, the planned-vs-implemented distinction, or the *paths* of system-injected context (CLAUDE.md, Skill files) — which live outside the conversation being summarized and are therefore never re-fetched `[cross-link C1; S3-C1, S3-F13919-F1]`.
- **The highest-leverage actionable fix (C5).** After compaction CC injects a fixed continuation directive — *"Please continue the conversation from where we left it off without asking the user any further questions. Continue with the last task that you were asked to work on."* — which overrides user `plan-then-approve` constraints in CLAUDE.md and makes a freshly-amnesic agent barrel into action `[S3-C5, S3-WF-REDDIT-1piny6t-A]`. Inverting this directive ("confirm current status before acting") via a `SessionStart(matcher:'compact')` hook is the cheapest, biggest win.
- **The architectural trap to avoid.** `PreCompact` hook **stdout is dropped** — written to the debug log, never injected into the recovered model context. Stdout reaches context only for `SessionStart`, `UserPromptSubmit`, `UserPromptExpansion`. The published `mvara-ai/precompact-hook` is non-functional precisely because it assumes the wrong injection point `[S2-F3, S2-F2]`. The correct re-injection surface is `SessionStart(matcher:'compact')` `[S2-F8]`.
- **What transfers from prior art.** The single best-fit algorithm is **Rsum** recursive summary carry-forward (applicability 3/3) `[S4:F1-Rsum]`; the best-fit *engineering* prior art is **bioqa's escalating lossy ladder** + per-message keep/truncate/summarize decision + dedup-with-backref (file:line verified, "keep"/"adapt" verdicts) `[S5-F3, S5-F4, S5-F5]`. KV-eviction methods (H2O/SnapKV/StreamingLLM/…) are a **documented negative result** — they need GPU KV-tensor access cc-squash does not have `[S4:KV_EVICTION_NEGATIVE_RESULT]`.
- **What `cc-squash` is *not*.** It does not reimplement summarization. CC's built-in compaction (and Anthropic's server-side Compaction) is the baseline that actually shrinks context; cc-squash improves **what survives** and **re-injects it correctly** `[S2-F11, S2-F10]`.
- **Forgetting is the thinnest-prior-art requirement.** Losslessly preserving live constraints while dropping superseded ones at a single boundary has only three concrete leads: MemoryBank Ebbinghaus decay, Mem0 `DELETE`, and Zep bi-temporal edge invalidation `[S4:forgetting_mechanisms, OQ-12]`.
- **The testing constraint.** Auto-compaction **does not fire in non-interactive `-p`/print/SDK mode** (forced to a 2000-token threshold, peak hit 103,984 tokens — ~52× — with zero compaction) `[S1-F16]`. cc-squash cannot exercise the live end-to-end sequence in CI through print mode; it must unit-test its hook payloads + state model and reserve the live sequence for an interactive TTY harness.

---

## 1. How Claude Code Compaction Works Today

> Source: S1, a byte-precise carve of **v2.1.183** corroborated against v2.1.181 / v2.1.179, plus an empirical dynamic probe. All function names (`Yw`, `Eho`, `J$`, `Swn`, …) are the minified identifiers from the carve. Confidence on all S1 findings is `high` unless a per-item downgrade is noted.

### 1.1 Faithful pseudocode of the subsystem

The authoritative trace assembled from verified function bodies `[S1-F15]`:

```text
# ---- enable / window resolution ----
function autoCompactEnabled():            # Yw
    if env.DISABLE_COMPACT: return False
    if env.DISABLE_AUTO_COMPACT: return False
    return settings.autoCompactEnabled (default True)

function modelContextSize(model):         # nE -> vti
    if DISABLE_COMPACT and env.CLAUDE_CODE_MAX_CONTEXT_TOKENS>0: return it   # Cti
    if SLr(model): return 200000                                            # KQ
    if model is [1m]-tagged or (1M-beta-header and N8(model)) or jB(model): return 1_000_000
    if dynamic max known: return it
    return 200000                                                          # fkt
    # env.CLAUDE_CODE_DISABLE_1M_CONTEXT (vme) forces all 1M checks False

function resolveWindow(model, settingWindow):   # J$ -> {window, configured, source}
    modelMax = modelContextSize(model)
    if env.CLAUDE_CODE_AUTO_COMPACT_WINDOW valid: w=clamp(parsed,[1e5,1e6]); return (min(modelMax,w),'env')
    if settingWindow is not None:                                       return (min(modelMax,settingWindow),'settings')
    if clientdata(rowan_thicket) for model:                             return (...,'clientdata')
    if experiment tengu_amber_redwood2 and model=='claude-opus-4-8':    return (...,'experiment')
    if modelMax<1e6 and model in default-set:                          return (min(modelMax,200000),'model-default')
    return (min(modelMax, mFi[model] or modelMax),'auto')

function effectiveWindow(model, window):  # cee
    return resolveWindow(model, enabled?window:None).window - min(toolDefTokens, 20000)   # AFi

function compactThreshold(window, cfg):   # Swn
    base = window - 13000                                                 # lFi
    if cfg.testPctOverride in (0,100]: return min(floor(window*pct/100), base)   # CLAUDE_AUTOCOMPACT_PCT_OVERRIDE
    return base

function level(tokens, window, cfg, modelWindow):   # pFi
    compact_t = compactThreshold(window, cfg)
    base      = cfg.enabled ? compact_t : window
    warn_t    = base - 20000
    block_t   = cfg.testBlockingOverride or (modelWindow - 3000)          # cFi ; CLAUDE_CODE_BLOCKING_LIMIT_OVERRIDE
    if tokens >= block_t:                  return 'blocked'
    if cfg.enabled and tokens >= compact_t: return 'compact'
    if tokens >= warn_t:                   return 'warn'
    return 'ok'

function eligible(messages, model, window, querySource):   # l3p
    if querySource=='compact': return False
    if querySource in {prompt_suggestion, away_summary, agent_summary}: return False   # GRe
    if not autoCompactEnabled(): return False
    if reactiveGate() and not redwood3() and not windowIsExplicit(model,window): return False  # vz && !mq && !VRe
    return level(currentTokens, ...) in {'compact','blocked'}

function reactiveGate():   # vz
    if env.CLAUDE_CODE_REMOTE and not flag(tengu_reactive_compact_remote): return False
    return True

# ---- master auto loop (async generator) ----
generator autoCompact(messages, ctx, querySource, compactTracking, precomputeFn):   # Eho
    if env.DISABLE_COMPACT: return {wasCompacted:False}
    if compactTracking.consecutiveFailures >= 3: return {wasCompacted:False}          # jho circuit breaker
    model, window = ctx.options.mainLoopModel, ctx.options.autoCompactWindow
    if not eligible(messages, model, window, querySource): return {wasCompacted:False}
    overflow = fixedPrefixOverflow(messages, model, window)                            # a3p
    if overflow: emit tengu_auto_compact_prefix_overflow{...overflow, wouldHaveBlocked:True}
    if rapidRefillCount(compactTracking) >= 3: return {wasCompacted:False}             # kho/f6n thrash breaker
    thresholdSource = querySource
    if thresholdSource != 'auto':
        emit tengu_auto_compact_routed_reactive{thresholdSource}
        result = reactiveCompact(...)                                                  # Hqn via Ytt
    else:
        result = classicCompact(messages, ..., coldCompact=env.CLAUDE_CODE_COLD_COMPACT)  # Vut, Gho()
    on failure: failures+=1; if failures>=3: emit tengu_auto_compact_circuit_breaker
    on success: return {wasCompacted:True, compactionResult:result, consecutiveFailures:0}

# ---- reactive path ----
async reactiveCompact(args):   # Hqn
    if not qAo(args): return {result:None}
    emit tengu_reactive_compact_triggered{...}
    hook = await runPreCompactHook(trigger, customInstructions)                        # lJ
    if hook.blockedBy: emit compact_progress; return {hookBlocked:True}
    customInstructions = merge(customInstructions, hook.newCustomInstructions)
    swap = args.precomputed or await summarizeWithRetry(messages, ...)                 # DRn (or borrowed precompute)
    if swap is None: emit tengu_reactive_compact_failed{...}; return {result:None}
    emit compact_progress(compact_end) + sdk_status(null); postCleanup(...)            # nne
    return {result: applied summary message (isCompactSummary:True)}

async summarizeWithRetry(messages, ctx, opts):   # DRn
    groups = splitGroups(messages)                                                     # FOt
    if len(groups) < 2: return {ok:False, reason:'too_few_groups'}
    preserved = 1
    if opts.initialTokenGap and len(groups)>3: preserved = 1 + gapSeed(groupTokens,gap)  # C$i
    while preserved < len(groups):
        toSummarize = groups[: len-preserved]; toPreserve = groups[len-preserved :]
        emit tengu_reactive_compact_attempt{attempt, ..., tokenGap}
        r = await summarizeCall(flatten(toSummarize), ...)                             # _kd
        if r.ok: (emit tengu_compact_credits_clamp_rescue if rescue); return {ok:True, summaryMessages, messagesToPreserve}
        switch r.reason:
            'media_too_large': strip media once, retry
            'prompt_too_long': step preserved up via gapGuidedStep(tokenGap)           # ykd/C$i
        if r.viaCreditsBoundary: creditsRescue=True   # clamp target -> Nv(msgs)-KQ

async summarizeCall(msgs, ctx, customInstructions, strip):   # _kd
    resp = await query(querySource:'compact', forkLabel:'reactive-compact', maxTurns:1,
                       maxOutputTokens:min(20000,modelMax),                            # Akt
                       systemPrompt: buildSummaryPrompt(VariantB, customInstructions))
    if startsWith(err,'Prompt is too long'): return {prompt_too_long, tokenGap, viaCreditsBoundary:qUi(resp)}  # she/Gwn
    if mediaError(err): return {media_too_large}
    return {ok:True, messages:[user msg {isCompactSummary:True, isVisibleInTranscriptOnly:True}]}

# ---- classic path ----
generator classicCompact(messages, ctx, ..., isAuto, customInstructions, coldCompact, hint):   # Vut
    label = isAuto ? 'compact_auto' : 'compact_manual'
    emit compact_progress(hooks_start, pre_compact) + sdk_status('compacting')
    hook = await runPreCompactHook(trigger=isAuto?'auto':'manual', customInstructions)         # lJ
    customInstructions = merge(customInstructions, hook.newCustomInstructions)
    emit compact_progress(compact_start, hint)
    cachePrefix = not coldCompact and flag(tengu_compact_cache_prefix, True)
    system = buildSummaryPrompt(VariantB IRn, customInstructions)
    ptl = 0
    loop:
        resp = await summarize(messages, system, stripNonEssential=coldCompact)
        if not startsWith(resp,'Prompt is too long'): break
        ptl += 1
        trimmed = ptl <= gel(3) ? dropOldest(messages, resp) : None                            # _el
        if not trimmed: emit tengu_compact_failed{prompt_too_long, ptl}; throw
        emit tengu_compact_ptl_retry; messages = trimmed
    return summary (isCompactSummary:True)

# ---- microcompaction (time-based) ----
function microcompact(messages, cfg):   # o$i
    keep, clear, tokensSaved = computeKeepClear(messages, cfg.keepRecent)              # H5r
    if tokensSaved < 20000: return None                                               # k5r — NO event below this
    for r in clear: r.content = '[Old tool result content cleared]'                   # yRn
    emit tengu_time_based_microcompact{tokensSaved, keepRecent}
    return rewritten messages

# ---- precomputed compaction ----
function shouldArm(tokens, model, window, cfg):   # hFi
    eff = effectiveWindow(model, window)
    if not redwood3() and not windowIsExplicit(model,window):
        return tokens >= min(eff - round(eff*precomputeBufferFraction), Swn)           # A8r ; default frac f8r=0.2
async precomputeArm(boundaryUuid):
    emit tengu_precomputed_compact_started{...}          # present v2.1.181/179; outside 2.1.183 extract (medium for 183)
    r = await summarizeWithRetry(...)
    on retry: count[uuid]+=1; if count==3: emit _rearm_capped (tQa=3) else re-arm
    on ok:    emit tengu_precomputed_compact_ready{durationMs, attempts, groupsPreserved, totalGroups}
function precomputeConsume(boundaryUuid):   # NAo + G2p
    if matches boundary: emit _consumed; apply swap   else if stale: emit _discarded

# ---- hooks ----
async runPreCompactHook(trigger, customInstructions):   # lJ
    input = {hook_event_name:'PreCompact', trigger, custom_instructions}
    run matching hooks; return {newCustomInstructions, userDisplayMessage, blockedBy}   # non-empty blockedBy ABORTS
async runPostCompactHook(compactSummary):   # R0e
    input = {hook_event_name:'PostCompact', compact_summary: compactSummary}; run matching hooks

# ---- transcript load ----
function loadTranscript(path):   # nce
    if file large and not env.CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP:
        stream-scan for last system msg subtype=='compact_boundary' (compactMetadata.preservedSegment/preservedMessages)
        start from that boundary (preserved segment + post-compact tail)
    else: full parse
```

### 1.2 Trigger / threshold math `[S1-F2]`

- **Compact threshold** `Swn(window, cfg)`: `base = window − 13000` (`lFi`). The level evaluator `pFi` returns `blocked` at `tokens ≥ modelWindow − 3000` (`cFi`, or `testBlockingOverride`), `compact` at `tokens ≥ Swn` (only when enabled), `warn` at `tokens ≥ threshold − 20000`, else `ok`.
- **Effective compactable window** `cee(model, window) = resolveWindow(...).window − min(toolDefTokens, 20000)` (`AFi`) — a fixed tool-definition prefix up to 20k is reserved off the top `[S1-F4]`.
- **PCT override** (`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` → `testPctOverride`, `parseFloat`, `0<pct≤100`): `return min(floor(window*pct/100), base)`. Worked example, **empirically + statically confirmed by the probe** `[S1-F16]`: `window=200000, pct=1 → min(floor(2000), 187000) = 2000` tokens. `CLAUDE_CODE_BLOCKING_LIMIT_OVERRIDE` → `testBlockingOverride` (`parseInt`) overrides the blocking limit.

### 1.3 Window resolver precedence `[S1-F3]`

`J$` chain, highest precedence first; always returns `window = min(modelMax, configured)`:

1. env `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (`Tae`-parsed, clamped `[1e5, 1e6]`, `source:env`)
2. settings `autoCompactWindow` (`source:settings`)
3. clientdata via `Iwd`/`Rti` cache `rowan_thicket` (`source:clientdata`)
4. experiment `T8r` `tengu_amber_redwood2`, **opus-4-8 only** (`source:experiment`)
5. model-default `KQ=200000` when `modelMax<1e6` and model in `Hwd`/`SLr` (`source:model-default`)
6. `auto` — `mFi` per-model table or `modelMax`

Model context size `nE`/`vti`: `1e6` for `[1m]`-tagged / 1M-beta-header + `N8`-eligible / `jB`-eligible models; `CLAUDE_CODE_MAX_CONTEXT_TOKENS` honored **only** under `DISABLE_COMPACT` (`Cti`); `CLAUDE_CODE_DISABLE_1M_CONTEXT` (`vme`) disables all 1M detection; else `fkt=200000` `[S1-F4]`.

### 1.4 The four distinct paths

| Path | Function(s) | When | Output behavior |
|---|---|---|---|
| **Reactive** | `Hqn`→`DRn`→`_kd` `[S1-F6, S1-F7]` | `thresholdSource != 'auto'`; emits `tengu_auto_compact_routed_reactive` | Splits messages into groups (`FOt`), summarizes a prefix, steps `groupsPreserved` up via gap-guided stepping on `prompt_too_long`, strips media on `media_too_large`, clamp-rescues on a credits boundary (`tengu_compact_credits_clamp_rescue`, clamp target `Nv(msgs)−KQ`). Success → user message `isCompactSummary:true` + `isVisibleInTranscriptOnly:true`. |
| **Classic** | `Vut` `[S1-F8]` | the `else` branch (`thresholdSource=='auto'`), with `coldCompact=Gho()` | Picks `compact_auto`/`compact_manual`, runs `PreCompact` hook, builds system prompt `IRn`, loops summarizing; on `'Prompt is too long'` retries via `_el` up to `gel=3` else emits `tengu_compact_failed{prompt_too_long}` and throws. `tengu_compact_cache_prefix` gates prompt-cache sharing, skipped in cold-compact. |
| **Time-based microcompaction** | `o$i` `[S1-F9]` | when `tokensSaved ≥ k5r=20000` | Clears **old** `tool_result` content (keeps most recent, `keepRecent`), replaces cleared content with literal `'[Old tool result content cleared]'` (`yRn`). Below the 20k floor it returns `null` with **no event**. Fires `tengu_time_based_microcompact`. |
| **Precomputed** | `hFi`/`h4t`/`U6`/`NAo`+`G2p` `[S1-F10]` | arms in advance when `tokens ≥ A8r = min(eff − round(eff*frac), Swn)` and not `(mq() && VRe)` | Arms an in-flight summary keyed to a boundary uuid: `_started` → `_ready` → `_consumed` (re-arm up to `tQa=3` then `_rearm_capped`; stale → `_discarded`). Buffer fraction default `f8r=0.2`, overridable by `tengu_amber_rokovoko`/per-window `tengu_amber_moleskin`. Recovery-timeout `v8r=600000` ms `[S1-F16]`. |
| **Cold** | `Vut(coldCompact=true)` via `Gho()` / env `CLAUDE_CODE_COLD_COMPACT` | low-resource summarization | `stripNonEssential` summarization; **skips** cache-prefix sharing. |

### 1.5 PreCompact + PostCompact hook contract `[S1-F11]`

- **`PreCompact` (`lJ`)** builds `{hook_event_name:'PreCompact', trigger:('auto'|'manual'), custom_instructions}`, runs matching hooks, returns `{newCustomInstructions, userDisplayMessage, blockedBy}`. `newCustomInstructions` is merged into the summary system prompt via `qho`. **A non-empty `blockedBy` aborts compaction** with `'Reactive compact blocked by PreCompact hook: ${blockedBy}'`.
- **`PostCompact` (`R0e`)** builds `{hook_event_name:'PostCompact', compact_summary: e.compactSummary}` and runs matching hooks after a compaction completes. Both are registered in the `Pvl` hook event registry (~30 events).
- **CRITICAL injection caveat** `[S2-F3]` (confidence `confirmed-stable`): plain `PreCompact` **stdout** is *"written to the debug log but not shown in the transcript"*. Stdout is injected into the model's context **only** for `SessionStart`, `UserPromptSubmit`, `UserPromptExpansion`. `PreCompact` *can* block (exit code 2) and supports JSON output (`decision`/`systemMessage`/`additionalContext`), but a "echo a brief → it lands in post-compaction context" pipeline does **not** work. The supported re-injection path is `SessionStart(matcher:'compact')` `[S2-F8]`.

### 1.6 Summary-prompt variants `[S1-F13]`

Two variants share a builder selected by a ternary; **both** are wrapped by a CRITICAL TEXT-ONLY preamble + a `REMINDER` suffix (`b$i`), with an optional `Additional Instructions:` block injected before the reminder. Preamble + reminder present in **both** v2.1.179 and v2.1.183 (stable); their absence from the 2.1.181 curated body-only `.txt` is an extraction artifact, not a regression `[S1 version_drift]`.

**CRITICAL TEXT-ONLY preamble (verbatim):**

```text
CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.

- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.
- You already have all the context you need in the conversation above.
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.
```

**REMINDER suffix (`b$i`, verbatim):**

```text
REMINDER: Do NOT call any tools. Respond with plain text only — an <analysis> block followed
by a <summary> block. Tool calls will be rejected and you will fail the task.
```

- **Variant B (`IRn`, standard reactive / manual `/compact`):** body *"…of the conversation so far…"*, 9 sections: 1. Primary Request and Intent; 2. Key Technical Concepts; 3. Files and Code Sections; 4. Errors and fixes; 5. Problem Solving; 6. **All user messages** (instructs preserving security-relevant constraints **verbatim** so they continue to apply after compaction); 7. Pending Tasks; 8. Work Completed; 9. Context for Continuing Work. (Verified verbatim against `extracted/2.1.181/summary_prompt_variant1.txt`.)
- **Variant A (`hkd`, microcompact / forward-continuation):** body *"…continuing session…"*, ends 7. Pending Tasks; 8. Current Work; 9. **Optional Next Step** — which instructs keeping the next step *directly in line with the user's most recent explicit request*, *"do not start on tangential requests or really old requests… without confirming with the user first"*, include direct quotes.

> **Note the distinction (cross-link C5, OQ-8):** the Variant A "Optional Next Step" caution lives in the **summarization** system prompt. The harmful **continuation directive** in §2 (C5) is a *separately injected* instruction in the recovered session — a related but distinct surface. They co-exist; the continuation directive optimizes against re-confirmation and wins.

### 1.7 Post-compact transcript markers `[S1-F12]`

The transcript loader `nce` fast-skips large transcripts to the **last `compact_boundary` system message** unless `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP` is set. A compact boundary is a system message `subtype:'compact_boundary'` carrying `compactMetadata` with `preservedSegment` / `preservedMessages` / `postTokens` — the on-disk marker any observer/probe sees after compaction. The summary message itself carries `isCompactSummary:true`.

### 1.8 DYNAMIC PROBE — auto-compaction does not fire in print mode `[S1-F16]`

`high` confidence, `dynamic_probe` evidence. Across **4 print-mode (`-p`/SDK) sessions** in CC 2.1.183 forced with `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=1` (threshold = 2000 tokens), peak context reached **103,984 tokens (~52× threshold)** with **no compaction** — no `compact_boundary`, no `isCompactSummary:true`, no `compactMetadata`. Root cause: the static gates `Yw→l3p→Eho` contain **no interactivity check**; the suppression lives at the **query-loop wiring level** — the interactive REPL main loop invokes `Eho`, the print/SDK path does not. `--debug` emitted **zero stderr bytes** in print mode. The probe also confirmed the `Swn` threshold math empirically and resolved the last two numerics: `gel=3`, `v8r=600000`. The live end-to-end compaction **sequence** is observable only in an interactive TTY (out of scope for the non-interactive probe → `OQ-1`).

### 1.9 Per-version drift note `[S1 version_drift]`

Only **v2.1.183** is byte-precise beautified; **v2.1.179 / v2.1.181** corroborate via string/event presence only. Drift verdicts:

- Numeric thresholds (`lFi`/`cFi`/`AFi`/`Akt`/`k5r`/`bwn`/`_8r`/`KQ`/`fkt`/`jho`/`Who`/`f6n`/`tQa`/`f8r`) — **confirmed-stable**.
- `J$` window precedence chain — **confirmed-stable**.
- Reactive path (`Eho`/`Hqn`/`DRn`/`_kd`) + credits-clamp-rescue — **confirmed-stable**.
- Summary-prompt CRITICAL preamble + REMINDER suffix — **confirmed-stable** (181 `.txt` absence is an extraction artifact).
- Two prompt variants sharing a builder — **confirmed-stable**.
- `tengu_precomputed_compact_started` (arm-start event) — **version-drift-suspected-artifact**: named in the carve and present in both adjacent versions; absence in the 183 extract is almost certainly an extraction-window gap, **not** a removal. **Downgraded to `medium` for 183** `[OQ-2]`.
- PreCompact/PostCompact hooks (`lJ`/`R0e`/`Pvl`) — **confirmed-stable**.

> **Caveat (`OQ-3`, severity medium):** a regression that preserved all strings/event names but silently changed numeric constants between 179→181→183 would not be caught for 179/181 (no contradicting evidence found, but not byte-verified there).

### 1.10 Key numerics & env vars (quick reference)

| Constant | Value | Meaning |
|---|---|---|
| `lFi` | 13000 | compact offset (`threshold = window − lFi`) |
| `cFi` | 3000 | blocking offset (`block at modelWindow − cFi`) |
| warn offset | 20000 | `warn at threshold − 20000` |
| `AFi` | 20000 | reserved tool-definition prefix cap |
| `Akt` | 20000 | summary output-token max |
| `k5r` | 20000 | microcompact min tokens-saved floor (no event below) |
| `bwn`/`_8r` | 1e5 / 1e6 | window clamp min/max |
| `KQ`/`fkt` | 200000 | model-default window / context |
| `jho`/`Who`/`f6n` | 3 | circuit-breaker / thrash window / thrash count |
| `tQa` | 3 | precompute re-arm cap |
| `f8r` | 0.2 | precompute buffer fraction default |
| `gel` | 3 | classic prompt-too-long retry cap (probe-confirmed) |
| `v8r` | 600000 | precompute recovery-timeout ms (probe-confirmed) |

Env vars `[S1 env_vars]`: `DISABLE_COMPACT` (kills all compaction; then `CLAUDE_CODE_MAX_CONTEXT_TOKENS` honored), `DISABLE_AUTO_COMPACT` (auto only; manual `/compact` still works), `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (highest-precedence window override, clamped `[1e5,1e6]`), `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` (lowers compact threshold), `CLAUDE_CODE_BLOCKING_LIMIT_OVERRIDE` (overrides blocking limit), `CLAUDE_CODE_COLD_COMPACT` (cold-compact arg), `CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP` (forces full transcript parse), `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (honored only under `DISABLE_COMPACT`), `CLAUDE_CODE_REMOTE` (reactive compaction requires `tengu_reactive_compact_remote` flag), `CLAUDE_CODE_DISABLE_1M_CONTEXT` (forces 200000).

---

## 2. What Is Broken — Complaint Clusters C1–C5

> Source: S3, corroborated across GitHub + Reddit. Each cluster is mapped via `cross_links` to the S1 mechanism that causes it. **S3's own mechanism hypotheses are superseded by the S1 mechanisms below** `[S3 note, OQ-8]`. Ranked by frequency × severity.

### C1 (rank 1) — Context loss / forgetting after compaction `[S3-C1]` — *confidence: confirmed-single-version*

Drops user intent, prior state, in-flight working procedures, decisions/rationale, file mods, and **active skills**; the agent confabulates done-vs-todo, hallucinates, repeats fixed errors. Reddit reframes it as *structural*: compaction-by-conversation is not neutral compression; primacy/recency bias weakens the middle of the run `[S3-WF-REDDIT-1ro7rw4-F1, -F7]`. Skill loss is its own corroborated sub-cluster: post-compaction the agent loses awareness of skills in use, does not re-read skill files, and ignores explicit CLAUDE.md reload instructions `[S3-F13919-F1, -F2, -F5]`.

**Mechanism (S1):** the reactive/classic summary **replaces** the live message history with a single LLM-generated prose summary (`Vut`/`DRn`/`_kd` → one `isCompactSummary` user message; transcript loads from the last `compact_boundary`) `[S1-F6, S1-F8, S1-F12]`. The Variant B summary prompt is prose-oriented with 9 sections — it instructs preserving user messages and security constraints, but provides **no schema** privileging the planned-vs-implemented distinction or the *paths* of system-injected context (CLAUDE.md, Skill files) which live **outside** the conversation being summarized, so their paths are lost and never re-fetched `[S1-F13]`. Microcompaction (`o$i`) additionally hard-clears old `tool_result` content to a literal placeholder `[S1-F9]`.

### C2 (rank 2) — Fires mid-task / too early, without warning `[S3-C2]` — *confidence: confirmed-single-version*

Interrupts active work at a token threshold the user cannot see coming or align to task boundaries; a premature **~55K threshold in VS Code** compounds it (user-reported, unverified) `[S3-F13919-F3]`. Reddit: *"burns a meaningful chunk of context before useful work even starts"*, radical/unexpected `[S3-WF-REDDIT-1ro7rw4-F2, -F6]`.

**Mechanism (S1):** the trigger is a pure token-budget heuristic with **no task-boundary / safe-point detection** — `pFi` returns `compact` at `tokens ≥ Swn = effective_window − 13000`, evaluated whenever the budget is crossed (statistically mid-tool-call / mid-reasoning when token volume peaks) `[S1-F2]`. No pre-trigger notification beyond the `F4l` status bar (*"{pct}% until auto-compact"*) `[S1-F14]`. The user "~55K" and "pct=80" numbers reconcile against S1's threshold math; with `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` the compact threshold becomes `min(floor(window*pct/100), window−13000)`, so a low pct (or small effective window) lowers it well below task completion. **cc-squash cannot move the trigger** (it is wired into the interactive query loop, `[S1-F16]`) — only react at the `PreCompact` boundary.

### C3 (rank 3) — No reliable recovery after a bad compaction `[S3-C3]` — *confidence: confirmed-single-version*

Only escapes are lossy (`/rewind` discards subsequent work, `/clear`, force `/compact`, restart) or DIY checkpoint workflows; the agent does not self-recover, turning 1h tasks into 5–6h across 4–6 compaction cycles `[S3-F13919-F2]`. Reddit: disabling auto-compact just moves the failure to the hard context limit — *"no clean continuation, no real recovery"* `[S3-WF-REDDIT-1ro7rw4-F3]`.

**Mechanism (S1):** CC runs **no post-compaction recovery protocol** beyond injecting the continuation directive — `PostCompact` (`R0e`) only passes `compact_summary` to user hooks; nothing re-reads the system-injected files/skills/CLAUDE.md whose *paths* were dropped (they were never in the summarized conversation) `[S1-F11]`. The transcript loader `nce` starts from the last `compact_boundary` preserved segment + tail; `/rewind` operates on the message timeline so it discards everything after `[S1-F12]`. At the hard limit `pFi` returns `blocked` (`≥ window−3000`) and `_kd` retries summarization, but there is no clean user-controlled continuation path.

### C4 (rank 4) — No user control over auto-compact `[S3-C4]` — *confidence: confirmed-single-version*

Cannot disable, configure threshold, or opt into a pre-compact prompt; disabling works in CLI but is reportedly impossible in the VS Code extension (`[S3-F13112-F3]`, single GitHub thread → verify against current build, `OQ-10`). Reddit: the only "controls" are an undocumented env var (`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=80` in `settings.json`) and CLAUDE.md override rules `[S3-WF-REDDIT-1ro7rw4-F5]`.

**Mechanism (S1):** auto-compact is an always-on harness behavior keyed to the token budget. The first-class knobs **do exist** as env/settings (`DISABLE_AUTO_COMPACT`, `autoCompactEnabled`, `autoCompactWindow`, `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW`) — confirming the reddit "undocumented env var" is **real** `[S1-F5, S1-F2]` — but they are not surfaced through every front-end (the VS Code gap), and there is no `settings.json autoCompact{enabled,threshold}` schema or pre-trigger interactive prompt because the trigger runs synchronously inside the interactive query loop with no user-facing decision point `[S1-F16]`.

### C5 (rank 5) — Post-compaction prompt-injection makes the agent act recklessly `[S3-C5]` — *HIGHEST-LEVERAGE ACTIONABLE ISSUE*

> Confidence: `confirmed-single-version` for the cluster; the verbatim directive is **double-confirmed across channels** (`high`). Highest-engagement single complaint (reddit 1piny6t score 67, 44 comments) `[S3-WF-REDDIT-1piny6t-A]`.

After compaction CC injects a fixed **continuation directive** into the recovered context. Verbatim `[S3 verbatim_injected_directive]`:

> *"Please continue the conversation from where we left it off without asking the user any further questions. Continue with the last task that you were asked to work on."*

Right after context loss this makes the agent barrel into action (wrong/unrelated tasks, off into recent-git code) instead of pausing to confirm. Reddit shows it **overriding explicit user CLAUDE.md plan-then-approve constraints** `[S3-WF-REDDIT-1piny6t-A]`. The commenter's proposed correction: *"Confirm current status with the user before proceeding."* `[S3-F13112-F2]`. Users' only current mitigation is a self-authored CLAUDE.md "CONTEXT COMPACTION OVERRIDE" rule that pattern-matches the exact injected string `[S3-WF-REDDIT-1piny6t-B]`.

**Mechanism (S1):** this continuation directive is **related to but distinct from** the summarization system prompt `[S1-F13]`. The summary prompt's Variant A "Optional Next Step" already cautions to stay in line with the user's most recent explicit request and not start tangential requests without confirming — but the separately-injected continuation directive optimizes *against* re-confirmation, so combined with a lossy summary (C1), any phrase that reads as a pending action is taken as "begin executing now," and the **system directive wins over user CLAUDE.md constraints**. With grounding gone, the model fills the gap with the most salient nearby signal (recent git diffs) `[cross-link C5]`.

**Why this is the highest-leverage fix:** it is the cheapest to address (one inverted directive re-injected at `SessionStart(matcher:'compact')`) and the most damaging when unaddressed (it actively *defeats* the user's plan-then-approve workflow — the exact constraint cc-squash exists to preserve).

---

## 3. What Others Do

### 3.1 Third-party plugins/tools (S2)

| Tool | Architecture | Verdict |
|---|---|---|
| **mvara-ai/precompact-hook** `[S2-F1, S2-F2]` (`confirmed-single-version`) | One `PreCompact` hook → `tail -c 40960` of the `.jsonl` → fresh `claude -p` subagent → 6-section brief → stores via a **private** `genesis-ocean` MCP `preserve` tool, echoes only a memory UUID. | **Non-functional as published** (private MCP not shipped). Anti-pattern: external private-MCP dependency. |
| — its **architectural defect** `[S2-F3]` (`confirmed-stable`) | Assumes `PreCompact` stdout reaches the recovered model context. It does not (stdout → debug log only). | **The central lesson:** do not rely on `PreCompact` stdout for re-injection. |
| **disler/claude-code-hooks-mastery** `[S2-F4]` (`confirmed-single-version`) | `pre_compact.py` archives raw `.jsonl` (no compression); re-injection is a **separate** `SessionStart` hook (`session_start.py`) emitting `hookSpecificOutput.additionalContext` (git branch, first 1000 chars of CONTEXT.md/TODO.md, ≤5 GitHub issues). | Both inert by default; **the two pieces never link**. Confirms the correct `additionalContext` re-injection mechanism. |
| **webdevtodayjason/claude-hooks** `[S2-F5]` (`confirmed-single-version`) | `PreCompact`+`Stop` pair; reminder-injection (re-read checklist via `decision:block`), not preservation. Marker approach self-labeled PROVISIONAL. | Reminder, not preservation. |
| **thedotmack/claude-mem** `[S2-F6]` (`inferred`) | 5 lifecycle hooks (NOT PreCompact-centric); AI-compresses session data into **structured observations** (facts, concepts, file refs) in SQLite FTS5 + optional ChromaDB; `SessionStart` injects a compressed index. | **Best preservation target idea:** structured records over prose `[S2 claude_mem_structured_observations]`. Borrow the idea, skip the SQLite/Chroma/worker weight. |
| **peless/claude-thread-continuity** `[S2-F7]` (`inferred`) | Pure-MCP (no hooks); auto-saves project state (decisions, modified files, action items) to `~/.claude_states/` JSON, restores via `load_project_state`; "every 10 messages" fallback, 5-snapshot rotation. | Representative of a large near-identical MCP cluster. Durability via snapshot rotation transfers. |
| **Dicklesworthstone/post_compact_reminder** `[S2-F8]` (`confirmed-single-version`) | **ONE** hook: `SessionStart` with `matcher:'compact'` (fires only after compaction); verifies `source=='compact'`, prints a plain-text reminder to stdout (which **is** added to context). | **The correct post-compaction injection mechanism** mvara-ai got wrong. The template for cc-squash's re-injection. |
| **simonw/claude-code-transcripts** et al. `[S2-F9]` (`inferred`) | Read-only transcript→HTML tooling; no hooks, no compaction. | False positives. Useful only to confirm transcript location `~/.claude/projects/<slug>/*.jsonl` and the `.jsonl` record shape. |

**Official Anthropic baselines:**

- **Memory tool** (`memory_20250818`, beta) `[S2-F10]` (`confirmed-stable`): model-driven client-managed `/memories` file CRUD (view/create/str_replace/insert/delete/rename). System prompt tells the model to **always view `/memories` first** and **assume the context window may reset at any moment**, recording progress. Docs explicitly recommend **pairing memory with compaction** — compaction keeps active context manageable, memory persists critical info across compaction boundaries *"so nothing critical is lost in the summary."* SDK helpers: `BetaAbstractMemoryTool` (Py/C#), `betaMemoryTool` (TS), `BetaMemoryToolHandler` (Java).
- **Server-side Compaction + Context editing** `[S2-F11]` (`confirmed-stable`): server-side compaction auto-summarizes the whole conversation near the limit (distinct from context editing, which only clears stale tool results). **This is the baseline cc-squash augments** — CC's built-in compaction is what actually shrinks context; cc-squash improves *what survives* and *re-injects it correctly*, not reimplementing summarization.
- **Upstream bug #13572** `[S2-F12]` (`unverified`): reported that `PreCompact` does not fire on manual `/compact` (auto only). CLOSED as "not planned"+"stale", single report. **Must be re-verified against the target CC version** (`OQ-5`); if true, a PreCompact-only design misses manual `/compact` — reinforcing the need to also hook `SessionStart(matcher:'compact')`.

### 3.2 Prior-art taxonomy (S4) — shortlist by applicability + the negative result

> All S4 findings are `high` confidence (or `confirmed-stable`/`confirmed-single-version` for the KV bucket). Applicability is scored 0–3 for client-side fit to cc-squash (text-in/text-out over a frozen hosted model, transcript + hooks only).

**Client-side shortlist, ranked by applicability:**

| Technique | App. | Transfer | ID |
|---|---|---|---|
| **Rsum** (Recursive Summarization for Long-Term Dialogue Memory) | **3/3 (HIGHEST)** | Prompt-only rolling memory: `previous-memory + new-context → new-memory`, then respond from latest memory. Works on a frozen LLM by prompting alone; maps one-to-one onto compacting at each boundary while carrying an evolving summary. | `[S4:F1-Rsum]` |
| LLMLingua-2 | 2/3 | Distilled XLM-RoBERTa token classifier predicts keep/drop per token, plain-text in/out, any frozen black-box LLM. **But** domain-agnostic — no mechanism to protect user constraints/decisions/in-flight work; needs structure-aware gating. | `[S4:llmlingua2]` |
| Selective Context | 2/3 | Frozen base LM scores token self-information `I(x)=−log2 P(x)`, prunes below a percentile. **But** surprisal ≠ task relevance — a pruning signal, not a preservation policy. | `[S4:selective-context]` |
| Chain of Density (CoD) | 2/3 | Iterative densification — pack more salient entities into a fixed budget without growing length. Drop-in densification pass at the boundary; re-specify entity-saliency to protect constraints/decisions/in-flight work. | `[S4:cod]` |
| MemGPT | 2/3 (agent-memory) | Preserve-on-eviction + recursive summarization transfer; the live LLM-driven paging runtime does not. | `[S4:memgpt]` |
| Mem0 | 2/3 (agent-memory) | **Extract salient facts → ADD/UPDATE/DELETE/NOOP** reconcile half transfers; the vector/graph store + retrieval half does not. LOCOMO: ~26% LLM-judge gain, >90% token savings. | `[S4:mem0]` |
| Generative Agents | 2/3 (agent-memory) | record→score→synthesize-cited-summaries→keep-within-budget; recast importance/recency as constraint/decision/in-flight salience. | `[S4:generative-agents]` |
| MemoryBank | 2/3 (agent-memory) | **Closest LOCUS match** (prompt/text layer, frozen model); recall-reinforced **Ebbinghaus decay** = a concrete salience-over-time policy. FAISS retrieval is the non-transferable half. | `[S4:memorybank-mechanism]` |
| Wu et al. 2021 (recursive book summarization) | 2/3 | Hierarchical chunk→summarize→recursively-summarize with "prior summaries as context"; RLHF + full multi-level tree do not transfer. | `[S4:wu2021]` |
| RAPTOR | 2/3 | Recursive abstractive summarization of old spans into coarser levels; the embedding store + GMM/UMAP clustering + retrieval do not fit a no-query compaction step. | `[S4:raptor]` |

**Explicit negative results — DO NOT transfer:**

- **KV-eviction bucket** `[S4:KV_EVICTION_NEGATIVE_RESULT]` (StreamingLLM, SnapKV, BUZZ, Scissorhands, FastGen, PyramidInfer at 1/3; H2O, Ada-KV, PyramidKV at 0/3): these operate inference-internally on **GPU KV tensors / per-head attention scores** inside a single forward pass against a closed hosted model. **cc-squash has no KV access.** Only loose heuristic analogies transfer ("keep recent + a budget of the most-important; importance persists; older/deeper material needs fewer details") — never the mechanism.
- **Soft-token / architectural-memory** `[S4:icae, S4:autocompressors, S4:gisting, S4:melodi]` (all 0/3 or framing-only): ICAE, AutoCompressors, Gisting require training a LoRA encoder / fine-tuning / attention-mask surgery and injecting **embedding-level soft tokens** — there is no surface to inject soft prompts into a frozen hosted model over the messages API. MELODI's only transferable bit is its two-tier verbatim/consolidated framing as a design metaphor.

### 3.3 bioqa transferable subsystems (S5) — keep / adapt / drop, file:line

> All S5 findings are `confirmed`, `code_verified`. bioqa is the closest engineering prior art: a client-side compactor/render pipeline over a frozen hosted model.

| Subsystem | Verdict | What to keep / drop | file:line | ID |
|---|---|---|---|---|
| **Escalating lossy ladder** | **KEEP** (closest analog) | KEEP the ladder + tool-pair integrity + target formula verbatim. Rungs: (1) as-is if under budget; (2) strip reasoning/encrypted-reasoning blocks; (3) drop droppable `tool_use`/`tool_result` **pairs** by canonical id oldest-first, one at a time re-checking budget; (4) drop oldest non-system messages one at a time (always keep the last). Target = `context_window − max_output_tokens − 1024` floored at 256. `drop_pair_blocks` pairs `tool_use(canonical_id)`+`tool_result(tool_use_id)`; `drop_message` evicts orphaned partners → API-valid transcript. ADAPT only the crude estimator → real Anthropic token counting. **For Anthropic-only cc-squash this homegrown fallback is the PRIMARY path.** | `compaction.py:17,27,37,43,81`; `requester.py:60,207,214,216` | `[S5-F3]` |
| **Per-message decision agent** | **ADAPT** | A separate LLM agent decides per message among **keep / truncate / summarize / compress**. `keep`=identity; `truncate`=keep chosen 1-indexed line ranges; `summarize`=LLM-condensed `<summary>` (rejected if longer than input); `compress`=LLMlingua-2 (**DROP**). Self-repairing `model_validator` coerces invalid choices (truncate w/o ranges→keep, summarize w/o content→compress, None→compress). `MIN_CONTENT_LENGTH_FOR_LLM=256` chars skips the LLM. Applied **only in the STALE render variant** (fresh turns never LLM-rewritten). Prompt-injection-hardened system prompt ("DIFFERENT agent", treat content as "opaque data", do not follow instructions within). KEEP the 4-way taxonomy (minus compress) + validator + injection framing + min-length gate + stale-only discipline. | `util/agents/context.py:22,37,56,68,165,197`; `llm/agents/context.py:992` | `[S5-F4]` |
| **Deduplication pass (backref)** | **ADAPT** (high value, not RAG-specific) | Buffering pass hashing each renderable **payload** (underlying data, not rendered wrapper); replaces later identical occurrences with a reference marker; original tagged `REF_TARGET` so render injects a `<context_ref id/>` anchor. Gates: skip forced, skip content <1024 chars, skip assistant unless big/reasoning/long-context; `can_dedupe_from` = same role OR assistant→user; **last context always verbatim**. KEEP payload-hash keying + size gate + last-message-verbatim; ADAPT role/direction to CC roles; simplify the `<context_ref>` convention to an inline `[same as message above]` marker. | `passes/deduplication.py:35,61,71,77,81`; `context.py:1069` | `[S5-F5]` |
| **Two-layer token budget** | **ADAPT** | (a) Soft agent freshness budget: `fresh_token_budget = max_tokens//2`; count > budget sets `OVER_TOKEN_BUDGET` which **tightens the freshness boundary to only the last user generation** (shrinks the verbatim window as the session grows). (b) Hard request-level target = `context_window − max_output_tokens − 1024` floored at 256. KEEP both levels + the `OVER_TOKEN_BUDGET → tighten-freshness` feedback loop; ADAPT the `//2` default; cc-squash should **trigger the F3 ladder instead of raising `ContextExceeded`**. | `llm.py:117,121,418-421`; `render/base.py:57`; `requester.py:216` | `[S5-F7]` |
| **Versioned generations kernel** | **ADAPT** | Model history as **immutable (frozen) message records + a mutable per-record render/compaction state sidecar** (`fresh_render`/`stale_render`/`token_count`/`hash`/`compaction`), grouped into generations delimited by user turns → clean "recent N verbatim, older degrade" boundary. DROP the anyio `MemoryObjectSendStream` append path, weakref fork sync, `pending_updates`, `ContextToken` futures, `checkpoint_loc`, reset/restore (parallel-fork machinery). | `llm/agents/context.py:99,630,690,697,804,831,888` | `[S5-F1]` |
| **12-pass render pipeline** | **ADAPT** | Composable ordered passes (Forced→Inclusion→Freshness→ViewImage→Compaction→Diff→Deduplication→Ordering→ToolCall→Error→Image→Cache). KEEP the composable pipeline + streaming-vs-buffering distinction (`BufferingRenderPass` collects the whole list) + freshness boundary heuristic (<2 user gens ⇒ all fresh; else last user gen if `OVER_TOKEN_BUDGET` else second-to-last) + `CachePass` prefix breakpoints + `cap_cache_hints(4)` (maps to Anthropic's 4-breakpoint cap). DROP RAG-specific passes (ViewImage/Image/Diff/ToolCall) + async plumbing. | `pipeline.py:36,57`; `base.py:26,48,61`; `context.py:1039,1077` | `[S5-F2]` |
| **Ordering pass** | **ADAPT CAUTIOUSLY** | KEEP pinning system/instruction context to a stable front (lands on a cache prefix) + a settable `priority` field. **DROP free reordering of conversational turns / tool runs** — chronological order is load-bearing and the Anthropic API requires valid alternating turns with paired `tool_use`/`tool_result`. bioqa can reorder only because its "data" contexts are order-independent RAG blocks. | `passes/ordering.py:14,19,28,35`; `context.py:126` | `[S5-F6]` |
| **LLMlingua-2 server** | **DROP** | A self-hosted 560M-param XLM-RoBERTa Modal GPU service (`microsoft/llmlingua-2-xlm-roberta-large-meetingbank`, rate 0.45). Heavyweight infra a client-side CC compactor cannot/should not ship. Removing it leaves keep/truncate/summarize as the portable strategies. **No part transfers.** | `compress/compress.py:21,30,55` | `[S5-F8]` |

---

## 4. What We Should Build (`cc-squash`)

> Each recommendation cites the finding IDs that justify it. Style: async-native (`anyio`, `aiosqlite`), frozen dataclasses, pattern-matching for dispatch (per STYLEGUIDE.md). cc-squash sits **between** Anthropic's compaction (shrinks) and memory (persists) `[S2-F10, S2-F11]` — it improves *what survives* and *re-injects correctly*.

### 4.1 Which hooks to use (and why)

Two hooks, with a clean separation of capture and re-injection:

1. **`PreCompact` (`lJ`) — capture + shape.** Fires before CC summarizes `[S1-F11]`. Use it to (a) read the transcript and **build the structured working state** (§4.2) at the trigger boundary, persisting it durably (not to stdout); (b) optionally inject `newCustomInstructions` into the summary prompt via the documented merge path; (c) optionally `blockedBy` to abort a compaction (e.g., if the user has a `cc-squash` "pin until safe-point" rule). **Do NOT rely on `PreCompact` stdout** — it is dropped (debug-log only) `[S2-F3]`. If you must inject from PreCompact, use its JSON `additionalContext`/`systemMessage` output (`OQ-6`: verify end-to-end against the target version).
2. **`SessionStart(matcher:'compact')` — actually re-inject.** Fires only after compaction (`source=='compact'`); its stdout / `hookSpecificOutput.additionalContext` **does** reach the recovered model context `[S2-F8, S2-F3]`. This is where cc-squash delivers (a) the structured brief from §4.2, (b) the **inverted continuation directive** (§4.4), and (c) the *paths* of system-injected context (CLAUDE.md, Skill files) so the agent re-reads them — automating the manual "reload all skills before continuing" workaround `[S3-F13919-F4, cross-link C3]`.

> Re-verify upstream #13572 (PreCompact may not fire on manual `/compact`) against the target version `[S2-F12, OQ-5]`. Because `SessionStart(matcher:'compact')` fires after *any* compaction, hooking it covers the manual-`/compact` gap regardless.

### 4.2 Core data model (frozen dataclasses)

Make invalid states unrepresentable. The preservation target is **structured records, not prose** — the lesson from claude-mem's structured observations `[S2-F6]` and the cross-link C1 fix. Three salience-typed records plus an immutable-record + mutable-sidecar kernel from bioqa `[S5-F1]`:

```python
from __future__ import annotations
from dataclasses import dataclass
from enum import Enum
from typing import NewType

MessageId = NewType("MessageId", str)

class Salience(Enum):
    CONSTRAINT = "constraint"        # live user rule (plan-then-approve, never-touch files, secret handling)
    DECISION = "decision"            # a settled choice + rationale (planned-vs-implemented)
    IN_FLIGHT = "in_flight"          # the current task + safe-point state

@dataclass(frozen=True, slots=True)
class Constraint:
    """A live user constraint that MUST survive compaction verbatim."""
    text: str                        # preserved verbatim (cf. summary prompt §6 "security constraints verbatim")
    source_message: MessageId
    superseded_by: MessageId | None = None   # bi-temporal: live unless superseded [S4:zep-graphiti]

@dataclass(frozen=True, slots=True)
class Decision:
    text: str
    rationale: str
    planned: bool                    # the planned-vs-implemented distinction CC's prose summary loses [cross-link C1]
    superseded_by: MessageId | None = None

@dataclass(frozen=True, slots=True)
class InFlightWork:
    task: str
    last_safe_point: str
    open_files: tuple[str, ...] = ()
    skill_paths: tuple[str, ...] = ()   # paths of system-injected context to re-read [S3-F13919-F4]

@dataclass(frozen=True, slots=True)
class WorkingState:
    """The structured brief re-injected at SessionStart(matcher:'compact')."""
    constraints: tuple[Constraint, ...] = ()
    decisions: tuple[Decision, ...] = ()
    in_flight: InFlightWork | None = None
```

The transcript is modeled as **frozen message records + a mutable per-record sidecar** carrying `fresh_render` / `stale_render` / `token_count` / `hash` / `compaction` state, grouped into **generations delimited by user turns** for a clean "recent N verbatim, older degrade" boundary `[S5-F1]`. Drop bioqa's fork/weakref/anyio-stream machinery.

### 4.3 The compaction algorithm

A layered design composing the highest-applicability prior art with bioqa's verified engineering. Two budget layers `[S5-F7]`:

- **Soft layer (degrade early).** As the session grows past a render budget, demote older generations to *stale* and shrink the verbatim window — the `OVER_TOKEN_BUDGET → tighten-freshness` feedback loop. This is also cc-squash's partial answer to **C2** ("fires too early"): it preserves a recoverable structured state *before* CC's hard trigger, since cc-squash cannot move the trigger itself `[S1-F16, cross-link C2]`.
- **Hard layer (the ladder).** When the request-level target (`context_window − max_output_tokens − 1024`, floored at 256) is exceeded, run bioqa's **escalating lossy ladder** with **tool-pair integrity** `[S5-F3]`. ADAPT only the estimator → real Anthropic token counting (`OQ-14`).

Within that frame, the per-message and carry-forward logic:

1. **Recursive carry-forward (Rsum).** Maintain a single evolving `WorkingState` summary: at each boundary, `previous-state + new-turns → new-state`, prompt-only over the frozen model `[S4:F1-Rsum]`. This is the one 3/3-applicability algorithm.
2. **Per-message strategy selection.** A prompt-injection-hardened decision agent picks **keep / truncate / summarize** per message (drop `compress`/LLMlingua-2) with the self-repairing validator + 256-char min-length gate + **stale-only discipline** (fresh turns are never LLM-rewritten) `[S5-F4]`. Use `match` for dispatch:

   ```python
   match decision:
       case Keep():                       return msg
       case Truncate(ranges=ranges):      return msg.keep_lines(ranges)
       case Summarize(content=content):   return msg.replace(content)
   ```
3. **Dedup-with-backref.** Hash payloads, replace later duplicates with an inline `[same as message above]` marker; last message always verbatim; size-gate at ~1024 chars `[S5-F5]`.
4. **Constraint-aware gating atop a LLMLingua-2-style scorer.** A domain-agnostic token-importance dropper has **no mechanism to protect constraints** `[S4:llmlingua2, S4:selective-context]`. cc-squash bolts a structure-aware gate on top: anything tagged `Salience.CONSTRAINT`/`DECISION`/`IN_FLIGHT` is **pinned** (never dropped, preserved verbatim for constraints), and only un-pinned, stale, non-tool-paired content is eligible for the scorer/ladder. This is the open design problem `OQ-11` — the scorer is a *pruning signal*, the salience tags are the *preservation policy*.
5. **Densify to budget (optional).** A CoD-style densification pass packs more salient state into the fixed brief without growing length `[S4:cod]`.

> Render ordering: pin system/instruction context to a stable cache prefix; **do not reorder conversational turns or tool runs** (API requires valid alternating turns with paired `tool_use`/`tool_result`) `[S5-F6]`. Respect Anthropic's 4-breakpoint cache cap via `cap_cache_hints(4)` `[S5-F2]`.

### 4.4 Neutralizing / augmenting the "continue without asking" directive (C5 — highest leverage)

The single biggest win `[S3-C5, cross-link C5]`. cc-squash re-injects, via `SessionStart(matcher:'compact')` `additionalContext` (where the harmful directive also lands), an **inverted, subordinated** directive:

- **Invert:** replace "continue without asking" with "confirm current status with the user / re-read state before acting" — designbyian's proposed correction `[S3-F13112-F2]`.
- **Subordinate:** the recovery brief must explicitly defer to user CLAUDE.md constraints rather than override them. Surface the preserved `Constraint` records (esp. plan-then-approve) verbatim at the top of the brief, so they out-rank the residual continuation directive.
- This **automates** the manual CLAUDE.md "CONTEXT COMPACTION OVERRIDE" pattern-match users currently hand-write `[S3-WF-REDDIT-1piny6t-B]`, and reinforces the Memory-tool framing ("assume interruption, record progress, re-verify before acting") `[S2-F10]`.

### 4.5 Forgetting / superseding

The thinnest-prior-art requirement: losslessly preserve live constraints while dropping superseded ones at one boundary `[OQ-12]`. Only three concrete leads exist `[S4:forgetting_mechanisms]`:

- **Bi-temporal invalidation (primary).** Mark a superseded `Constraint`/`Decision` invalid-as-of-T (`superseded_by` field above) while **keeping history**, rather than deleting — Zep/Graphiti's edge-invalidation `[S4:zep-graphiti]`. A constraint is "live" iff `superseded_by is None`. Only live constraints are pinned/re-injected.
- **Mem0 reconcile.** On extracting a new fact, run `ADD / UPDATE / DELETE / NOOP` against existing records so the store stays conflict-free — `DELETE` only when explicitly contradicted `[S4:mem0]`.
- **Ebbinghaus decay (for un-pinned recency).** `retention = exp(−t/(5·S))`; recalling a record increments strength `S` and resets last-recall, so frequently-recalled memories decay slower `[S4:memorybank-mechanism]`. Apply **only** to un-pinned/stale material — never to live constraints (decay fights retention of recent in-flight constraints if applied indiscriminately `[S4:memorybank-summarization]`).

### 4.6 How cc-squash should test itself

The hard constraint: **auto-compaction does not fire in print/SDK mode** `[S1-F16]`, so CI cannot exercise the live end-to-end sequence through `-p`.

- **Unit-test the pure functions.** The data model, the salience extractor, the per-message decision dispatch (`match`), the dedup hasher, the ladder rungs, the bi-temporal supersede logic, and the brief renderer are all pure given a fixture transcript — test them with strict assertions against expected `WorkingState` outputs.
- **Test hook payloads against real fixtures.** Use a real `.jsonl` transcript fixture (location confirmed: `~/.claude/projects/<slug>/*.jsonl` `[S2-F9]`) and assert cc-squash's `PreCompact` capture and `SessionStart(matcher:'compact')` `additionalContext` payloads — including the inverted directive and the constraint block.
- **Use real token counting, not the crude estimator.** Replace bioqa's `len*4` estimator with Anthropic `count_tokens` (or the anthropic tokenizer); assert the ladder/budget math against real counts `[OQ-14]`.
- **Reserve the live sequence for an interactive TTY harness.** The compact-boundary emission *order* and the actual continuation-directive surface can only be observed in an interactive TTY (`OQ-1`). Drive a real interactive session (e.g. a pty harness) forced with `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` to a low threshold, then assert on the on-disk `compact_boundary` / `isCompactSummary` / `compactMetadata` markers `[S1-F12]`. Do not expect `--debug` stderr telemetry in print mode (it emits zero bytes `[S1-F16]`).
- **Verify the chosen injection path end-to-end** against the live docs/version before relying on it (`OQ-6`): confirm `SessionStart(matcher:'compact')` `additionalContext` reaches context and that `PreCompact` JSON output behaves as documented on the target CC version.

---

## 5. Open Questions & Runtime-Confirm Backlog

> Items needing live confirmation, with severity. Nothing below should be treated as settled.

| ID | From | Question / risk | Severity |
|---|---|---|---|
| **OQ-1** | S1 | **Live end-to-end compaction sequence never observed** — auto-compaction does not fire in print/SDK mode `[S1-F16]`, so the on-disk markers are code-confirmed but their live emission **order** is unverified. Needs an interactive TTY session. `--debug` emits zero stderr in print mode. | **high** |
| OQ-2 | S1 | `tengu_precomputed_compact_started` not re-observed in the 2.1.183 extract (present in 181/179) — single-version downgrade to `medium` for 183; almost certainly an extraction-window gap, not a removal. | low |
| OQ-3 | S1 | Cross-version **logic** diff anchored only on 2.1.183; 179/181 corroborate via string/event presence only. A numeric-constant regression preserving all strings between versions would not be caught for 179/181. | medium |
| OQ-4 | S1 | Exact group-splitting (`FOt`) and gap-seed (`C$i`/`ykd`) per-step token arithmetic confirmed to exist + be gap-guided but not exhaustively unit-traced. `mFi` per-model window table and `Hwd` default-set contents not enumerated. | low |
| OQ-5 | S2 | **#13572 (PreCompact may not fire on manual `/compact`)** is CLOSED+stale+single-source — must be re-verified against the target CC version. (Mitigated by hooking `SessionStart(matcher:'compact')`.) | medium |
| OQ-6 | S2 | `PreCompact` JSON-output `additionalContext` injection not exhaustively tested end-to-end (stdout-drop rule + exit-2 block were verified). Verify cc-squash's chosen injection path live. | medium |
| OQ-7 | S2 | ~30 sibling memory-MCP repos + observability tools deduped into representative clusters, not independently code-verified; the input envelope was truncated (full ~90-repo list lost). | low |
| OQ-8 | S3 | All C1–C5 mechanism hypotheses were inferred without S1's mechanism — now superseded by the S1 cross-links. The "continue without asking" directive is the post-compaction **continuation** surface, **distinct from** S1-F13's summarization system prompt. | medium |
| **OQ-9** | S3 | **Unverified quantitative user claims:** the "~55K premature threshold (VS Code)" (C2), the desired "150000 threshold" (#10948, C4), and "pct=80" (C4). S1 confirms the env var exists + the exact math `min(floor(window*pct/100), window−13000)`; the surface numbers (esp. 55K) remain **user-reported, not measured**. | medium |
| OQ-10 | S3 | Corroboration spans GitHub+Reddit **only** (HN + official forum not sampled). Most reddit weight is two posts (1piny6t carries C5/C4; low-engagement 1ro7rw4 carries C1/C2/C3). Three intended reddit sources were unretrievable (403/deleted). The VS Code-vs-CLI disable gap (C4) is single-thread-sourced → verify against the current extension build. | medium |
| **OQ-11** | S4 | **Open design question:** bolting a constraint-aware keep/drop gate on top of a domain-agnostic LLMLingua-2-style token classifier — the text-in/text-out compressors have **no mechanism to protect** user constraints/decisions/in-flight work; perplexity-LM variants also need a local scorer LM at edit time. | medium |
| **OQ-12** | S4 | **Forgetting is underspecified in prior art** (CoALA calls unlearning "understudied"). Only MemoryBank Ebbinghaus decay, Mem0 DELETE, and Zep bi-temporal invalidation prescribe a concrete mechanism — cc-squash's hardest requirement has the thinnest direct prior art. | **high** |
| OQ-13 | S4 | Reconstructed-from-truncated citations to verify: PyramidInfer (`2405.12532`), FastGen's ~40% figure, Reflexion (`2303.11366`), Gisting (`2304.08467`). All in the negative/inapplicable buckets, so low report-impact. | low |
| **OQ-14** | S5 | Real-API behavior of the transferable kernel (F3 ladder + F7 budget) against **actual Anthropic token counts** not exercised — transferability assessed structurally. The crude `len*4` estimator must be replaced with `count_tokens` / the anthropic tokenizer. | medium |
| OQ-15 | S5 | Several bioqa passes classified RAG-specific by spot-check, not exhaustive audit (ViewImage/Image, Diff, ToolCall, Error, Forced, Inclusion). F8 (LLMlingua-2 server) and F7 (budget) reconstructed from a truncated reader envelope but **fully re-verified from code**. | low |

**Print-mode probe limitation (restated for prominence):** the entire live-sequence picture rests on observing an **interactive TTY** session. Everything cc-squash's test harness needs from the live path (`compact_boundary` ordering, the exact injected continuation-directive surface, live `tengu_*` telemetry) is **only** observable interactively `[S1-F16, OQ-1]`. Plan the test harness around a pty, not `-p`.
