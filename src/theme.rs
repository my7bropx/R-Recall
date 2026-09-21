//! Color themes.
//!
//! Every color is defined twice: an exact 24-bit RGB value, and an ANSI-16
//! fallback for terminals without truecolor. Which one is used is decided once
//! at startup by [`truecolor_supported`], so a TTY or tmux session without
//! truecolor degrades to the terminal's own palette instead of rendering muddy
//! approximations of RGB values it cannot show.

use ratatui::style::Color;

/// One color: exact RGB plus the ANSI slot to fall back to.
#[derive(Debug, Clone, Copy)]
pub struct Shade {
    pub rgb:  (u8, u8, u8),
    pub ansi: Color,
}

impl Shade {
    const fn new(rgb: (u8, u8, u8), ansi: Color) -> Self {
        Shade { rgb, ansi }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    /// True when 24-bit color should be used.
    pub rgb:  bool,
    /// Paint the app background rather than inheriting the terminal's.
    pub paint_bg: bool,

    // surfaces
    pub bg:     Shade, // app background, and text color on accent-filled chips
    pub sel:    Shade, // selected-row background
    pub border: Shade, // widget borders

    // text
    pub fg:   Shade,
    pub dim:  Shade,

    // roles
    pub accent: Shade,
    pub warn:   Shade,
    pub star:   Shade,

    // entry categories
    pub cmd:  Shade,
    pub note: Shade,
    pub tool: Shade,

    // markdown / code highlighting
    pub code:       Shade,     // `inline code`
    pub code_block: Shade,     // fenced block body
    pub comment:    Shade,     // # comments inside code
    pub bold:       Shade,     // **bold**
    pub italic:     Shade,     // *italic*
    pub link:       Shade,     // [link text]
    pub link_url:   Shade,     // (destination)
    pub heads:      [Shade; 6], // heading levels 1-6
}

impl Theme {
    /// Resolve a shade to a concrete color under the current color mode.
    #[inline]
    pub fn c(&self, s: Shade) -> Color {
        if self.rgb {
            Color::Rgb(s.rgb.0, s.rgb.1, s.rgb.2)
        } else {
            s.ansi
        }
    }

    // Convenience accessors so call sites read as `th.accent_c()`.
    pub fn bg_c(&self)     -> Color { self.c(self.bg) }
    pub fn sel_c(&self)    -> Color { self.c(self.sel) }
    pub fn border_c(&self) -> Color { self.c(self.border) }
    pub fn fg_c(&self)     -> Color { self.c(self.fg) }
    pub fn dim_c(&self)    -> Color { self.c(self.dim) }
    pub fn accent_c(&self) -> Color { self.c(self.accent) }
    pub fn warn_c(&self)   -> Color { self.c(self.warn) }
    pub fn star_c(&self)   -> Color { self.c(self.star) }
    pub fn cmd_c(&self)    -> Color { self.c(self.cmd) }
    pub fn note_c(&self)   -> Color { self.c(self.note) }
    pub fn tool_c(&self)   -> Color { self.c(self.tool) }
    pub fn code_c(&self)       -> Color { self.c(self.code) }
    pub fn code_block_c(&self) -> Color { self.c(self.code_block) }
    pub fn comment_c(&self)    -> Color { self.c(self.comment) }
    pub fn bold_c(&self)       -> Color { self.c(self.bold) }
    pub fn italic_c(&self)     -> Color { self.c(self.italic) }
    pub fn link_c(&self)       -> Color { self.c(self.link) }
    pub fn link_url_c(&self)   -> Color { self.c(self.link_url) }

    /// Color for a heading of the given level (1-based; clamped to 6).
    pub fn head_c(&self, level: usize) -> Color {
        let i = level.clamp(1, 6) - 1;
        self.c(self.heads[i])
    }

    pub fn all() -> &'static [Theme] {
        &[SAFELIGHT, DEFAULT, GRUVBOX, CATPPUCCIN, NORD, TOKYONIGHT]
    }

