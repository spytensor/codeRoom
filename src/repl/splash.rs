use std::fmt::Write as FmtWrite;
use std::path::Path;

use crossterm::style::{Color, Stylize};

use crate::adapter::{Engine, PermissionMode};
use crate::config::Config;
use crate::output;
use crate::priors;

use super::text::truncate_inline;

#[derive(Debug, Clone)]
pub(super) struct UiCell {
    styled: String,
    visible: usize,
}

pub(super) fn plain_cell(text: impl Into<String>) -> UiCell {
    let styled = text.into();
    let visible = styled.chars().count();
    UiCell { styled, visible }
}

pub(super) fn styled_cell(plain: &str, styled: impl std::fmt::Display) -> UiCell {
    UiCell {
        styled: styled.to_string(),
        visible: plain.chars().count(),
    }
}

pub(super) fn join_cells(parts: &[UiCell]) -> UiCell {
    let mut styled = String::new();
    let mut visible = 0;
    for part in parts {
        styled.push_str(&part.styled);
        visible += part.visible;
    }
    UiCell { styled, visible }
}

pub(super) fn empty_cell() -> UiCell {
    plain_cell("")
}

// ───────────────────── boot splash ─────────────────────────
//
// The framed two-column splash printed on every `cr start` (and on the
// `/welcome` slash command). All visible columns are computed from
// `UiCell::visible`, which excludes ANSI escape bytes — that is how the
// `┌`, `│`, and `┘` glyphs stay aligned column-for-column regardless of
// how richly the inner content is styled. Right-column copy is read from
// `data/splash_content.toml` so the tips and "what's new" entries can
// move release-by-release without touching code.
//
// Width budget per row:
//   `│ ` + left(left_w) + `  ` + right(right_w) + ` │` = 6 + left_w + right_w
// We choose `width` ∈ [60, 80] from the terminal size, then split the
// inner area as left ≈ 50 % so role rows fit even at 60 cols.

const SPLASH_CONTENT_TOML: &str = include_str!("../../data/splash_content.toml");

#[derive(Debug, serde::Deserialize)]
pub(super) struct SplashContent {
    pub(super) tips: SplashTips,
    pub(super) whats_new: Vec<SplashRelease>,
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct SplashTips {
    pub(super) items: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct SplashRelease {
    pub(super) version: String,
    pub(super) items: Vec<String>,
}

pub(super) fn load_splash_content() -> SplashContent {
    toml::from_str(SPLASH_CONTENT_TOML)
        .expect("data/splash_content.toml must be valid TOML — checked in tests")
}

fn splash_engine_short(engine: Engine) -> &'static str {
    match engine {
        Engine::Cc => "cc",
        Engine::Codex => "codex",
        Engine::Gemini => "gemini",
    }
}

/// Short, fixed-width context label so role rows stay aligned even at
/// the 60-col floor. Mirrors the longer hint shown in the old dashboard
/// but trims `" context"` to keep room for the role name.
fn splash_context_short(engine: Engine, model: Option<&str>) -> &'static str {
    let normalized = model.unwrap_or_default().to_ascii_lowercase();
    match engine {
        Engine::Cc if normalized.contains("sonnet") || normalized.contains("haiku") => "200k",
        Engine::Cc => "1M",
        Engine::Codex if normalized.contains("gpt-5") => "400k",
        Engine::Gemini if normalized.contains("pro") => "1M",
        Engine::Codex | Engine::Gemini => "default",
    }
}

/// Frame piece styled with the splash teal stroke.
fn frame(s: &str) -> String {
    s.with(output::SPLASH_FRAME).to_string()
}

/// Total visible width of the splash frame.
///
/// `min(term_width − 4, 80)` clamped to a 60-col floor so the box still
/// fits on the narrowest reasonable ssh window. We subtract 4 instead of
/// 2 to leave a comfortable margin from both terminal edges.
fn splash_width() -> usize {
    let columns = crossterm::terminal::size().map_or(80, |(c, _)| usize::from(c));
    columns.saturating_sub(4).clamp(60, 80)
}

