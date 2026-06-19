# cc-squash

![cc-squash banner](https://github.com/yasyf/cc-squash/raw/main/docs/assets/readme-banner.webp)

[![PyPI](https://img.shields.io/pypi/v/cc-squash.svg)](https://pypi.org/project/cc-squash/)
[![Python](https://img.shields.io/pypi/pyversions/cc-squash.svg)](https://pypi.org/project/cc-squash/)
[![Docs](https://img.shields.io/github/actions/workflow/status/yasyf/cc-squash/docs.yml?branch=main&label=docs)](https://yasyf.github.io/cc-squash/)
[![License: PolyForm-Noncommercial-1.0.0](https://img.shields.io/badge/License-PolyForm-Noncommercial-1.0.0-blue.svg)](https://github.com/yasyf/cc-squash/blob/main/LICENSE)

Augmented auto-compaction for long-running Claude Code sessions.

cc-squash steps into the moment Claude Code compacts its context and decides what
survives the cut, keeping the constraints you set, the decisions you made, and the
files still in flight instead of flattening the whole session into one lossy summary.
Because it runs at the compaction boundary, a long session keeps its thread instead
of restarting from a summary that forgot why.

## Install

No install needed — run everything through [uvx](https://docs.astral.sh/uv/):

```bash
uvx cc-squash --help
```

`uvx` fetches cc-squash into a throwaway environment and runs it. To add it
to a project instead:

```bash
uv add cc-squash
```

## Quickstart

Confirm the CLI runs:

```bash
$ uvx cc-squash hello
Hello from cc-squash!
```

That `hello` command is the starting point; the compaction hook that wires cc-squash
into a live session lands next. Run `uvx cc-squash --help` to see the current commands.

## What problems does this solve?

- Built-in compaction compresses a constraint you set hundreds of messages ago with the
  same weight as a stale directory listing. cc-squash ranks what to keep, so decisions and
  constraints outlive the cut.
- After a compaction, the model re-asks questions you already answered and reopens choices
  you already settled. cc-squash carries the settled parts through verbatim.
- Compaction is otherwise a black box that hides what it dropped and gives you no way to
  steer it. cc-squash makes the kept, summarized, and dropped split inspectable, so you can
  tune what each session holds onto.

## Docs

[Read the docs](https://yasyf.github.io/cc-squash/) for the full guide and API reference.
