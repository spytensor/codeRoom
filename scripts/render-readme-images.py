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

    draw_text(draw, (136, 178), "welcome back, chaojiezhu", WHITE, FONT)
    roles = [
        ("@backend", "cc", "1M", BLUE),
        ("@ci", "cc", "1M", GREEN),
        ("@ghost", "cc", "1M", PURPLE),
        ("@security", "codex", "default", SECURITY),
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
    draw_text(draw, (148, 430), "1.0k", BG, BOLD)
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
    prompt(draw, 88, 754, "@security scan repo permission boundaries")

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    image.save(OUT_DIR / "boot-dashboard.png")


def status_line(draw: ScaledDraw, y: int, role: str) -> None:
    draw_text(
        draw,
        (86, y),
        f"│ 1 role working · {role} · 33s · 4 tools · running Read Cargo.toml",
        MUTED,
        FONT,
    )


def work_card(
    draw: ScaledDraw,
    y: int,
    role: str,
    title: str,
    duration: str,
    steps: int,
    color: tuple[int, int, int],
) -> None:
    left, right = 90, 1144
    draw.line((left, y, right, y), fill=color, width=2)
    draw.line((left, y, left, y + 31), fill=color, width=2)
    draw.line((right, y, right, y + 31), fill=color, width=2)
    draw.line((left, y + 31, right, y + 31), fill=color, width=2)
    label = f" {role} · {title} · done in {duration} · {steps} steps "
    draw.rectangle((left + 24, y - 15, left + 24 + text_width(draw, label, FONT), y + 16), fill=BG)
    draw_text(draw, (left + 27, y - 17), label, color, FONT)


def chat_line(
    draw: ScaledDraw,
    y: int,
    role: str,
    text: str,
    color: tuple[int, int, int],
) -> None:
    draw.line((80, y - 3, 80, y + 29), fill=color, width=4)
    draw_text(draw, (111, y), role, color, BOLD)
    text_x = 111 + text_width(draw, role, BOLD) + 18
    draw_text(draw, (text_x, y), fit_text(draw, text, 1130 - text_x, FONT), WHITE, FONT)


def reply_quote(
    draw: ScaledDraw,
    y: int,
    child_role: str,
    parent_role: str,
    snippet: str,
    child_color: tuple[int, int, int],
    parent_color: tuple[int, int, int],
) -> None:
    """Two-line Slack-style reply pointer printed before an auto-routed
    turn — mirrors `format_reply_quote` in src/repl/render.rs. The
    gutter belongs to the child (it sits directly above the child's
    output); the parent role label keeps its own role color so the eye
    links the quote back to that role's earlier reply."""
    draw.line((80, y - 3, 80, y + 29), fill=child_color, width=4)
    draw_text(draw, (111, y), child_role, child_color, BOLD)
    arrow_x = 111 + text_width(draw, child_role, BOLD) + 14
    draw_text(draw, (arrow_x, y), "→", DIM, FONT)
    reply_x = arrow_x + text_width(draw, "→", FONT) + 14
    draw_text(draw, (reply_x, y), "replying to", MUTED, FONT)
    parent_x = reply_x + text_width(draw, "replying to", FONT) + 12
    draw_text(draw, (parent_x, y), parent_role, parent_color, FONT)

    quote_y = y + 32
    draw.line((80, quote_y - 3, 80, quote_y + 29), fill=child_color, width=4)
    draw_text(draw, (111, quote_y), "│", DIM, FONT)
    snippet_text = f'"{snippet}"'
    draw_text(draw, (135, quote_y), fit_text(draw, snippet_text, 1110, FONT), DIM, FONT)


def handoff_banner(
    draw: ScaledDraw,
    y: int,
    role: str,
    color: tuple[int, int, int],
) -> None:
    """Full-width handoff divider — mirrors `handoff_banner` in
    src/repl/render.rs. Painted when a TurnDispatched fires with
    queue_position == 0 (the new speaker actually starts)."""
    draw.line((80, y - 3, 80, y + 29), fill=color, width=4)
    draw_text(draw, (111, y), role, color, BOLD)
    dash_start = 111 + text_width(draw, role, BOLD) + 14
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
    draw_text(draw, (x, 149), "engine trace modes", YELLOW, TITLE)
    draw_text(draw, (x, 197), "@claude", BLUE, BOLD)
    draw_text(draw, (x + 26, 230), "live tools", WHITE, FONT)
    draw_text(draw, (x + 26, 264), "cr-task + assistant text", MUTED, SMALL)
    draw_text(draw, (x, 309), "@codex", SECURITY, BOLD)
    draw_text(draw, (x + 26, 342), "streaming text", WHITE, FONT)
    draw_text(draw, (x + 26, 376), "MCP deltas + tool progress", MUTED, SMALL)
    draw_text(draw, (x, 421), "@gemini", PURPLE, BOLD)
    draw_text(draw, (x + 26, 454), "streaming json", WHITE, FONT)
    draw_text(draw, (x + 26, 488), "assistant text + tool events", MUTED, SMALL)

    draw_text(draw, (x, 566), "visual rule", YELLOW, TITLE)
    draw_text(draw, (x, 613), "work is framed", WHITE, FONT)
    draw_text(draw, (x, 648), "chat is markdown-lite", MUTED, FONT)
    draw_text(draw, (x, 699), "deltas are live-only", WHITE, FONT)
    draw_text(draw, (x, 734), "history keeps final replies", MUTED, FONT)


def render_work_cards() -> None:
    image, draw = new_canvas()
    prompt(draw, 82, 84, "@security scan project security and explain findings")
    status_line(draw, 134, "@security")
    work_card(draw, 208, "@security", "Audit permission boundaries", "4m49s", 33, (10, 118, 108))
    chat_line(draw, 291, "@security", "Findings: bypass defaults, role paths, session scope.", SECURITY)

    # Cross-role auto-route: reply pointer (#99) then handoff banner
    # (#98) before @backend's work surfaces.
    reply_quote(
        draw,
        340,
        "@backend",
        "@security",
        "Findings: bypass defaults, role paths, session scope.",
        BLUE,
        SECURITY,
    )
    handoff_banner(draw, 410, "@backend", BLUE)

    status_line(draw, 454, "@backend")
    work_card(draw, 522, "@backend", "Implement observable role output", "52s", 7, (30, 110, 178))
    chat_line(draw, 606, "@backend", "Done: markdown replies, live deltas, WorkCard previews.", BLUE)

    prompt(draw, 82, 678, "@ci run focused regression tests")
    status_line(draw, 728, "@ci")
    work_card(draw, 796, "@ci", "Run focused regression tests", "21s", 5, (30, 142, 107))
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