/// Split the box's inner width into a left and right column, with a
/// 2-column gap between them.
///
/// The split prefers ~40 % to the left so the right column has room for
/// tip and release-note copy without needing aggressive truncation. The
/// caller passes `role_floor` — the visible width of the widest role
/// row — and the left column never shrinks below that, so role rows
/// always fit. The right column is guaranteed at least 20 visible
/// columns; below that, headings start losing meaning.
pub(super) fn splash_columns(width: usize, role_floor: usize) -> (usize, usize) {
    let inner = width.saturating_sub(4); // `│ ` and ` │`
    let gap = 2;
    let available = inner.saturating_sub(gap);
    let preferred = (available * 4) / 10;
    let max_left = available.saturating_sub(20).max(20);
    let left = preferred.max(role_floor).min(max_left);
    let right = available.saturating_sub(left);
    (left, right)
}

/// Visible width of the longest role row that will be rendered.
/// Mirrors the layout used in `splash_role_cell`:
///   `● ␠@{role:<role_pad+1}␠␠{engine:<6}␠·␠{ctx}␠·␠{perm}`
fn splash_role_floor(cfg: &Config, role_names: &[&str], role_pad: usize) -> usize {
    role_names
        .iter()
        .map(|name| {
            let role_cfg = cfg.role_config(name, Path::new(""));
            let engine = role_cfg
                .as_ref()
                .map_or(cfg.default_engine, |role| role.engine);
            let model = role_cfg
                .as_ref()
                .and_then(|role| role.model.as_deref())
                .or(cfg.default_model.as_deref());
            let mode = role_cfg
                .as_ref()
                .map_or(cfg.permission_mode, |role| role.permission_mode);
            let ctx = splash_context_short(engine, model);
            let (perm, _) = permission_label(engine, mode);
            // ● + ' ' + '@' + role_pad + ' ' + ' ' + engine_pad(6) + ' ' + '·' + ' ' + ctx
            //   + ' ' + '·' + ' ' + perm
            1 + 1
                + 1
                + role_pad
                + 2
                + 6
                + 1
                + 1
                + 1
                + ctx.chars().count()
                + 3
                + perm.chars().count()
        })
        .max()
        .unwrap_or(28)
}

/// Per-role permission gate descriptor shown on the dashboard.
///
/// The dashboard's job here is to keep the user from assuming CodeRoom
/// gates every role's tool calls. Two states matter:
///
/// - **CR-gated** — engine is `cc` and the configured `permission_mode`
///   is `ask`/`auto`. CodeRoom's PreToolUse hook intercepts. Rendered
///   in [`output::MUTE`] (visually "covered").
/// - **No CR gate** — anything else: `bypass` on any engine, or any
///   mode on `codex`/`gemini` (CodeRoom doesn't intermediate tool calls
///   on those engines). Rendered in [`output::WARN`] so the user can
///   tell at a glance that they own the diff review.
///
/// The label itself is the literal `permission_mode` string so the
/// dashboard mirrors `cr config show` rather than introducing a new
/// vocabulary. Discrimination is by color, not by inventing terms.
fn permission_label(engine: Engine, mode: PermissionMode) -> (&'static str, Color) {
    let cr_gated = matches!(engine, Engine::Cc) && !matches!(mode, PermissionMode::Bypass);
    let color = if cr_gated { output::MUTE } else { output::WARN };
    (mode.as_str(), color)
}

/// `┌─ {title} ─...─┐` — title is embedded into the top stroke.
///
/// Visible breakdown: `┌` + `─` + ` ` + title + ` ` + N×`─` + `┐` = width.
pub(super) fn splash_top(width: usize, title: &UiCell) -> String {
    let prefix = 3; // "┌─ "
    let between = 1; // " " after title
    let suffix = 1; // "┐"
    let n = width.saturating_sub(prefix + title.visible + between + suffix);
    let mut out = String::new();
    out.push_str(&frame("┌─"));
    out.push(' ');
    out.push_str(&title.styled);
    out.push(' ');
    out.push_str(&frame(&"─".repeat(n)));
    out.push_str(&frame("┐"));
    out
}

pub(super) fn splash_bottom(width: usize) -> String {
    let mut out = frame("└");
    out.push_str(&frame(&"─".repeat(width.saturating_sub(2))));
    out.push_str(&frame("┘"));
    out
}

