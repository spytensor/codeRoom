# CodeRoom Color Specification — v0.1

> **Status:** v0.1, locked. Adjustments from the cross-validation pass on
> 2026-05-09 are integrated. Further changes go through
> `docs/proposed-amendments.md` first, not into code.

## 1. Goals

- **Recognition.** Every role is identifiable at a glance; role colors don't
  collide perceptually.
- **Readability.** Body text and role tokens clear **WCAG AA 4.5:1** on the
  three common dark terminal backgrounds we test against (`#0d0d0e`,
  `#1e1e1e`, One Dark `#282c34`). The `dim` tier clears AA-UI 3:1. The
  `fade` and `rule` tiers are deliberately sub-AA — they're decoration, not
  information.
- **Semantic stability.** One color, one meaning. (The v1.0 draft's
  deliberate `key ≡ honey` and `prompt ≡ jade` collisions are dropped — see
  §3.)
- **Cross-terminal consistency.** Truecolor (24-bit RGB), not ANSI 8.
  iTerm2, Alacritty, Kitty, Wezterm, Ghostty, and Windows Terminal support
  it by default. tmux ≥ 3.2 needs it enabled explicitly (see §7).
- **Graceful degradation.** `NO_COLOR=1`, non-TTY stdout, and `TERM=dumb`
  silently fall back to plain text with glyphs intact.

## 2. Role palette (8 slots)

Defined in this order. FNV-1a hash of the role name picks the slot:

| slot | name | hex | notes |
|---|---|---|---|
| 0 | lavender | `#c0a8ff` | `@host` is pinned here; **does not** participate in the hash |
| 1 | jade | `#5dcaa5` | |
| 2 | coral | `#f0997b` | |
| 3 | rose | `#f09080` | adjusted from `#f09595` to avoid tritanopia collapse with blossom |
| 4 | sky | `#85b7eb` | |
| 5 | blossom | `#e088c4` | adjusted from `#ed93b1`: deuteranopia ΔE76 vs jade was 3.4 (collapse); now ~14 |
| 6 | honey | `#f4c775` | |
| 7 | teal | `#7bc6c1` | |

**Rules:**

- `@host` is always lavender. The host role is the most-seen color in any
  session; pinning it protects user memory from drifting when other roles
  are added.
- All other roles map via `fnv1a(role_name) % 7` into slots 1..8.
- Beyond 8 roles the slots collide. We accept that. The "6–8 distinct color
  recall" ceiling is a CLI-tooling rule of thumb, not a scientific law;
  glyphs and the role name (`@backend`, `@auth`) carry recognition even
  when colors duplicate. If real-world projects routinely exceed this, a
  per-role `color =` override in `config.toml` is the next step — not a
  larger palette.
- The earlier "HSL S ≈ 50–60% to avoid fluorescence" claim was wrong: only
  jade lands in that band. The actual control variable for "no neon" is
  **lightness** (these are L 70–83% pastels). The doc no longer claims a
  saturation band.

**Hash choice.** Hand-rolled FNV-1a (32-bit), five lines, no dependency.
We do **not** use `std::hash::DefaultHasher` — its output isn't stable
across Rust toolchain versions, which would silently break "the same role
keeps the same color forever."

## 3. Semantic palette

Used strictly for their named purpose; never reused for role coloring:

| name | hex | AA on `#0d0d0e` / `#1e1e1e` / OneDark | use | example site |
|---|---|---|---|---|
| `ok` | `#97c459` | 9.6 / 8.2 / 6.9 ✓ | success | `✓ added role @backend` |
| `warn` | `#ef9f27` | 8.9 / 7.7 / 6.4 ✓ | attention, non-fatal | `⚠ cap reached`, `⟳ refreshing` |
| `bad` | `#ea5b5b` | 5.5 / 4.7 / 3.9 † | failure | `✗ no such role` |
| `info` | `#6fa8dc` | 7.7 / 6.6 / 5.5 ✓ | neutral hint, auto-routing | `↳ auto-routing to @x` |
| `key` | `#d4b87a` | 9.0 / 7.7 / 6.5 ✓ | commands, hotkeys | `/help`, `cr update` |
| `prompt` | `#58c39c` | 8.9 / 7.7 / 6.4 ✓ | input prompt | `cr ›` |
| `em` | `#f0f0f0` | 17.4 / 14.9 / 12.5 ✓ | emphasis | API paths, key values |
| `text` | `#d4d4d4` | 13.3 / 11.4 / 9.6 ✓ | body | normal message text |
| `mute` | `#9a9a9a` | 5.7 / 4.9 / 4.1 ‡ | secondary | timestamps, side labels |
| `dim` | `#828282` | 4.6 / 3.9 ‡ / 3.3 ‡ | system rows | tool-call summaries |
| `fade` | `#4a4a4a` | 2.2 / 1.9 / 1.6 — sub-AA by design | decoration | `·` separator, `↳` glyph |
| `rule` | `#3a3a3d` | 1.7 / 1.4 / 1.2 — sub-AA by design | borders | reserved for future box drawing |

