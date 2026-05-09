# Contributing to CodeRoom

Thank you for your interest. CodeRoom's design is opinionated and the
architecture is locked at v0.1 (see [`docs/architecture.md`](docs/architecture.md)).
This document covers how to land a change without surprises.

## Ground rules

1. **Read the constitution first.** `docs/architecture.md` lists locked
   decisions. PRs that contradict locked decisions must accompany — or
   follow — an entry in
   [`docs/proposed-amendments.md`](docs/proposed-amendments.md).
2. **One concern per PR.** A code change *and* a doc change to a different
   area belong in two PRs.
3. **CI must be green.** `fmt`, `clippy`, `test`, and `shellcheck` jobs are
   mandatory. There is no "land yellow" path.
4. **Conventional commits.** Both individual commits *and* PR titles use the
   conventional commits format (see below).

## Local setup

```bash
# Rust toolchain — pinned via rust-toolchain.toml
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# (rustup will pick up rust-toolchain.toml automatically the first time
#  you run a cargo command in this repo)

# Build, lint, test
cargo build --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --locked
```

See [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) for engine-specific setup
(Claude Code, Codex, Gemini CLIs) and how to run real-engine integration
tests locally without burning unbounded API spend.

## Branches

- `main` — always green, always releasable. Direct push is allowed only
  for the project owner; everyone else opens a PR.
- Topic branches — name like `feat/adapter-codex` or `fix/repl-mention-parse`.
  No long-lived feature branches; rebase on `main` regularly.
- Tags `vX.Y.Z` for releases. Pre-release tags use `vX.Y.Z-rcN`.

## Commit message format

[Conventional Commits 1.0](https://www.conventionalcommits.org/en/v1.0.0/).

```
<type>(<scope>): <subject>

<body>

<footer>
```

| `<type>` | When to use                                            |
| -------- | ------------------------------------------------------ |
| `feat`   | New user-visible capability                            |
| `fix`    | Bug fix                                                |
| `docs`   | Documentation only                                     |
| `chore`  | Tooling, deps, scaffolding (not user-visible)          |
| `refactor` | Internal restructure, no behavior change             |
| `test`   | Tests only                                             |
| `ci`     | CI/CD config                                           |
| `perf`   | Performance improvement                                |

`<scope>` is a short module identifier: `adapter-cc`, `crep`, `repl`, `bus`,
`docs`, `ci`, etc.

Examples:

```
feat(adapter-cc): emit ToolCallProposed/Executed events
fix(repl): preserve trailing whitespace in @-mention parser
docs(architecture): clarify host role escalation rules
chore(deps): bump tokio to 1.42
```

A trailing line of `Co-Authored-By:` is encouraged when AI assistants
contribute non-trivial work.

## PR review checklist

A PR is ready to merge when:

- [ ] CI is green (all four jobs).
- [ ] PR description follows the template — clear summary, architecture
      impact, test plan.
- [ ] No `unsafe` code added (forbidden at workspace level — see
      [Cargo.toml](Cargo.toml) `[lints.rust]`).
- [ ] Public APIs in `src/lib.rs` are documented (`#![warn(missing_docs)]`).
- [ ] If you changed the constitution: `docs/proposed-amendments.md` was
      updated *first*, in a separate landed PR.

## Architectural amendments

If you believe a locked decision should change:

1. Open a PR that **only** edits `docs/proposed-amendments.md` with a
   one-page proposal (problem, alternatives considered, recommended change,
   migration impact).
2. Discuss in PR review. Land the amendment first.
3. *Then* open a separate PR with the implementation, referencing the
   amendment.

This keeps the constitution searchable and ensures every behavior change
has a paper trail.

## Releasing (maintainer only)

```bash
# bump version in Cargo.toml + CHANGELOG.md (move [Unreleased] → [vX.Y.Z])
cargo build --release          # sanity check
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin main vX.Y.Z
```

The release tag triggers the (future) release workflow that produces
binaries for linux + macOS.

## Code of conduct

Be precise, be kind, attack ideas not people. Issues that spiral get
closed without further comment.