/// Truncate a styled cell to `max_visible` columns. Falls back to the
/// plain string when truncation is needed because we can't safely cut
/// inside ANSI escape sequences.
fn fit_cell(cell: UiCell, plain: &str, max_visible: usize) -> UiCell {
    if cell.visible <= max_visible {
        cell
    } else {
        plain_cell(truncate_inline(plain, max_visible))
    }
}

/// Pad a cell to exactly `width` visible columns by appending spaces.
fn pad_cell(cell: &UiCell, width: usize) -> String {
    let pad = width.saturating_sub(cell.visible);
    let mut out = cell.styled.clone();
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

/// `│ {left:left_w}  {right:right_w} │` — body row with two columns.
pub(super) fn splash_pair(left: &UiCell, right: &UiCell, left_w: usize, right_w: usize) -> String {
    let mut out = frame("│");
    out.push(' ');
    out.push_str(&pad_cell(left, left_w));
    out.push_str("  ");
    out.push_str(&pad_cell(right, right_w));
    out.push(' ');
    out.push_str(&frame("│"));
    out
}

/// Read a display name for the welcome line without shelling out from
/// the async REPL runtime. The splash falls back to a nameless greeting
/// when no common user-name environment variable is set.
fn git_user_name() -> Option<String> {
    ["GIT_AUTHOR_NAME", "USER", "USERNAME"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
}

/// Display a path with `$HOME` collapsed to `~`. Falls back to the raw
/// path when home dir is unavailable or the path lives outside it.
fn home_relative_display(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            let rel_str = rel.display().to_string();
            return if rel_str.is_empty() {
                "~".to_owned()
            } else {
                format!("~/{rel_str}")
            };
        }
    }
    path.display().to_string()
}

/// Build the styled "● @role  engine · ctx" cell, padded to fit the
/// left column. The bullet and role name pick up the role's stable
/// color; engine and context render in muted neutral tones.
fn splash_role_cell(
    cfg: &Config,
    coderoom_dir: &Path,
    name: &str,
    role_pad: usize,
    max_width: usize,
) -> UiCell {
    let role_cfg = cfg.role_config(name, coderoom_dir);
    let engine = role_cfg
        .as_ref()
        .map_or(cfg.default_engine, |role| role.engine);
    let model = role_cfg
        .as_ref()
        .and_then(|role| role.model.as_deref())
        .or(cfg.default_model.as_deref());
    let mode = role_cfg
        .as_ref()
        .map_or(cfg.permission_mode, |role| role.permission_mode);
    let engine_short = splash_engine_short(engine);
    let ctx = splash_context_short(engine, model);
    let (perm_label, perm_color) = permission_label(engine, mode);
    let role_paint = output::role_color(name, &cfg.host_role);
    let role_token = format!("@{name}");
    let role_padded = format!("{role_token:<width$}", width = role_pad + 1);
    let plain = format!("● {role_padded}  {engine_short:<6} · {ctx} · {perm_label}");
    let cell = join_cells(&[
        styled_cell("●", "●".with(role_paint)),
        plain_cell(" "),
        styled_cell(&role_padded, role_padded.as_str().with(role_paint).bold()),
        plain_cell("  "),
        styled_cell(
            &format!("{engine_short:<6}"),
            format!("{engine_short:<6}").with(output::MUTE),
        ),
        styled_cell(" ", " ".with(output::FADE)),
        styled_cell("·", "·".with(output::FADE)),
        plain_cell(" "),
        styled_cell(ctx, ctx.with(output::MUTE)),
        plain_cell(" "),
        styled_cell("·", "·".with(output::FADE)),
        plain_cell(" "),
        styled_cell(perm_label, perm_label.with(perm_color)),
    ]);
    fit_cell(cell, &plain, max_width)
}

/// `[ 1.0k ] base tokens loaded` — the count renders inside a teal
/// pill (background fill + dark foreground). The pill text is built
/// with literal spaces around the digits so it reads as a chip rather
/// than a colored substring.
fn splash_token_pill_cell(total_tokens: u64, max_width: usize) -> UiCell {
    let formatted = priors::format_token_count(total_tokens);
    let pill_inner = format!(" {formatted} ");
    let pill_visible = pill_inner.chars().count();
    let trailer = " base tokens loaded";
    let plain = format!("{pill_inner}{trailer}");
    let cell = join_cells(&[
        styled_cell(
            &pill_inner,
            pill_inner
                .as_str()
                .with(output::SPLASH_PILL_FG)
                .on(output::SPLASH_FRAME)
                .bold(),
        ),
        styled_cell(trailer, trailer.with(output::TEXT)),
    ]);
    debug_assert_eq!(cell.visible, pill_visible + trailer.chars().count());
    fit_cell(cell, &plain, max_width)
}