✓ AA-text (4.5:1) on every common dark background.
† AA-UI (3:1) on One Dark, AA-text on the others. `bad` is a high-frequency
failure indicator; we accept the One Dark gap rather than push it brighter
into pink.
‡ AA-UI on the marked background, AA-text on the lighter backgrounds.

**What changed from the v1.0 draft:**

- Dropped the deliberate `key ≡ honey` (`#f4c775`) and `prompt ≡ jade`
  (`#5dcaa5`) collisions. The boot dashboard at `repl.rs:579` and `:583`
  already paints `@role` and `/patch` tokens within the same panel; once
  both became truecolor honey, the perceptual collision the rule was
  designed to prevent would happen on first launch. `key` and `prompt`
  each get their own hex now.
- `bad`, `dim`, `mute`, `rule` adjusted to satisfy contrast on `#1e1e1e`
  and One Dark.
- `fade` and `rule` no longer claim AA. They're decorative: a `·`
  separator and the future box border conveys structure through position,
  not color.

## 4. Glyph rules

Color and glyph are bound — visual redundancy helps colorblind users:

- `✓` — `ok`
- `✗` — `bad`
- `⚠` — `warn`
- `⟳` — `warn` (refresh)
- `⊘` — `warn` (permission denied)
- `●` — current role color (boot role list)
- `↳` — `fade` (tool-call arrow, auto-routing arrow)
- `·` — `fade` (decorative separator)
- `›` — `prompt` (input prompt)

**No emoji.** A `grep` of `src/` shows zero emoji today. The rule codifies
the existing practice rather than restricting it.

## 5. Typography

**Timestamps.** `mute` color, fixed `HH:MM` width (5 chars), at the very
left of the line, followed by two spaces:

```
14:22  @host this spans @backend...
14:23  @backend plan:
```

**`@role` token.** Color + bold. Body text is not bold.

**Secondary lines** indent by 2 spaces and step the color down one tier
(npm / pnpm / cargo convention):

```
✓ patched @backend → .coderoom/patches/...
  applies to next /refresh; current session unchanged
```

**Tool calls.** Prefix `  ↳ @role · `, `fade` arrow + `dim` text, **no
timestamp**:

```
14:22  @backend let me check verify_token...
  ↳ @backend · read internal/auth/verify_token.go
  ↳ @backend · grep "gateway" config/
14:23  @backend done. plan:
```

**Blank line between role turns** — different roles separated by a blank
line. Same role continuing across multiple events: no blank line.

**Out of scope for v0.1**: continuation-line alignment for multi-line LLM
bodies, partial-line repaint for streaming tokens, embedded ANSI stripping
in tool output, terminal-width reflow. These belong in
`proposed-amendments.md`.

## 6. Rust implementation: `src/output.rs`

A new central module. All **non-raw-mode** user-facing prints in
`src/repl.rs` route through it. Centralization isn't a hygiene fetish —
it's so that adjusting a color later touches one file instead of forty.