    pub fn names() -> String {
        Theme::all().iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
    }

    /// Look up by name (case-insensitive). Returns None if unknown.
    pub fn by_name(name: &str) -> Option<Theme> {
        let n = name.trim().to_lowercase();
        Theme::all().iter().find(|t| t.name == n).copied()
    }

    /// Theme selected by `RECALL_THEME`, defaulting to `safelight`. Color mode
    /// is auto-detected; `RECALL_TRUECOLOR=0|1` forces it.
    pub fn from_env() -> Theme {
        let mut t = std::env::var("RECALL_THEME")
            .ok()
            .and_then(|n| Theme::by_name(&n))
            .unwrap_or(SAFELIGHT);
        t.rgb = truecolor_supported();
        if let Ok(v) = std::env::var("RECALL_BG") {
            let v = v.trim().to_lowercase();
            t.paint_bg = !(v == "0" || v == "false" || v == "no");
        }
        t
    }

    pub fn with_color_mode(mut self, rgb: bool) -> Theme {
        self.rgb = rgb;
        self
    }

    pub fn with_background(mut self, paint: bool) -> Theme {
        self.paint_bg = paint;
        self
    }
}

/// `RECALL_TRUECOLOR` wins if set; otherwise sniff `COLORTERM`, which is what
/// terminals advertising 24-bit color set (`truecolor` or `24bit`).
pub fn truecolor_supported() -> bool {
    if let Ok(v) = std::env::var("RECALL_TRUECOLOR") {
        let v = v.trim().to_lowercase();
        return !(v == "0" || v == "false" || v == "no");
    }
    match std::env::var("COLORTERM") {
        Ok(v) => {
            let v = v.to_lowercase();
            v.contains("truecolor") || v.contains("24bit")
        }
        Err(_) => false,
    }
}

// ─── presets ─────────────────────────────────────────────────────────────────

/// Safelight — darkroom. Substrate is a warm near-black, text is silver
/// gelatin, accents are dichroic enlarger filters plus darkroom chemistry.
/// Ported from the VS Code theme of the same name; heading levels, inline vs.
/// fenced code, comments and links all follow its Markdown token rules.
pub const SAFELIGHT: Theme = Theme {
    name: "safelight",
    rgb: true,
    paint_bg: true,

    bg:     Shade::new((0x16, 0x12, 0x0F), Color::Black),     // substrate
    sel:    Shade::new((0x3A, 0x2E, 0x26), Color::DarkGray),  // selection
    border: Shade::new((0x2C, 0x25, 0x21), Color::DarkGray),  // border

    fg:  Shade::new((0xD6, 0xD0, 0xC6), Color::White),        // silver
    dim: Shade::new((0x6B, 0x63, 0x5B), Color::DarkGray),     // fog.deep

    accent: Shade::new((0x62, 0xBE, 0xC4), Color::Cyan),      // filter.cyan
    warn:   Shade::new((0xD2, 0x62, 0x4F), Color::Red),       // stopbath.red
    star:   Shade::new((0xE3, 0x9B, 0x41), Color::Yellow),    // safelight.amber

    cmd:  Shade::new((0x8F, 0xB5, 0x73), Color::Green),       // fixer.green
    note: Shade::new((0xD8, 0xB4, 0x5F), Color::Yellow),      // filter.yellow
    tool: Shade::new((0xB7, 0x9B, 0xD6), Color::Magenta),     // toner.violet

    code:       Shade::new((0x8F, 0xB5, 0x73), Color::Green), // inline code
    code_block: Shade::new((0xB3, 0xAB, 0xA1), Color::Gray),  // fenced body
    comment:    Shade::new((0x8A, 0x80, 0x76), Color::DarkGray), // fog
    bold:       Shade::new((0xD8, 0xB4, 0x5F), Color::Yellow),
    italic:     Shade::new((0xC8, 0xBE, 0xB0), Color::Gray),
    link:       Shade::new((0x62, 0xBE, 0xC4), Color::Cyan),
    link_url:   Shade::new((0x6B, 0x63, 0x5B), Color::DarkGray),
    heads: [
        Shade::new((0xE3, 0x9B, 0x41), Color::Yellow),   // h1 safelight.amber
        Shade::new((0xD8, 0xB4, 0x5F), Color::Yellow),   // h2 filter.yellow
        Shade::new((0x62, 0xBE, 0xC4), Color::Cyan),     // h3 filter.cyan
        Shade::new((0x8F, 0xB5, 0x73), Color::Green),    // h4 fixer.green
        Shade::new((0xD0, 0x6E, 0x9E), Color::Magenta),  // h5 filter.magenta
        Shade::new((0xB7, 0x9B, 0xD6), Color::Magenta),  // h6 toner.violet
    ],
};

