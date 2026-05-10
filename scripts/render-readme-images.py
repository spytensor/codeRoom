#!/usr/bin/env python3
"""Render the PNG screenshots embedded in README.md.

The images are intentionally synthetic terminal compositions. They keep the
README stable and reproducible without requiring a live TUI, VHS, freeze, or a
particular desktop screenshot setup.
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
CANVAS = (1800, 900)

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


FONT_REGULAR = [
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/truetype/ubuntu/UbuntuMono-R.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
]
FONT_BOLD = [
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Bold.ttf",
    "/usr/share/fonts/truetype/ubuntu/UbuntuMono-B.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Bold.ttf",
]
FONT_ITALIC = [
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Italic.ttf",
    "/usr/share/fonts/truetype/ubuntu/UbuntuMono-RI.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Italic.ttf",
]


def load_font(candidates: Iterable[str], size: int) -> ImageFont.ImageFont:
    for candidate in candidates:
        path = Path(candidate)
        if path.exists():
            return ImageFont.truetype(str(path), size=size)
    return ImageFont.load_default()


FONT = load_font(FONT_REGULAR, 26)
BOLD = load_font(FONT_BOLD, 26)
TITLE = load_font(FONT_BOLD, 29)
BODY = load_font(FONT_REGULAR, 24)
SMALL = load_font(FONT_REGULAR, 22)
SMALL_BOLD = load_font(FONT_BOLD, 22)
ITALIC = load_font(FONT_ITALIC, 24)


def text_width(draw: ImageDraw.ImageDraw, text: str, font: ImageFont.ImageFont) -> int:
    left, _, right, _ = draw.textbbox((0, 0), text, font=font)
    return right - left


def fit_text(
    draw: ImageDraw.ImageDraw, text: str, max_width: int, font: ImageFont.ImageFont
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
    draw: ImageDraw.ImageDraw,
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


def new_canvas() -> tuple[Image.Image, ImageDraw.ImageDraw]:
    image = Image.new("RGB", CANVAS, BG)
    draw = ImageDraw.Draw(image)
    return image, draw


def bullet(draw: ImageDraw.ImageDraw, x: int, y: int, color: tuple[int, int, int]) -> None:
    draw.ellipse((x, y, x + 14, y + 14), fill=color)


def prompt(draw: ImageDraw.ImageDraw, x: int, y: int, body: str) -> None:
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


def status_line(draw: ImageDraw.ImageDraw, y: int, role: str) -> None:
    draw_text(
        draw,
        (86, y),
        f"│ 1 role working · chat stream paused until it reports · {role} -",
        MUTED,
        FONT,
    )


def work_card(
    draw: ImageDraw.ImageDraw,
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
    draw: ImageDraw.ImageDraw,
    y: int,
    role: str,
    text: str,
    color: tuple[int, int, int],
) -> None:
    draw.line((80, y - 3, 80, y + 29), fill=color, width=4)
    draw_text(draw, (111, y), role, color, BOLD)
    draw_text(draw, (111 + text_width(draw, role, BOLD) + 18, y), text, WHITE, FONT)


def right_rail(draw: ImageDraw.ImageDraw) -> None:
    x = 1244
    draw.rectangle((1204, 128, 1726, 764), fill=RAIL_BG)
    draw_text(draw, (x, 149), "engine trace modes", YELLOW, TITLE)
    draw_text(draw, (x, 197), "@claude", BLUE, BOLD)
    draw_text(draw, (x + 26, 230), "live tools", WHITE, FONT)
    draw_text(draw, (x + 26, 264), "cr-task + assistant text", MUTED, SMALL)
    draw_text(draw, (x, 309), "@codex", SECURITY, BOLD)
    draw_text(draw, (x + 26, 342), "partial live", WHITE, FONT)
    draw_text(draw, (x + 26, 376), "MCP notifications when emitted", MUTED, SMALL)
    draw_text(draw, (x, 421), "@gemini", PURPLE, BOLD)
    draw_text(draw, (x + 26, 454), "buffered", WHITE, FONT)
    draw_text(draw, (x + 26, 488), "stream parsed after turn", MUTED, SMALL)

    draw_text(draw, (x, 566), "visual rule", YELLOW, TITLE)
    draw_text(draw, (x, 613), "work is framed", WHITE, FONT)
    draw_text(draw, (x, 648), "chat stays unframed", MUTED, FONT)
    draw_text(draw, (x, 699), "handoffs stay serial", WHITE, FONT)
    draw_text(draw, (x, 734), "until concurrent roles land", MUTED, FONT)


def render_work_cards() -> None:
    image, draw = new_canvas()
    prompt(draw, 82, 84, "@security review work-card protocol and timeout behavior")
    status_line(draw, 134, "@security")
    work_card(draw, 208, "@security", "Audit adapter timeout semantics", "24s", 4, (10, 118, 108))
    chat_line(draw, 291, "@security", "REPL owns timeouts; stale engine replies are suppressed.", SECURITY)
    draw_text(draw, (112, 344), "→ auto-routing to @backend", MUTED, SMALL)

    status_line(draw, 392, "@backend")
    work_card(draw, 466, "@backend", "Wire WorkTitle across engines", "31s", 3, (30, 110, 178))
    chat_line(draw, 550, "@backend", "Work titles are extracted before role chat is rendered.", BLUE)

    prompt(draw, 82, 633, "@ci run focused regression tests")
    status_line(draw, 683, "@ci")
    work_card(draw, 752, "@ci", "Run focused regression tests", "18s", 2, (30, 142, 107))
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
