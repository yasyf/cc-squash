# Changelog

All notable changes to this project are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.12.1] - 2026-08-31

Supersedes v0.12.0, which was tagged but published nothing: its release smoke
test refused the daemon on the control lane, so the draft was never promoted and
no artifacts or formula ever shipped under that version. The tag remains because
this repository forbids deleting one.

### Changed
- Move the control plane onto daemonkit v0.23. `ccs daemon` runs through
  `daemonkit.Serve` with the product as daemonkit's `Product`, clients reach it
  over one persistent `daemonkit.Open(...).Business()` session, and
  `ccs service install` / `ccs service uninstall` converge through
  `Client.Ensure` and `Client.Stop` instead of driving launchd themselves.
- Publish the running release over daemonkit's trust-gated control lane, so a
  launcher can order an incumbent against its own build without opening a
  business session.
- Spawn `ccs-proxy` over a daemonkit `ChannelHandoff`. The child inherits the
  seam socketpair at fd 3 and no longer takes `--socket`, and the daemon serves
  exactly one child channel at a time.
- Move the daemon socket, ownership record, and start lock to
  `~/.daemonkit/a/com.yasyf.cc-squash.daemon`; cc-squash's own state stays in
  `~/.cc-squash`. Nothing migrates the old location, so stop the running daemon
  before upgrading and then run `ccs service install`.

### Removed
- Drop the `proxy-v1.sock` listener, its file lock and single-entrant guard,
  and the four-field peer-identity match run against every accepted child.
  The handoff channel comes from the spawn itself, so nothing is left to dial,
  lock, or re-identify.

## [0.11.0] - 2026-07-24

### Changed
- Pin daemonkit v0.18.0 and dispatch its exact verifier child before CLI parsing,
  so runtime startup proves its bounded trust-verification lane before serving.

## [0.10.2] - 2026-07-24

### Fixed
- Verify the release-stamped `ccs` commit decoration together with the shared
  `ccs`/`ccs-proxy` version before publishing the archive.

## [0.10.1] - 2026-07-24

### Fixed
- Stamp `ccs` and `ccs-proxy` from the same release tag and verify the packaged
  pair before publishing, preventing same-release proxy replacement loops.

## [0.10.0] - 2026-07-24

### Changed
- Pin daemonkit v0.17.4 so runtime drain settles admitted requests and terminal
  transport acknowledgements before socket teardown.

## [0.9.0] - 2026-07-24

### Changed
- Pin daemonkit v0.17.2 for exact admitted-publication resolution.

### Fixed
- Resolve every business wire callback through the publication admitted with
  that request, instead of retaining the server assembled before activation.

## [0.6.2] - 2026-07-23

### Fixed
- Bind draft staging to the exact created release ID and use the concurrent
  tap publisher so release creation cannot race tag-based discovery.

## [0.6.1] - 2026-07-23

### Fixed
- Correct the downloaded universal-binary architecture check and keep
  Gatekeeper assessment out of bare command-line executable validation.

### Added
- Initial scaffolding.

### Changed
- Pin daemonkit v0.9.0 for the exact fleet-wide runtime hard cut.
- Hard-cut service convergence to daemonkit v0.10.0 with an exact canonical
  program and a fresh replacement-fenced controller-state epoch.

[Unreleased]: https://github.com/yasyf/cc-squash/compare/v0.12.1...HEAD
[0.12.1]: https://github.com/yasyf/cc-squash/compare/v0.11.1...v0.12.1
[0.11.0]: https://github.com/yasyf/cc-squash/compare/v0.10.2...v0.11.0
[0.10.2]: https://github.com/yasyf/cc-squash/compare/v0.10.1...v0.10.2
[0.10.1]: https://github.com/yasyf/cc-squash/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/yasyf/cc-squash/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/yasyf/cc-squash/compare/v0.8.0...v0.9.0
[0.6.2]: https://github.com/yasyf/cc-squash/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/yasyf/cc-squash/compare/v0.6.0...v0.6.1