/// The original ANSI look, for terminals whose own palette you want to keep.
pub const DEFAULT: Theme = Theme {
    name: "default",
    rgb: false,
    paint_bg: false,

    bg:     Shade::new((16, 16, 16),    Color::Black),
    sel:    Shade::new((40, 40, 60),    Color::DarkGray),
    border: Shade::new((110, 110, 110), Color::DarkGray),

    fg:  Shade::new((220, 220, 220), Color::White),
    dim: Shade::new((110, 110, 110), Color::DarkGray),

    accent: Shade::new((0, 200, 210),   Color::Cyan),
    warn:   Shade::new((225, 80, 80),   Color::Red),
    star:   Shade::new((220, 190, 60),  Color::Yellow),

    cmd:  Shade::new((90, 200, 90),   Color::Green),
    note: Shade::new((220, 190, 60),  Color::Yellow),
    tool: Shade::new((200, 120, 210), Color::Magenta),

    code:       Shade::new((90, 200, 90),    Color::Green),
    code_block: Shade::new((170, 170, 170),  Color::Gray),
    comment:    Shade::new((110, 110, 110),  Color::DarkGray),
    bold:       Shade::new((220, 220, 220),  Color::White),
    italic:     Shade::new((190, 190, 190),  Color::Gray),
    link:       Shade::new((0, 200, 210),    Color::Cyan),
    link_url:   Shade::new((110, 110, 110),  Color::DarkGray),
    heads: [
        Shade::new((220, 190, 60),  Color::Yellow),
        Shade::new((220, 190, 60),  Color::Yellow),
        Shade::new((0, 200, 210),   Color::Cyan),
        Shade::new((90, 200, 90),   Color::Green),
        Shade::new((200, 120, 210), Color::Magenta),
        Shade::new((200, 120, 210), Color::Magenta),
    ],
};

pub const GRUVBOX: Theme = Theme {
    name: "gruvbox",
    rgb: true,
    paint_bg: true,

    bg:     Shade::new((0x28, 0x28, 0x28), Color::Black),
    sel:    Shade::new((0x3c, 0x38, 0x36), Color::DarkGray),
    border: Shade::new((0x50, 0x49, 0x45), Color::DarkGray),

    fg:  Shade::new((0xeb, 0xdb, 0xb2), Color::White),
    dim: Shade::new((0x92, 0x83, 0x74), Color::DarkGray),

    accent: Shade::new((0x83, 0xa5, 0x98), Color::Cyan),
    warn:   Shade::new((0xfb, 0x49, 0x34), Color::Red),
    star:   Shade::new((0xfa, 0xbd, 0x2f), Color::Yellow),

    cmd:  Shade::new((0xb8, 0xbb, 0x26), Color::Green),
    note: Shade::new((0xfa, 0xbd, 0x2f), Color::Yellow),
    tool: Shade::new((0xd3, 0x86, 0x9b), Color::Magenta),

    code:       Shade::new((0xb8, 0xbb, 0x26), Color::Green),
    code_block: Shade::new((0xd5, 0xc4, 0xa1), Color::Gray),
    comment:    Shade::new((0x92, 0x83, 0x74), Color::DarkGray),
    bold:       Shade::new((0xfa, 0xbd, 0x2f), Color::Yellow),
    italic:     Shade::new((0xd5, 0xc4, 0xa1), Color::Gray),
    link:       Shade::new((0x83, 0xa5, 0x98), Color::Cyan),
    link_url:   Shade::new((0x92, 0x83, 0x74), Color::DarkGray),
    heads: [
        Shade::new((0xfe, 0x80, 0x19), Color::Yellow),
        Shade::new((0xfa, 0xbd, 0x2f), Color::Yellow),
        Shade::new((0x83, 0xa5, 0x98), Color::Cyan),
        Shade::new((0xb8, 0xbb, 0x26), Color::Green),
        Shade::new((0xd3, 0x86, 0x9b), Color::Magenta),
        Shade::new((0xd3, 0x86, 0x9b), Color::Magenta),
    ],
};

