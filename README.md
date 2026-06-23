# cc-squash

![cc-squash banner](https://github.com/yasyf/cc-squash/raw/main/docs/assets/readme-banner.webp)

[![PyPI](https://img.shields.io/pypi/v/cc-squash.svg)](https://pypi.org/project/cc-squash/)
[![Python](https://img.shields.io/pypi/pyversions/cc-squash.svg)](https://pypi.org/project/cc-squash/)
[![Docs](https://img.shields.io/github/actions/workflow/status/yasyf/cc-squash/docs.yml?branch=main&label=docs)](https://yasyf.github.io/cc-squash/)
[![License: PolyForm-Noncommercial-1.0.0](https://img.shields.io/badge/License-PolyForm--Noncommercial--1.0.0-blue.svg)](https://github.com/yasyf/cc-squash/blob/main/LICENSE)

Augmented auto-compaction for long-running Claude Code sessions.

cc-squash steps into the moment Claude Code compacts its context and decides what survives the cut. It ranks the constraints you set, the decisions you made, and the files still in flight ahead of stale directory listings, so a long session keeps its thread instead of restarting from a lossy summary that forgot why. The kept, summarized, and dropped split stays inspectable, so you can tune what each session holds onto.

## Install

Run it with [uvx](https://docs.astral.sh/uv/): `uvx cc-squash --help`.

## Quickstart

```bash
$ uvx cc-squash hello
Hello from cc-squash!
```

`hello` is the starter command; run `uvx cc-squash --help` for the current surface.

## Docs

[Read the docs](https://yasyf.github.io/cc-squash/) for the full guide and API reference.
