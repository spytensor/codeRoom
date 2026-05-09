# Changelog

All notable changes to CodeRoom are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- v0.1 architecture constitution (`docs/architecture.md`).
- Feasibility spike report and harness scripts (`docs/spike-2026-05-09.md`,
  `spike/L{1,2,3}*.sh`) — verified Claude Code permission deny under
  `--dangerously-skip-permissions`, long-lived stream-json session with
  prompt cache reuse, and Codex MCP-server JSON-RPC handshake.
- Project scaffolding: Cargo manifest, rustfmt/clippy config, editor config,
  rust-toolchain pin (1.82).
- GitHub Actions CI (fmt, clippy, multi-OS build+test, shellcheck) and
  Dependabot for cargo + actions.
- Issue templates (bug, feature) and PR template enforcing conventional
  commits and architecture-amendment hygiene.
- MIT license, top-level README, CONTRIBUTING, and DEVELOPMENT docs.

[Unreleased]: https://github.com/spytensor/codeRoom/compare/HEAD