pub const CATPPUCCIN: Theme = Theme {
    name: "catppuccin",
    rgb: true,
    paint_bg: true,

    bg:     Shade::new((0x1e, 0x1e, 0x2e), Color::Black),
    sel:    Shade::new((0x31, 0x32, 0x44), Color::DarkGray),
    border: Shade::new((0x45, 0x47, 0x5a), Color::DarkGray),

    fg:  Shade::new((0xcd, 0xd6, 0xf4), Color::White),
    dim: Shade::new((0x6c, 0x70, 0x86), Color::DarkGray),

    accent: Shade::new((0x89, 0xdc, 0xeb), Color::Cyan),
    warn:   Shade::new((0xf3, 0x8b, 0xa8), Color::Red),
    star:   Shade::new((0xf9, 0xe2, 0xaf), Color::Yellow),

    cmd:  Shade::new((0xa6, 0xe3, 0xa1), Color::Green),
    note: Shade::new((0xf9, 0xe2, 0xaf), Color::Yellow),
    tool: Shade::new((0xcb, 0xa6, 0xf7), Color::Magenta),

    code:       Shade::new((0xa6, 0xe3, 0xa1), Color::Green),
    code_block: Shade::new((0xba, 0xc2, 0xde), Color::Gray),
    comment:    Shade::new((0x6c, 0x70, 0x86), Color::DarkGray),
    bold:       Shade::new((0xf9, 0xe2, 0xaf), Color::Yellow),
    italic:     Shade::new((0xba, 0xc2, 0xde), Color::Gray),
    link:       Shade::new((0x89, 0xb4, 0xfa), Color::Cyan),
    link_url:   Shade::new((0x6c, 0x70, 0x86), Color::DarkGray),
    heads: [
        Shade::new((0xfa, 0xb3, 0x87), Color::Yellow),
        Shade::new((0xf9, 0xe2, 0xaf), Color::Yellow),
        Shade::new((0x89, 0xdc, 0xeb), Color::Cyan),
        Shade::new((0xa6, 0xe3, 0xa1), Color::Green),
        Shade::new((0xf5, 0xc2, 0xe7), Color::Magenta),
        Shade::new((0xcb, 0xa6, 0xf7), Color::Magenta),
    ],
};

