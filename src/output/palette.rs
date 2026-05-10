//! Terminal color palette.
//!
//! Keep raw RGB values here; higher-level modules should consume the
//! semantic names re-exported by `crate::output`.

use crossterm::style::Color;

/// Success — paired with `✓`.
pub const OK: Color = Color::Rgb {
    r: 0x97,
    g: 0xc4,
    b: 0x59,
};
/// Attention but not failure — paired with `⚠`, `⟳`, `⊘`.
pub const WARN: Color = Color::Rgb {
    r: 0xef,
    g: 0x9f,
    b: 0x27,
};
/// Failure — paired with `✗`.
pub const BAD: Color = Color::Rgb {
    r: 0xea,
    g: 0x5b,
    b: 0x5b,
};
/// Neutral hint, auto-routing arrow.
pub const INFO: Color = Color::Rgb {
    r: 0x6f,
    g: 0xa8,
    b: 0xdc,
};
/// Commands and hotkeys (`/help`, `/patch`).
pub const KEY: Color = Color::Rgb {
    r: 0xd4,
    g: 0xb8,
    b: 0x7a,
};
/// Input prompt (`cr ›`).
pub const PROMPT: Color = Color::Rgb {
    r: 0x58,
    g: 0xc3,
    b: 0x9c,
};
/// Emphasis: panel titles, API paths, key values.
pub const EM: Color = Color::Rgb {
    r: 0xf0,
    g: 0xf0,
    b: 0xf0,
};
/// Default body text.
pub const TEXT: Color = Color::Rgb {
    r: 0xd4,
    g: 0xd4,
    b: 0xd4,
};
/// Secondary information: timestamps, side labels.
pub const MUTE: Color = Color::Rgb {
    r: 0x9a,
    g: 0x9a,
    b: 0x9a,
};
/// System rows: tool call summaries, in-place spinner status.
pub const DIM: Color = Color::Rgb {
    r: 0x82,
    g: 0x82,
    b: 0x82,
};
/// Decorative `·` separators and the `↳` glyph. Sub-AA by design.
pub const FADE: Color = Color::Rgb {
    r: 0x4a,
    g: 0x4a,
    b: 0x4a,
};
/// Box drawing borders.
pub const RULE: Color = Color::Rgb {
    r: 0x6a,
    g: 0x6a,
    b: 0x6a,
};

pub(crate) const ROLE_PALETTE: [Color; 8] = [
    // 0: lavender — host is pinned here.
    Color::Rgb {
        r: 0xb8,
        g: 0x9c,
        b: 0xff,
    },
    // 1: jade
    Color::Rgb {
        r: 0x4d,
        g: 0xd4,
        b: 0xa4,
    },
    // 2: coral
    Color::Rgb {
        r: 0xff,
        g: 0x88,
        b: 0x66,
    },
    // 3: rose
    Color::Rgb {
        r: 0xff,
        g: 0x7a,
        b: 0x8a,
    },
    // 4: sky
    Color::Rgb {
        r: 0x6b,
        g: 0xb6,
        b: 0xff,
    },
    // 5: blossom
    Color::Rgb {
        r: 0xff,
        g: 0x90,
        b: 0xc8,
    },
    // 6: honey
    Color::Rgb {
        r: 0xff,
        g: 0xc8,
        b: 0x59,
    },
    // 7: teal
    Color::Rgb {
        r: 0x5c,
        g: 0xd6,
        b: 0xcc,
    },
];