/// Pick the `[[whats_new]]` entry whose `version` matches
/// `CARGO_PKG_VERSION`, falling back to the head of the list when no
/// exact match is recorded yet (e.g. a bumped Cargo.toml without a new
/// CHANGELOG entry).
pub(super) fn pick_release<'a>(
    content: &'a SplashContent,
    version: &str,
) -> Option<&'a SplashRelease> {
    content
        .whats_new
        .iter()
        .find(|r| r.version == version)
        .or_else(|| content.whats_new.first())
}

/// Build the "● @role  engine · ctx" rows for the left column. Roles
/// are sorted alphabetically to match the rest of the dashboard.
fn splash_role_rows(
    cfg: &Config,
    coderoom_dir: &Path,
    role_names: &[&str],
    role_pad: usize,
    max_width: usize,
) -> Vec<UiCell> {
    role_names
        .iter()
        .map(|name| splash_role_cell(cfg, coderoom_dir, name, role_pad, max_width))
        .collect()
}

/// Boot splash, framed two-column edition. Shown on every `cr start`,
/// bare `cr`, and the `/welcome` slash command. The `first_run` flag
/// only swaps the greeting verb — the surrounding frame stays identical
/// so returning users see the same status surface.
pub(super) fn print_home(cfg: &Config, coderoom_dir: &Path, project_root: &Path, first_run: bool) {
    let user_name = git_user_name();
    print!(
        "{}",
        render_home_at_width(
            cfg,
            coderoom_dir,
            project_root,
            first_run,
            splash_width(),
            user_name.as_deref(),
        )
    );
}

