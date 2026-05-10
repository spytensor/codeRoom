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
cargo --version          # cargo 1.85+
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
cargo test --test cc_adapter_smoke -- --ignored
```

Budget guard: every real-engine test must pass `--max-budget-usd` to its
spawned subprocess (CC) or its config equivalent (codex/gemini). A single
test should cost at most a few cents.

Permission guard: default tests cover the policy classifier and the CC
stdin pacing regression. Before changing hook behavior, run the ignored
Claude smoke manually with a low-value prompt and inspect
`.coderoom/messages.jsonl` for `tool_call_proposed`,
`permission_denied`, and `tool_call_executed` events.

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

## Visual / TUI changes need screenshot verification

**`cargo test` cannot validate layout.** It catches type errors and
dataflow bugs but not "this picker bleeds across rows on a 200-col
terminal" — and that's exactly the class of regression that just shipped
and broke the role expansion picker. Anything that touches a code path
the user looks at must be eyeballed in a real terminal before merge.

PRs that modify any of:

- `src/init.rs` (the wizard, role / engine pickers, project summary)
- `src/repl.rs::print_home` (the boot dashboard)
- `src/repl.rs::ThinkingSpinner`, tool-trace rendering, role turn
  formatting
- `src/output.rs` palette or helpers
- the CLI's first-run abort screen (`src/engines.rs`)

must include screenshots at three terminal widths and call out any
visible truncation:

- [ ] **80 × 24** — default xterm / SSH session
- [ ] **120 × 40** — modern desktop terminal
- [ ] **60 × 20** — split pane / mobile SSH

The two PNGs embedded in `README.md` are generated separately from the
real-TUI verification screenshots. Refresh them with:

```bash
make readme-images
```

The command runs `scripts/render-readme-images.py`, a Pillow renderer that
keeps the README hero images reproducible without depending on a live
interactive session, VHS, freeze, silicon, or desktop screenshot state.

For the role expansion picker specifically, run:

```bash
cargo test --lib picker_visual_smoke -- --nocapture --ignored
```

This renders the picker rows at 60 / 80 / 120 columns to stderr. Eyeball
the alignment; descriptions should truncate with `…` at narrow widths
and never wrap to a second line.

**The visual smoke is necessary but not sufficient.** It bypasses raw
mode and prints with `eprintln!`, which translates LF to the
terminal's preferred line ending. The actual wizard runs in raw mode
where `\n` does NOT return to column 0 — a layout that looks fine in
the smoke can still drift diagonally on real `cr init`. Always
manually run `cr init` in an empty directory and watch the picker
render before merging. This lesson is paid for in v0.1.7 → v0.1.10.

For the no-engine abort screen, force-trigger it with:

```bash
PATH=/tmp ./target/debug/cr
```

A row that wraps is a regression even if the test suite is green.

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
