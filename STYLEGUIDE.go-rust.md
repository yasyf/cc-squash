# cc-squash Go + Rust Style Appendix

The Python scaffold (`cc_squash/`) was deleted when the Go control plane + Rust
data plane landed (build plan §7, Layer 1). The Python-specific rules in
`STYLEGUIDE.md` are **legacy** — they apply only to a possible future Python
`capt-hook` sidecar, not to `go/` or `crates/`. This file holds the concrete
rules for the two real languages. A full merge into `STYLEGUIDE.md` is a
follow-up; the workflow/orchestration rules in `AGENTS.md`/`CLAUDE.md` stay
language-agnostic and apply unchanged.

## Layout

- **`go/`** — the `ccs` control-plane binary. Module `github.com/yasyf/cc-squash/go`,
  `cmd/ccs` + `internal/`, mirroring cc-pool (`/Users/yasyf/Code/claude-pool`),
  which is the reference to port. Consumes `github.com/yasyf/fusekit` v0.5.1.
- **`crates/`** — the Cargo workspace. `ccs-proxy` is the only Layer-1 crate; the
  engine crates (`ccs-economics`/`ccs-policy`/`ccs-refs`/…) are added at their layer.

## Go

- Target Go 1.26.x. Format with **gofumpt** (superset of gofmt); CI gates `gofmt -l`.
  `go vet ./...` and `go test -race ./...` are clean before any push.
- Follow Effective Go + the surrounding cc-pool idiom you are porting. Errors wrap
  with `%w`; sentinel errors via `errors.Is`. Short-lived unix-socket clients
  (dial-per-call with a deadline) per cc-pool `internal/daemon/client.go`.
- Tests use real ephemeral resources (temp dirs/sockets) and a fake peer for the
  cross-language seam — never mock the transport. Mirror cc-pool's `*_test.go`.

## Rust

- Edition 2021. Format with `cargo fmt`; lint with `cargo clippy -- -D warnings`.
  Pattern matching for dispatch; `thiserror` for typed library errors, `anyhow`
  at the binary edge. Async-native I/O on `tokio`.
- Tests use real ephemeral resources + a mock upstream Anthropic — never mock the
  driver (tokio/the socket are real).

## THE ONE INVERSION — fail-open, do NOT "crash on the unexpected"

`AGENTS.md`/`STYLEGUIDE.md` say *"No defensive coding… crash on the unexpected."*
**`ccs-proxy`'s hot path inverts this** (build plan §5, Appendix 1): its cardinal
invariant is **fail-open to identity** — any error/timeout/panic/validation-fail/
uncertainty ⇒ forward CC's original request and relay the original response
byte-for-byte. A relay that panics is a *worse* failure than one that passes
through unchanged.

Concretely, on the RelayCore / Interceptor hot path
(`crates/ccs-proxy/src/{relay,intercept,synth,demux,auth}`):

- Every Interceptor entrypoint is sandboxed with `std::panic::catch_unwind` +
  `tokio::time::timeout`; failure ⇒ `None` ⇒ the original request is relayed.
- **No `.unwrap()` / `.expect()` on the hot path** — each is a latent fail-open
  violation. CI greps for them and fails the build (allowed only in `main.rs`
  startup, which runs before any traffic, and in `#[cfg(test)]`).
- An unknown/expired session token ⇒ transparent passthrough, **never a 404**.
- A synthesized SSE error ⇒ relay the real upstream call, **never** an empty or
  partial stream.

"Crash on the unexpected" still applies to genuine programmer-error invariants at
**startup** (e.g. a malformed `--socket` arg before any request is served). Do not
let a reviewer "simplify" a fail-open arm into a panic — that is the bug, not the fix.
