#!/usr/bin/env python3
"""Render the PNG screenshots embedded in README.md.

The images are intentionally synthetic terminal compositions. They keep the
README stable and reproducible without requiring a live TUI, VHS, freeze, or a
particular desktop screenshot setup.

The renderer keeps the layout code at the original 1× coordinate space and
upscales every draw call by ``SCALE`` at the boundary — so the PNGs land at
retina resolution (3600 × 1800 by default) and text stays crisp on modern
displays without making every literal coordinate harder to read.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path
from typing import Iterable

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError as exc:
    raise SystemExit(
        "Pillow is required. Install it with: python3 -m pip install --user pillow"
    ) from exc

try:
    import tomllib
except ImportError as exc:
    raise SystemExit("Python 3.11+ is required for tomllib.") from exc


ROOT = Path(__file__).resolve().parents[1]
OUT_DIR = ROOT / "docs" / "images"

# Layout is authored at this base size; SCALE multiplies the actual pixel
# output so the PNGs are crisp on retina displays. Layout helpers below
# work in 1× coordinates and the `ScaledDraw` wrapper handles the
# translation when calling Pillow.
CANVAS_LOGICAL = (1800, 900)
SCALE = 2
CANVAS = (CANVAS_LOGICAL[0] * SCALE, CANVAS_LOGICAL[1] * SCALE)

BG = (0, 7, 7)
PANEL = (0, 12, 12)
CYAN = (64, 238, 224)
YELLOW = (255, 210, 38)
WHITE = (226, 226, 226)
MUTED = (145, 145, 145)
DIM = (95, 95, 95)
BLUE = (78, 166, 255)
GREEN = (54, 230, 178)
PURPLE = (164, 117, 255)
SECURITY = (45, 224, 215)
BACKEND = (55, 141, 220)
CI = (38, 190, 142)
RAIL_BG = (0, 10, 10)


# Font candidate lists, checked in order. Linux paths first (CI + most
# dev boxes), macOS system paths as fallback so contributors can
# regenerate locally without installing a font package. If none match,
# Pillow drops to a default bitmap font that ignores the size argument;
# `load_font` prints a loud warning when that happens so a broken
# regeneration is loud rather than silent.
FONT_REGULAR = [
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/truetype/ubuntu/UbuntuMono-R.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/SFNSMono.ttf",
]
FONT_BOLD = [
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Bold.ttf",
    "/usr/share/fonts/truetype/ubuntu/UbuntuMono-B.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Bold.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/SFNSMono.ttf",
]
FONT_ITALIC = [
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Italic.ttf",
    "/usr/share/fonts/truetype/ubuntu/UbuntuMono-RI.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Italic.ttf",
    "/System/Library/Fonts/SFNSMonoItalic.ttf",
    "/System/Library/Fonts/Menlo.ttc",
]


def load_font(candidates: Iterable[str], size: int) -> ImageFont.ImageFont:
    """Load a truetype font at the *scaled* pixel size so retina output
    is sharp. Layout code calls this with logical sizes; the SCALE
    multiplication happens here."""
    pixel_size = size * SCALE
    for candidate in candidates:
        path = Path(candidate)
        if path.exists():
            return ImageFont.truetype(str(path), size=pixel_size)
    print(
        "WARN: no matching truetype font found — output will use Pillow's "
        "default bitmap font and ignore size hints",
        file=sys.stderr,
    )
    return ImageFont.load_default()


FONT = load_font(FONT_REGULAR, 26)
BOLD = load_font(FONT_BOLD, 26)
TITLE = load_font(FONT_BOLD, 29)
BODY = load_font(FONT_REGULAR, 24)
SMALL = load_font(FONT_REGULAR, 22)
SMALL_BOLD = load_font(FONT_BOLD, 22)
ITALIC = load_font(FONT_ITALIC, 24)


def _scale(value):
    """Recursively multiply every numeric leaf in a Pillow coordinate
    argument by SCALE. Handles scalars, tuples, and nested tuples."""
    if isinstance(value, tuple):
        return tuple(_scale(v) for v in value)
    if isinstance(value, list):
        return [_scale(v) for v in value]
    if isinstance(value, (int, float)):
        return value * SCALE
    return value


class ScaledDraw:
    """Thin shim over `ImageDraw.ImageDraw` that scales every coordinate
    argument by SCALE so the layout code can stay in logical pixels.
    ``textbbox`` returns logical-space coordinates (the Pillow result
    is divided back) so text-width math composes cleanly with the rest
    of the layout."""

    def __init__(self, draw: ImageDraw.ImageDraw) -> None:
        self._draw = draw

    def text(self, xy, *args, **kwargs):
        return self._draw.text(_scale(xy), *args, **kwargs)

    def line(self, xy, *args, **kwargs):
        return self._draw.line(_scale(xy), *args, **kwargs)

    def rectangle(self, xy, *args, **kwargs):
        return self._draw.rectangle(_scale(xy), *args, **kwargs)

    def ellipse(self, xy, *args, **kwargs):
        return self._draw.ellipse(_scale(xy), *args, **kwargs)

    def textbbox(self, xy, *args, **kwargs):
        bbox = self._draw.textbbox(_scale(xy), *args, **kwargs)
        return tuple(c // SCALE for c in bbox)


def text_width(draw: ScaledDraw, text: str, font: ImageFont.ImageFont) -> int:
    left, _, right, _ = draw.textbbox((0, 0), text, font=font)
    return right - left


def fit_text(
    draw: ScaledDraw, text: str, max_width: int, font: ImageFont.ImageFont
) -> str:
    if text_width(draw, text, font) <= max_width:
        return text
    suffix = "..."
    available = max_width - text_width(draw, suffix, font)
    clipped = ""
    for char in text:
        if text_width(draw, clipped + char, font) > available:
            break
        clipped += char
    return clipped.rstrip() + suffix


def draw_text(
    draw: ScaledDraw,
    xy: tuple[int, int],
    text: str,
    color: tuple[int, int, int] = WHITE,
    font: ImageFont.ImageFont = FONT,
) -> None:
    draw.text(xy, text, fill=color, font=font)


def package_version() -> str:
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', manifest)
    if not match:
        raise SystemExit("Could not find package version in Cargo.toml")
    return match.group(1)


def splash_copy(version: str) -> tuple[list[str], str, list[str]]:
    with (ROOT / "data" / "splash_content.toml").open("rb") as handle:
        data = tomllib.load(handle)
    tips = list(data.get("tips", {}).get("items", []))
    entries = list(data.get("whats_new", []))
    if not entries:
        return tips, version, []
    chosen = next((entry for entry in entries if entry.get("version") == version), entries[0])
    return tips, str(chosen.get("version", version)), list(chosen.get("items", []))


def new_canvas() -> tuple[Image.Image, ScaledDraw]:
    image = Image.new("RGB", CANVAS, BG)
    draw = ScaledDraw(ImageDraw.Draw(image))
    return image, draw


def bullet(draw: ScaledDraw, x: int, y: int, color: tuple[int, int, int]) -> None:
    draw.ellipse((x, y, x + 14, y + 14), fill=color)


def prompt(draw: ScaledDraw, x: int, y: int, body: str) -> None:
    draw_text(draw, (x, y), "⚡", YELLOW, BOLD)
    draw_text(draw, (x + 36, y), "cr", CYAN, BOLD)
    draw_text(draw, (x + 89, y), "›", GREEN, BOLD)
    draw_text(draw, (x + 118, y), body, WHITE, FONT)


def render_boot_dashboard() -> None:
    version = package_version()
    tips, whats_new_version, whats_new = splash_copy(version)
    image, draw = new_canvas()

    left, top, right, bottom = 90, 92, 1440, 653
    draw.rectangle((left, top, right, bottom), fill=PANEL, outline=CYAN, width=2)
    title = f" codeRoom v{version} "
    draw.rectangle((left + 30, top - 21, left + 30 + text_width(draw, title, TITLE), top + 9), fill=BG)
    draw_text(draw, (left + 36, top - 19), title, CYAN, TITLE)

    draw_text(draw, (136, 178), "welcome back, Ada", WHITE, FONT)
    roles = [
        ("@host", "cc", "1M", PURPLE),
        ("@backend", "cc", "1M", BLUE),
        ("@security", "codex", "default", SECURITY),
        ("@ci", "cc", "default", CI),
    ]
    y = 243
    for role, engine, model, color in roles:
        bullet(draw, 137, y + 6, color)
        draw_text(draw, (163, y), role, color, FONT)
        draw_text(draw, (338, y), engine, MUTED, FONT)
        draw_text(draw, (447, y), "·", DIM, FONT)
        draw_text(draw, (479, y), model, MUTED, FONT)
        y += 43

    draw.rectangle((136, 427, 209, 464), fill=CYAN)
    draw_text(draw, (148, 430), "1.8k", BG, BOLD)
    draw_text(draw, (237, 431), "base tokens loaded", WHITE, FONT)
    draw_text(draw, (136, 489), "~/codes/codeRoom", MUTED, FONT)

    right_x = 630
    draw_text(draw, (right_x, 176), "tips for getting started", YELLOW, TITLE)
    y = 216
    for item in tips[:3]:
        draw_text(draw, (right_x, y), "•", WHITE, BODY)
        draw_text(draw, (right_x + 30, y), fit_text(draw, item, 780, BODY), WHITE, BODY)
        y += 39

    y += 18
    draw_text(draw, (right_x, y), f"what's new in {whats_new_version}", YELLOW, TITLE)
    y += 41
    for item in whats_new[:3]:
        draw_text(draw, (right_x, y), "•", WHITE, BODY)
        draw_text(draw, (right_x + 30, y), fit_text(draw, item, 780, BODY), WHITE, BODY)
        y += 39

    draw_text(draw, (right_x, 539), "/help for commands", MUTED, FONT)
    draw_text(draw, (132, 714), "type a task · @role · /help · /exit", MUTED, SMALL)
    prompt(draw, 88, 754, "@host verify the v0.4.4 hotfix")

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    image.save(OUT_DIR / "boot-dashboard.png")


def status_line(draw: ScaledDraw, y: int, role: str) -> None:
    _ = role
    line = "  3 roles working · ... @host · recording gate evidence    ... @security · waiting approval"
    draw_text(draw, (96, y), fit_text(draw, line, 1040, FONT), MUTED, FONT)


def active_card(
    draw: ScaledDraw,
    y: int,
    role: str,
    title: str,
    state: str,
    rows: list[tuple[str, tuple[int, int, int], str]],
    color: tuple[int, int, int],
) -> None:
    left, right = 126, 1138
    height = 86 + 37 * len(rows)
    draw.line((left, y, right, y), fill=color, width=2)
    draw.line((left, y, left, y + height), fill=color, width=2)
    draw.line((right, y, right, y + height), fill=color, width=2)
    draw.line((left, y + height, right, y + height), fill=color, width=2)

    label = f" {role} working · {title} "
    draw.rectangle(
        (left + 24, y - 15, left + 24 + text_width(draw, label, FONT), y + 16),
        fill=BG,
    )
    draw_text(draw, (left + 27, y - 17), label, color, FONT)
    draw_text(draw, (left + 42, y + 34), state, WHITE, FONT)

    row_y = y + 75
    for glyph, glyph_color, text in rows:
        draw_text(draw, (left + 42, row_y), glyph, glyph_color, BOLD)
        draw_text(draw, (left + 77, row_y), fit_text(draw, text, right - left - 120, FONT), MUTED, FONT)
        row_y += 37


def permission_card(
    draw: ScaledDraw,
    y: int,
    role: str,
    title: str,
    color: tuple[int, int, int],
) -> None:
    active_card(
        draw,
        y,
        role,
        title,
        "waiting for your approval",
        [
            ("✓", GREEN, "read src/adapter/cc.rs"),
            ("?", YELLOW, "wants Bash `cargo test --all-features --locked` — [a]llow · [s]ession · [d]eny"),
        ],
        color,
    )


def done_summary(
    draw: ScaledDraw,
    y: int,
    role: str,
    title: str,
    elapsed: str,
    steps: int,
    color: tuple[int, int, int],
) -> None:
    _ = (title, color)
    draw_text(draw, (126, y), fit_text(draw, f"{role} done · {elapsed} · {steps} steps", 1040, FONT), DIM, FONT)


def chat_line(
    draw: ScaledDraw,
    y: int,
    role: str,
    text: str,
    color: tuple[int, int, int],
) -> None:
    draw_text(draw, (126, y), role, color, BOLD)
    draw_text(draw, (164, y + 38), fit_text(draw, text, 980, FONT), WHITE, FONT)


def reply_quote(
    draw: ScaledDraw,
    y: int,
    child_role: str,
    parent_role: str,
    snippet: str,
    child_color: tuple[int, int, int],
    parent_color: tuple[int, int, int],
) -> None:
    """One-line reply pointer — mirrors `format_reply_quote`."""
    draw_text(draw, (126, y), child_role, child_color, BOLD)
    arrow_x = 126 + text_width(draw, child_role, BOLD) + 14
    draw_text(draw, (arrow_x, y), "↲", DIM, FONT)
    parent_x = arrow_x + text_width(draw, "↲", FONT) + 14
    draw_text(draw, (parent_x, y), parent_role, parent_color, FONT)
    sep_x = parent_x + text_width(draw, parent_role, FONT) + 14
    draw_text(draw, (sep_x, y), "·", DIM, FONT)
    snippet_text = f'"{snippet}"'
    draw_text(draw, (sep_x + 28, y), fit_text(draw, snippet_text, 870, FONT), DIM, FONT)


def handoff_banner(
    draw: ScaledDraw,
    y: int,
    role: str,
    color: tuple[int, int, int],
) -> None:
    """Full-width handoff divider — mirrors `handoff_banner` in
    src/repl/render.rs. Painted when a TurnDispatched fires with
    queue_position == 0 (the new speaker actually starts)."""
    draw_text(draw, (126, y), role, color, BOLD)
    dash_start = 126 + text_width(draw, role, BOLD) + 14
    dash_end = 1144
    mid_y = y + 15
    draw.line(
        (dash_start, mid_y, dash_end - text_width(draw, " starting", FONT) - 12, mid_y),
        fill=DIM,
        width=1,
    )
    status_x = dash_end - text_width(draw, "starting", FONT)
    draw_text(draw, (status_x, y), "starting", MUTED, FONT)


def right_rail(draw: ScaledDraw) -> None:
    x = 1244
    draw.rectangle((1204, 128, 1726, 764), fill=RAIL_BG)
    draw_text(draw, (x, 149), "gate surface", YELLOW, TITLE)
    draw_text(draw, (x, 197), "cr gate init", WHITE, FONT)
    draw_text(draw, (x + 26, 230), "one ledger per work thread", MUTED, SMALL)
    draw_text(draw, (x + 26, 264), "host records tier + feature", MUTED, SMALL)
    draw_text(draw, (x, 319), "evidence", WHITE, FONT)
    draw_text(draw, (x + 26, 352), "research, plan, review", MUTED, SMALL)
    draw_text(draw, (x + 26, 386), "sign-off and verification", MUTED, SMALL)
    draw_text(draw, (x, 441), "close", WHITE, FONT)
    draw_text(draw, (x + 26, 474), "blocks on missing proof", MUTED, SMALL)
    draw_text(draw, (x + 26, 508), "bypass requires reason", MUTED, SMALL)
    draw_text(draw, (x + 26, 542), "templates live in .coderoom", MUTED, SMALL)

    draw_text(draw, (x, 586), "context tools", YELLOW, TITLE)
    draw_text(draw, (x, 633), "/compact", WHITE, FONT)
    draw_text(draw, (x + 26, 668), "shrink live role context", MUTED, FONT)
    draw_text(draw, (x, 719), "cr compact", WHITE, FONT)
    draw_text(draw, (x + 26, 754), "fold old history into priors", MUTED, FONT)


def render_work_cards() -> None:
    image, draw = new_canvas()
    prompt(draw, 82, 84, "@host drive release readiness for v0.4.4")
    status_line(draw, 134, "@host")
    active_card(
        draw,
        208,
        "@host",
        "drive Tier 1 release gate",
        "recording gate evidence",
        [
            ("✓", GREEN, "cr gate init --tier 1 --feature v0.4.4"),
            ("✓", GREEN, "recorded research, plan, and sign-off artifacts"),
            ("…", WHITE, "waiting for independent review + verification"),
        ],
        PURPLE,
    )

    permission_card(
        draw,
        430,
        "@security",
        "cross-model review",
        SECURITY,
    )

    done_summary(draw, 636, "@ci", "verify release workflow", "1m12s", 4, CI)
    reply_quote(
        draw,
        684,
        "@ci",
        "@host",
        "fmt, clippy, tests, release build, and remote CI are green",
        CI,
        PURPLE,
    )
    chat_line(
        draw,
        732,
        "@ci",
        "GitHub Release assets and npm latest are live for v0.4.4.",
        CI,
    )

    done_summary(draw, 830, "@host", "drive Tier 1 release gate", "2m41s", 9, PURPLE)
    right_rail(draw)

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    image.save(OUT_DIR / "work-cards.png")


def main() -> int:
    render_boot_dashboard()
    render_work_cards()
    print("rendered docs/images/boot-dashboard.png")
    print("rendered docs/images/work-cards.png")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
