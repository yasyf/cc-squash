# Changelog

All notable changes to this project are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/yasyf/cc-squash/commits/main
[0.6.1]: https://github.com/yasyf/cc-squash/compare/v0.6.0...v0.6.1