pub(super) fn render_home_at_width(
    cfg: &Config,
    coderoom_dir: &Path,
    project_root: &Path,
    first_run: bool,
    width: usize,
    user_name: Option<&str>,
) -> String {
    let mut role_names: Vec<&str> = cfg.role_names().collect();
    role_names.sort_unstable();
    let total_tokens: u64 = role_names
        .iter()
        .map(|n| priors::estimate_role_tokens(coderoom_dir, n))
        .sum();
    let role_pad = role_names
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let role_floor = splash_role_floor(cfg, &role_names, role_pad);
    let (left_w, right_w) = splash_columns(width, role_floor);

    // ── title (top border)
    let version_str = format!("v{}", env!("CARGO_PKG_VERSION"));
    let title = join_cells(&[
        styled_cell("codeRoom", "codeRoom".with(output::SPLASH_FRAME).bold()),
        plain_cell(" "),
        styled_cell(
            &version_str,
            version_str.as_str().with(output::SPLASH_VERSION),
        ),
    ]);

    // ── left column
    let greeting_verb = if first_run { "welcome" } else { "welcome back" };
    let greeting_cell = match user_name {
        Some(name) => {
            let head = format!("{greeting_verb}, ");
            let plain = format!("{head}{name}");
            let cell = join_cells(&[
                styled_cell(&head, head.as_str().with(output::EM).bold()),
                styled_cell(name, name.with(output::EM)),
            ]);
            fit_cell(cell, &plain, left_w)
        }
        None => fit_cell(
            styled_cell(greeting_verb, greeting_verb.with(output::EM).bold()),
            greeting_verb,
            left_w,
        ),
    };

    let path_display = home_relative_display(project_root);
    let path_cell = fit_cell(
        styled_cell(&path_display, path_display.as_str().with(output::DIM)),
        &path_display,
        left_w,
    );

    let token_cell = splash_token_pill_cell(total_tokens, left_w);

    let mut left: Vec<UiCell> = Vec::new();
    left.push(greeting_cell);
    left.push(empty_cell());
    left.extend(splash_role_rows(
        cfg,
        coderoom_dir,
        &role_names,
        role_pad,
        left_w,
    ));
    left.push(empty_cell());
    left.push(token_cell);
    left.push(path_cell);

    // ── right column
    let content = load_splash_content();
    let release = pick_release(&content, env!("CARGO_PKG_VERSION"));

    let mut right: Vec<UiCell> = Vec::new();
    let tips_heading_str = "tips for getting started";
    right.push(fit_cell(
        styled_cell(
            tips_heading_str,
            tips_heading_str.with(output::SPLASH_ACCENT).bold(),
        ),
        tips_heading_str,
        right_w,
    ));
    for tip in &content.tips.items {
        let line_plain = format!("• {tip}");
        right.push(fit_cell(
            join_cells(&[
                styled_cell("•", "•".with(output::SPLASH_ACCENT)),
                styled_cell(&format!(" {tip}"), format!(" {tip}").with(output::TEXT)),
            ]),
            &line_plain,
            right_w,
        ));
    }
    right.push(empty_cell());

    let release_version = release.map_or_else(
        || env!("CARGO_PKG_VERSION").to_owned(),
        |r| r.version.clone(),
    );
    let whats_new_heading = format!("what's new in {release_version}");
    right.push(fit_cell(
        styled_cell(
            &whats_new_heading,
            whats_new_heading
                .as_str()
                .with(output::SPLASH_ACCENT)
                .bold(),
        ),
        &whats_new_heading,
        right_w,
    ));
    if let Some(rel) = release {
        for item in &rel.items {
            let line_plain = format!("• {item}");
            right.push(fit_cell(
                join_cells(&[
                    styled_cell("•", "•".with(output::SPLASH_ACCENT)),
                    styled_cell(&format!(" {item}"), format!(" {item}").with(output::TEXT)),
                ]),
                &line_plain,
                right_w,
            ));
        }
    }
    right.push(empty_cell());
    let footer_str = "/help for commands";
    right.push(fit_cell(
        styled_cell(footer_str, footer_str.with(output::DIM).italic()),
        footer_str,
        right_w,
    ));

    // ── render
    let rows = left.len().max(right.len());
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", splash_top(width, &title));
    // One blank line of breathing room inside the frame top.
    let _ = writeln!(
        out,
        "{}",
        splash_pair(&empty_cell(), &empty_cell(), left_w, right_w)
    );
    for idx in 0..rows {
        let lc = left.get(idx).cloned().unwrap_or_else(empty_cell);
        let rc = right.get(idx).cloned().unwrap_or_else(empty_cell);
        let _ = writeln!(out, "{}", splash_pair(&lc, &rc, left_w, right_w));
    }
    let _ = writeln!(
        out,
        "{}",
        splash_pair(&empty_cell(), &empty_cell(), left_w, right_w)
    );
    let _ = writeln!(out, "{}", splash_bottom(width));
    let _ = writeln!(
        out,
        "  {}",
        "type a task · @role · /help · /exit"
            .with(output::DIM)
            .italic()
    );
    out
}

pub(super) fn print_help(cfg: &Config) {
    println!("commands:");
    println!("  @<role> <text>      send to a specific role");
    println!("  @all <text>         broadcast to every running role");
    println!("  <text>              send to host (@{})", cfg.host_role);
    println!("  /host <role>        make role the host for this session");
    println!("  /patch <role> <…>   save a correction; loads on next /refresh");
    println!("  /refresh <role>     re-instantiate role with latest priors+patches");
    println!("  /resume [id]        list or switch saved CodeRoom sessions");
    println!("  /transcript <role>  show that role's recent spoken turns");
    println!("  /journal <role>     ask role to write today's journal entry");
    println!("  /welcome            re-show the first-run welcome card");
    println!("  /allow <tool>       allow a tool for this session");
    println!("  /deny <tool>        deny a tool for this session");
    println!("  /stop <role>        terminate a role's subprocess");
    println!("  /halt [<role>]      interrupt the in-flight turn; role stays alive");
    println!("  Ctrl-C              like /halt; second press within 2s exits REPL");
    println!("  /help               this help");
    println!("  /exit, /quit        leave the REPL");
    println!();
    println!("keys:");
    println!("  Tab                 cycle @role completions");
    println!("  Right, Ctrl-F       accept visible completion");
    println!("  Enter               accept visible completion and send");
    println!("  Esc                 dismiss visible completion");
    println!();
    println!(
        "{}",
        "tool traces are folded live; run `cr show` for the full event log".with(output::DIM)
    );
}
