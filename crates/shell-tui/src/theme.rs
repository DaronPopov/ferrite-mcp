//! Cream color palette — all colors defined as truecolor ANSI sequences.
//!
//! Base aesthetic: warm parchment/cream tones, low visual noise.

use crossterm::style::Color;

// ── Raw RGB values ───────────────────────────────────────────────────────────

/// Background-adjacent warm cream  #F5F0E8
pub const CREAM: Color = Color::Rgb { r: 245, g: 240, b: 232 };

/// Primary text — deep warm brown  #3B3228
pub const INK: Color = Color::Rgb { r: 59, g: 50, b: 40 };

/// Secondary text / dim             #A89880
pub const DUST: Color = Color::Rgb { r: 168, g: 152, b: 128 };

/// Accent / highlights               #8B7355
pub const SAND: Color = Color::Rgb { r: 139, g: 115, b: 85 };

/// Success / ok                      #5C8A3C
pub const SAGE: Color = Color::Rgb { r: 92, g: 138, b: 60 };

/// Error                             #A0522D
pub const SIENNA: Color = Color::Rgb { r: 160, g: 82, b: 45 };

/// Warning                           #B8860B
pub const OCHRE: Color = Color::Rgb { r: 184, g: 134, b: 11 };

/// Command / keyword highlight       #4A6FA5
pub const SLATE: Color = Color::Rgb { r: 74, g: 111, b: 165 };

// ── Semantic aliases ─────────────────────────────────────────────────────────

pub const COLOR_PROMPT_CWD:      Color = SAND;
pub const COLOR_PROMPT_BRANCH:   Color = DUST;
pub const COLOR_PROMPT_OK:       Color = SAGE;
pub const COLOR_PROMPT_FAIL:     Color = SIENNA;
pub const COLOR_PROMPT_ARROW:    Color = INK;
pub const COLOR_COMPLETION_SEL:  Color = SLATE;
pub const COLOR_COMPLETION_ITEM: Color = DUST;
pub const COLOR_ERROR:           Color = SIENNA;
pub const COLOR_WARNING:         Color = OCHRE;
pub const COLOR_HINT:            Color = DUST;
