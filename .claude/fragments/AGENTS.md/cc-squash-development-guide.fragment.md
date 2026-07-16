# cc-squash Development Guide

A live cache-economics rewriting proxy for long-running Claude Code sessions. The user-facing CLI is the Go `ccs` binary; the data plane is the Rust `ccs-proxy`.

## Repository Structure

```
cc-squash/
├── crates/           # Rust data plane: ccs-proxy (relay + L0/L1/L2), ccs-policy (pure
│                     # decision engine), ccs-economics, ccs-refs, ccs-summarizer, ccs-core
├── go/               # Go control plane: the ccs CLI, daemon, and proxy supervision (fusekit)
├── bin/              # Local build drop for live runs (ccs + ccs-proxy siblings)
├── .github/          # GitHub Actions workflows
├── AGENTS.md         # This file — rendered by cc-guides; edit .claude/fragments/AGENTS.md/*
└── README.md         # Project overview
```

## Engine Layering (format-core)

Byte-level format selection — which encoding is leanest for a payload — is owned by cc-context's `format-core` crate, consumed as a cargo git dependency **tag-pinned** to cc-context releases (`crates/Cargo.toml`). Token accounting and cache economics stay downstream in `ccs-economics`; never re-implement selection locally. Adopting an engine change is always a deliberate tag bump; format-core's golden corpus gates its behavior upstream.
