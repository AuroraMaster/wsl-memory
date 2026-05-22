# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added
- `CONTRIBUTING.md` describing the bug-report and patch workflow.
- Placeholder for upcoming reclaim-throttle config knob.

### Changed
- Host installer fallback now retries the SCM registration once before
  giving up. See `scripts/install-host.ps1`.

### Fixed
- Guest agent no longer logs `drop_caches` actions when the guest is below
  the configured headroom — the action was already gated, but the log line
  was noisy.

---

Older releases will be backfilled as their commits are tagged. The git
history is the authoritative source until then; this file exists so that
future releases have a stable place to land their notes.