pub const NORD: Theme = Theme {
    name: "nord",
    rgb: true,
    paint_bg: true,

    bg:     Shade::new((0x2e, 0x34, 0x40), Color::Black),
    sel:    Shade::new((0x3b, 0x42, 0x52), Color::DarkGray),
    border: Shade::new((0x4c, 0x56, 0x6a), Color::DarkGray),

    fg:  Shade::new((0xec, 0xef, 0xf4), Color::White),
    dim: Shade::new((0x61, 0x6e, 0x88), Color::DarkGray),

    accent: Shade::new((0x88, 0xc0, 0xd0), Color::Cyan),
    warn:   Shade::new((0xbf, 0x61, 0x6a), Color::Red),
    star:   Shade::new((0xeb, 0xcb, 0x8b), Color::Yellow),

    cmd:  Shade::new((0xa3, 0xbe, 0x8c), Color::Green),
    note: Shade::new((0xeb, 0xcb, 0x8b), Color::Yellow),
    tool: Shade::new((0xb4, 0x8e, 0xad), Color::Magenta),

    code:       Shade::new((0xa3, 0xbe, 0x8c), Color::Green),
    code_block: Shade::new((0xd8, 0xde, 0xe9), Color::Gray),
    comment:    Shade::new((0x61, 0x6e, 0x88), Color::DarkGray),
    bold:       Shade::new((0xeb, 0xcb, 0x8b), Color::Yellow),
    italic:     Shade::new((0xd8, 0xde, 0xe9), Color::Gray),
    link:       Shade::new((0x81, 0xa1, 0xc1), Color::Cyan),
    link_url:   Shade::new((0x61, 0x6e, 0x88), Color::DarkGray),
    heads: [
        Shade::new((0xd0, 0x87, 0x70), Color::Yellow),
        Shade::new((0xeb, 0xcb, 0x8b), Color::Yellow),
        Shade::new((0x88, 0xc0, 0xd0), Color::Cyan),
        Shade::new((0xa3, 0xbe, 0x8c), Color::Green),
        Shade::new((0xb4, 0x8e, 0xad), Color::Magenta),
        Shade::new((0xb4, 0x8e, 0xad), Color::Magenta),
    ],
};

pub const TOKYONIGHT: Theme = Theme {
    name: "tokyonight",
    rgb: true,
    paint_bg: true,

    bg:     Shade::new((0x1a, 0x1b, 0x26), Color::Black),
    sel:    Shade::new((0x29, 0x2e, 0x42), Color::DarkGray),
    border: Shade::new((0x3b, 0x42, 0x61), Color::DarkGray),

    fg:  Shade::new((0xc0, 0xca, 0xf5), Color::White),
    dim: Shade::new((0x56, 0x5f, 0x89), Color::DarkGray),

    accent: Shade::new((0x7d, 0xcf, 0xff), Color::Cyan),
    warn:   Shade::new((0xf7, 0x76, 0x8e), Color::Red),
    star:   Shade::new((0xe0, 0xaf, 0x68), Color::Yellow),

    cmd:  Shade::new((0x9e, 0xce, 0x6a), Color::Green),
    note: Shade::new((0xe0, 0xaf, 0x68), Color::Yellow),
    tool: Shade::new((0xbb, 0x9a, 0xf7), Color::Magenta),

    code:       Shade::new((0x9e, 0xce, 0x6a), Color::Green),
    code_block: Shade::new((0xa9, 0xb1, 0xd6), Color::Gray),
    comment:    Shade::new((0x56, 0x5f, 0x89), Color::DarkGray),
    bold:       Shade::new((0xe0, 0xaf, 0x68), Color::Yellow),
    italic:     Shade::new((0xa9, 0xb1, 0xd6), Color::Gray),
    link:       Shade::new((0x7a, 0xa2, 0xf7), Color::Cyan),
    link_url:   Shade::new((0x56, 0x5f, 0x89), Color::DarkGray),
    heads: [
        Shade::new((0xff, 0x9e, 0x64), Color::Yellow),
        Shade::new((0xe0, 0xaf, 0x68), Color::Yellow),
        Shade::new((0x7d, 0xcf, 0xff), Color::Cyan),
        Shade::new((0x9e, 0xce, 0x6a), Color::Green),
        Shade::new((0xbb, 0x9a, 0xf7), Color::Magenta),
        Shade::new((0xbb, 0x9a, 0xf7), Color::Magenta),
    ],
};