```rust
//! Centralized output styling. Non-raw-mode user-facing prints route
//! through here so the palette and semantic rules live in one place.
//! Raw-mode renderers (init wizard, in-place spinners, UiCell builders)
//! consume the constants and helpers but draw their own frames.

use crossterm::style::{Color, Stylize, StyledContent};

// ───────────────────── semantic colors ─────────────────────
pub const OK:     Color = Color::Rgb { r: 0x97, g: 0xc4, b: 0x59 };
pub const WARN:   Color = Color::Rgb { r: 0xef, g: 0x9f, b: 0x27 };
pub const BAD:    Color = Color::Rgb { r: 0xea, g: 0x5b, b: 0x5b };
pub const INFO:   Color = Color::Rgb { r: 0x6f, g: 0xa8, b: 0xdc };
pub const KEY:    Color = Color::Rgb { r: 0xd4, g: 0xb8, b: 0x7a };
pub const PROMPT: Color = Color::Rgb { r: 0x58, g: 0xc3, b: 0x9c };
pub const EM:     Color = Color::Rgb { r: 0xf0, g: 0xf0, b: 0xf0 };
pub const TEXT:   Color = Color::Rgb { r: 0xd4, g: 0xd4, b: 0xd4 };
pub const MUTE:   Color = Color::Rgb { r: 0x9a, g: 0x9a, b: 0x9a };
pub const DIM:    Color = Color::Rgb { r: 0x82, g: 0x82, b: 0x82 };
pub const FADE:   Color = Color::Rgb { r: 0x4a, g: 0x4a, b: 0x4a };
pub const RULE:   Color = Color::Rgb { r: 0x3a, g: 0x3a, b: 0x3d };

// ───────────────────── role palette ────────────────────────
const ROLE_PALETTE: [Color; 8] = [
    Color::Rgb { r: 0xc0, g: 0xa8, b: 0xff }, // 0: lavender — host (pinned)
    Color::Rgb { r: 0x5d, g: 0xca, b: 0xa5 }, // 1: jade
    Color::Rgb { r: 0xf0, g: 0x99, b: 0x7b }, // 2: coral
    Color::Rgb { r: 0xf0, g: 0x90, b: 0x80 }, // 3: rose
    Color::Rgb { r: 0x85, g: 0xb7, b: 0xeb }, // 4: sky
    Color::Rgb { r: 0xe0, g: 0x88, b: 0xc4 }, // 5: blossom
    Color::Rgb { r: 0xf4, g: 0xc7, b: 0x75 }, // 6: honey
    Color::Rgb { r: 0x7b, g: 0xc6, b: 0xc1 }, // 7: teal
];

/// FNV-1a 32-bit. Stable across Rust toolchains, unlike DefaultHasher.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Stable role color. `@host` is pinned to lavender; others hash to slots 1..8.
pub fn role_color(role: &str, host_role: &str) -> Color {
    if role == host_role {
        return ROLE_PALETTE[0];
    }
    let idx = 1 + (fnv1a(role) as usize) % 7;
    ROLE_PALETTE[idx]
}

// ───────────────────── status helpers ──────────────────────
// One-shot status lines print directly. These are the high-frequency
// sites in repl.rs we want to migrate first.
pub fn ok(msg: impl AsRef<str>) {
    println!("{} {}", "✓".with(OK), msg.as_ref().with(TEXT));
}
pub fn warn(msg: impl AsRef<str>) {
    println!("{} {}", "⚠".with(WARN), msg.as_ref().with(TEXT));
}
pub fn bad(msg: impl AsRef<str>) {
    println!("{} {}", "✗".with(BAD), msg.as_ref().with(TEXT));
}
/// Indented secondary line, color steps down to FADE.
pub fn hint(msg: impl AsRef<str>) {
    println!("  {}", msg.as_ref().with(FADE));
}
/// `[@role ready]` / `[@role stopped: ...]` — system bracket.
pub fn system(msg: impl AsRef<str>) {
    println!("{}", format!("[{}]", msg.as_ref()).with(DIM).italic());
}
/// `  ↳ @role · summary` — tool call trace, no timestamp.
pub fn tool_trace(role: &str, summary: impl AsRef<str>) {
    println!(
        "  {} @{role} · {}",
        "↳".with(FADE),
        summary.as_ref().with(DIM),
    );
}

// ───────────────────── fragment helpers ────────────────────
// Return styled content for callers that build their own lines
// (UiCell builders, multi-token rows). This is the bridge the v1.0
// draft missed — without it, the home dashboard cannot migrate.
pub fn role_token(role: &str, host_role: &str) -> StyledContent<String> {
    format!("@{role}").with(role_color(role, host_role)).bold()
}
pub fn role_dot(role: &str, host_role: &str) -> StyledContent<&'static str> {
    "●".with(role_color(role, host_role))
}
pub fn cmd(text: impl Into<String>) -> StyledContent<String> {
    text.into().with(KEY)
}

/// Caller controls placement (rustyline / tokio stdout); we just style.
pub fn prompt() -> String {
    format!("\n{} ", "cr ›".with(PROMPT))
}

// ───────────────────── degradation ─────────────────────────
/// True when colored output is appropriate. v0.1 relies on crossterm's
/// own should_colorize behaviour for TTY detection; explicit NO_COLOR
/// gating is v0.1.x.
pub fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() { return false; }
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() { return false; }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}
```

**Why this shape:**

- `ok`/`warn`/`bad`/`hint`/`system`/`tool_trace` are the ~20 hot
  sites in `repl.rs` (status lines, journal/refresh/patch results,
  tool traces). They print directly because that's all callers want.
- `role_token`/`role_dot`/`cmd` return styled fragments because
  `repl.rs::pair_line` and friends build their own rows and need the
  styled string + visible width separately. The v1.0 draft missed
  this and the home dashboard would have been unmigratable.
