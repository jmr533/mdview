//! Color palettes (GitHub Primer light/dark) and matching syntect themes.

use gtk::gdk::RGBA;
use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme as SynTheme, ThemeItem, ThemeSettings,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeChoice {
    Auto,
    Light,
    Dark,
}

impl ThemeChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeChoice::Auto => "auto",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "auto" => Some(ThemeChoice::Auto),
            "light" => Some(ThemeChoice::Light),
            "dark" => Some(ThemeChoice::Dark),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            ThemeChoice::Auto => ThemeChoice::Light,
            ThemeChoice::Light => ThemeChoice::Dark,
            ThemeChoice::Dark => ThemeChoice::Auto,
        }
    }
}

pub struct Palette {
    pub dark: bool,
    pub fg: u32,
    pub fg_muted: u32,
    pub canvas: u32,
    pub canvas_subtle: u32,
    /// Inline `code` background (GitHub's translucent neutral, pre-blended on the canvas).
    pub code_inline: u32,
    pub border: u32,
    pub border_muted: u32,
    pub accent: u32,
    pub success: u32,
    pub attention: u32,
    pub danger: u32,
    pub done: u32,
    pub selection: u32,
    pub hit: u32,
    pub hit_current: u32,
    pub syntax: SynTheme,
}

pub fn rgba(hex: u32) -> RGBA {
    RGBA::new(
        ((hex >> 16) & 0xff) as f64 / 255.0,
        ((hex >> 8) & 0xff) as f64 / 255.0,
        (hex & 0xff) as f64 / 255.0,
        1.0,
    )
}

pub fn css(hex: u32) -> String {
    format!("#{hex:06x}")
}

fn color(hex: u32) -> Color {
    Color { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8, a: 0xff }
}

struct Syn {
    fg: u32,
    comment: u32,
    keyword: u32,
    string: u32,
    entity: u32,
    constant: u32,
    variable: u32,
    tag: u32,
    inserted: (u32, u32),
    deleted: (u32, u32),
}

fn syntax_theme(s: Syn) -> SynTheme {
    let item = |sel: &str, fg: u32, bg: Option<u32>, style: Option<FontStyle>| ThemeItem {
        scope: sel.parse::<ScopeSelectors>().expect("valid scope selector"),
        style: StyleModifier { foreground: Some(color(fg)), background: bg.map(color), font_style: style },
    };
    SynTheme {
        name: None,
        author: None,
        // A transparent background marks "no background" so only diff lines get one.
        settings: ThemeSettings {
            foreground: Some(color(s.fg)),
            background: Some(Color { r: 0, g: 0, b: 0, a: 0 }),
            ..Default::default()
        },
        scopes: vec![
            item("comment, punctuation.definition.comment", s.comment, None, None),
            item("keyword, storage", s.keyword, None, None),
            item("string, string punctuation", s.string, None, None),
            item(
                "constant, support.constant, support.type, support.class, variable.other.constant, \
                 entity.other.attribute-name, meta.property-name, string constant.character.escape",
                s.constant,
                None,
                None,
            ),
            item(
                "entity.name, support.function, variable.function, entity.other.inherited-class",
                s.entity,
                None,
                None,
            ),
            item("entity.name.tag", s.tag, None, None),
            item("entity.name.tag.yaml", s.constant, None, None),
            item(
                "variable.parameter, variable.language, storage.modifier.lifetime, variable.other.readwrite.shell",
                s.variable,
                None,
                None,
            ),
            item("markup.heading", s.constant, None, Some(FontStyle::BOLD)),
            item("markup.bold", s.fg, None, Some(FontStyle::BOLD)),
            item("markup.italic", s.fg, None, Some(FontStyle::ITALIC)),
            item("markup.inserted", s.inserted.0, Some(s.inserted.1), None),
            item("markup.deleted", s.deleted.0, Some(s.deleted.1), None),
            item("meta.diff.range, meta.diff.header", s.entity, None, Some(FontStyle::BOLD)),
        ],
    }
}

pub fn light() -> Palette {
    Palette {
        dark: false,
        fg: 0x1f2328,
        fg_muted: 0x59636e,
        canvas: 0xffffff,
        canvas_subtle: 0xf6f8fa,
        code_inline: 0xeff1f3,
        border: 0xd1d9e0,
        border_muted: 0xdfe4e9,
        accent: 0x0969da,
        success: 0x1a7f37,
        attention: 0x9a6700,
        danger: 0xd1242f,
        done: 0x8250df,
        selection: 0xcde4fb,
        hit: 0xfff8c5,
        hit_current: 0xffd33d,
        syntax: syntax_theme(Syn {
            fg: 0x1f2328,
            comment: 0x59636e,
            keyword: 0xcf222e,
            string: 0x0a3069,
            entity: 0x6639ba,
            constant: 0x0550ae,
            variable: 0x953800,
            tag: 0x116329,
            inserted: (0x116329, 0xdafbe1),
            deleted: (0x82071e, 0xffebe9),
        }),
    }
}

pub fn dark() -> Palette {
    Palette {
        dark: true,
        fg: 0xf0f6fc,
        fg_muted: 0x9198a1,
        canvas: 0x0d1117,
        canvas_subtle: 0x151b23,
        code_inline: 0x1f242b,
        border: 0x3d444d,
        border_muted: 0x2a3038,
        accent: 0x4493f8,
        success: 0x3fb950,
        attention: 0xd29922,
        danger: 0xf85149,
        done: 0xab7df8,
        selection: 0x1e3a5f,
        hit: 0x4d3b0c,
        hit_current: 0x9e6a03,
        syntax: syntax_theme(Syn {
            fg: 0xf0f6fc,
            comment: 0x9198a1,
            keyword: 0xff7b72,
            string: 0xa5d6ff,
            entity: 0xd2a8ff,
            constant: 0x79c0ff,
            variable: 0xffa657,
            tag: 0x7ee787,
            inserted: (0xaff5b4, 0x033a16),
            deleted: (0xffdcd7, 0x67060c),
        }),
    }
}
