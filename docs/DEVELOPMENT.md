# Development guide

How to actually work on CodeRoom locally. For higher-level concerns
(commit style, branching, releases), see [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## Toolchain

```bash
# Rust — version pinned in rust-toolchain.toml (currently 1.85)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add rustfmt clippy

# Engine CLIs that CodeRoom drives
npm install -g @anthropic-ai/claude-code
npm install -g @openai/codex          # codex CLI 0.128+
npm install -g @google/gemini-cli     # gemini CLI 0.32+

# Useful auxiliaries
sudo apt-get install -y jq shellcheck timeout
```

Verify:

```bash
cargo --version          # cargo 1.82+
claude --version         # 2.1.137+
codex --version          # 0.128.0+
gemini --version         # 0.32.1+
```

## Standard local loop

```bash
cargo fmt --all                                                        # format
cargo clippy --all-targets --all-features -- -D warnings               # lint
cargo test --all-features --locked                                     # fast tests
```

This is the same triad CI runs. If all three pass locally, your PR will
not fail on `fmt` / `clippy` / `test`.

## Running real-engine tests

Integration tests that spawn an actual `claude` / `codex` / `gemini`
subprocess are gated behind `#[ignore]` because they cost API tokens and
require a network connection.

```bash
# Run only the ignored (real-engine) tests
cargo test -- --ignored

# Run a specific real-engine test
cargo test -- --ignored adapter::cc::smoke
```

Budget guard: every real-engine test must pass `--max-budget-usd` to its
spawned subprocess (CC) or its config equivalent (codex/gemini). A single
test should cost at most a few cents.

## Spike harness

`spike/` contains the original feasibility scripts. They're kept around
because:

- They document, executably, what the architecture assumes about each
  engine's CLI surface.
- New engine additions can copy this style for their own L1/L2/L3
  verification before writing an adapter.

Run them anytime:

```bash
cd spike
bash L1-permission-deny.sh
bash L2-stream-injection.sh
bash L3-codex-mcp.sh
```

CI runs `shellcheck` on these scripts; keep them lint-clean.

## Project layout

```
src/
├── main.rs              # `cr` binary entry; thin clap dispatcher
├── lib.rs               # library root (re-exports stable public API)
├── crep.rs              # CodeRoom Event Protocol — typed event enum
└── adapter/
    ├── mod.rs           # EngineAdapter trait, RoleHandle
    ├── cc.rs            # Claude Code adapter
    ├── codex.rs         # (planned for v0.2)
    └── gemini.rs        # (planned for v0.2)

tests/
├── cc_adapter_smoke.rs  # real claude smoke test, #[ignore]
└── ...                  # more as features land

docs/
├── architecture.md            # v0.1 constitution — read first
├── spike-2026-05-09.md        # feasibility report
├── proposed-amendments.md     # any deviation from architecture lands here first
└── DEVELOPMENT.md             # this file
```

## Debugging tips

### Tracing logs

CodeRoom uses `tracing` for structured logs. Enable verbose output:

```bash
RUST_LOG=coderoom=debug cr start
RUST_LOG=coderoom::adapter::cc=trace cr start   # one module only
```

### Inspecting a session's event stream

The message bus appends to `.coderoom/messages.jsonl` in your project.
Tail it from another terminal while `cr start` is running:

```bash
tail -f .coderoom/messages.jsonl | jq .
```

### Reproducing a stuck role

If `@backend` looks frozen:

1. Check `.coderoom/sessions/backend.state.json` — does it show an
   in-flight tool call?
2. Tail `.coderoom/transcripts/<today>/backend-<session>.jsonl` —
   what was the last raw event from the engine?
3. `/refresh backend` rebuilds the role from priors + patches +
   journal; this is the recommended escape hatch, not killing the
   wrapper.

## Performance baselines

(Filled in as v0.1 stabilizes — first measurements expected after
the CC adapter lands.)

| Operation                          | Target         |
| ---------------------------------- | -------------- |
| `cr` cold start                    | < 50 ms        |
| Spawn one role + RoleStarted event | < 1.5 s        |
| Per-turn wrapper overhead          | < 5 ms         |
| Memory at 5 active roles, idle     | < 50 MB        |