- `prompt` returns a `String` rather than printing — the REPL writes
  through async stdout in a specific order with flushing, and pulling
  that into `output` would entangle the module with tokio.
- `role_speak` is **deliberately absent**. Multi-line bodies, ANSI
  passthrough, streaming, and width-wrap are unsolved; locking an API
  shape now would just create future churn.

## 7. Compatibility / degradation

**Default**: truecolor. `crossterm::style::Color::Rgb` emits
`\x1b[38;2;R;G;Bm`.

**tmux caveat**: tmux ≥ 3.2 does **not** pass truecolor through by
default. Users need this in `~/.tmux.conf`:

```
set -as terminal-features ',xterm-256color:RGB'
```

Without it tmux quantizes to 256 colors and the carefully tuned L/S
distances of this palette silently degrade. Note this in the README's
troubleshooting section.

**Degradation**: `NO_COLOR=1`, non-TTY stdout, `TERM=dumb` — defer to
crossterm's `should_colorize` behavior. `Stylize::with(Color::...)` is
expected to emit empty SGR in those environments. Glyphs (`✓` `✗` `↳`)
remain so `grep`/`tee` consumers retain structure.

**Diagnostics**: startup terminal probing is opt-in. Use
`CODEROOM_TERMINAL_PROBE=1 cr` when collecting a color-rendering report.

**Out of scope for v0.1**: actively quantizing to 256 colors when
`COLORTERM != "truecolor"`. crossterm emits 24-bit SGR even on 256-color
terminals; quantization is the terminal's responsibility. Filed under
`proposed-amendments.md` if it becomes a real complaint.

## 8. Acceptance criteria

- [ ] `cr start` shows `@host` in `#c0a8ff` (lavender).
- [ ] A user-added `@backend` keeps the same color across restarts (FNV-1a
      is deterministic across Rust toolchains).
- [ ] **All non-raw-mode user-facing prints in `repl.rs` route through
      `output::*`**, with these explicit carve-outs:
      - `repl.rs::ThinkingSpinner` (~lines 716–779) draws in place via
        `\r\x1b[2K`. The `println!`-based helpers in `output` would break
        the in-place repaint. The spinner consumes `output::DIM` directly.
      - `repl.rs::{full_line, pair_line, heading_cell, label_cell,
        styled_cell, role_profile_cell, top_border, …}` build `UiCell`
        rows. They use `output::role_token`/`role_dot`/`cmd` plus the
        palette constants but assemble lines locally.
      - `init.rs::{run_wizard, WizardTerminal, render_*, push_*}` is a
        raw-mode buffered TUI; it imports `output::role_color` (replacing
        its private `role_color` at `init.rs:1338`) but keeps its own
        `queue!` / `execute!` rendering path.
- [ ] **`cost.rs` and `role.rs` are not in scope** — they have zero
      styled-output calls today and are already compliant.
- [ ] `repl.rs:692` (`↳ auto-routing`), `:1122` (`↳ tool proposed`), and
      `:1131` (`✓/✗ tool executed`) currently use `.dim()`; after
      migration the `↳` glyph specifically uses `output::FADE` per §4.
- [ ] `repl.rs:1141` `⊘` currently uses `.yellow()`; after migration it
      uses `output::WARN`.
- [ ] Role turns are separated by a blank line; consecutive lines from
      the same role are not.
- [ ] Timestamps render as `HH:MM` (5 chars), `mute` color, line start.
- [ ] Tool-call lines use `  ↳ @role · ` with **no** timestamp.

`NO_COLOR=1` end-to-end coverage and explicit 256-color quantization are
**not** part of v0.1 acceptance — they ride on crossterm's defaults for
now and graduate to v0.1.x.

## 9. Cross-validation record (2026-05-09)

The four points carried in from the cross-validation pass:

1. Six palette hex adjustments (contrast and CVD-discriminability).
2. `key`/`honey` and `prompt`/`jade` collisions split — the "never
   co-occur" rule was already false on the boot dashboard.
3. FNV-1a replaces `DefaultHasher` (cross-toolchain stability).
4. Added `role_token` / `role_dot` / `cmd` fragment helpers — without
   them, `UiCell`-building call sites can't migrate.
5. Acceptance #3 explicitly carves out the init wizard, the spinner, and
   `UiCell` builders rather than pretending they migrate cleanly.
6. `role_speak` deferred (multi-line / streaming / wrap unsolved).
7. `cost.rs` / `role.rs` removed from the migration list (already
   compliant).
8. Dropped the "all colors AA 4.5:1" and "S ≈ 50–60%" claims — neither
   was true; replaced with a per-background contrast table.
9. tmux truecolor deployment requirement noted in §7.
