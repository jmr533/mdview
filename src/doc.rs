//! Markdown → `GtkTextBuffer` renderer.
//!
//! Text goes into the buffer with tags (fonts, colors, margins, spacing). Things a
//! text buffer can't express — code block backgrounds, quote bars, table grids,
//! rules, checkboxes, alert icons — are recorded as [`Deco`]rations that the view
//! paints underneath the text.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use comrak::nodes::{AlertType, AstNode, ListType, NodeValue};
use comrak::{Anchorizer, Arena, Options, parse_document};
use gtk::prelude::*;
use gtk::glib::translate::IntoGlib;
use gtk::{TextBuffer, TextTag, TextTagTable, TextView, gdk_pixbuf, pango};
use syntect::easy::HighlightLines;
use syntect::highlighting::FontStyle;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::theme::{Palette, css, rgba};

const SANS: &[&str] = &["Noto Sans", "Red Hat Text", "Cantarell", "Helvetica Neue", "Segoe UI", "DejaVu Sans", "Liberation Sans"];
const MONO: &[&str] = &["DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono", "Red Hat Mono", "SF Mono", "Menlo", "Consolas"];

static FONTS: OnceLock<(String, String)> = OnceLock::new();

/// Picks the first installed body and code font. Resolving this up front (rather than handing
/// Pango a comma-separated list) behaves the same on every Pango backend.
pub fn init_fonts(ctx: &pango::Context) {
    let installed: Vec<String> = ctx.list_families().iter().map(|f| f.name().to_lowercase()).collect();
    let pick = |list: &[&str], fallback: &str| {
        list.iter().find(|f| installed.contains(&f.to_lowercase())).map(|f| f.to_string()).unwrap_or_else(|| fallback.into())
    };
    let _ = FONTS.set((pick(SANS, "Sans"), pick(MONO, "Monospace")));
}

pub fn sans_font() -> &'static str {
    FONTS.get().map(|f| f.0.as_str()).unwrap_or("Sans")
}

pub fn mono_font() -> &'static str {
    FONTS.get().map(|f| f.1.as_str()).unwrap_or("Monospace")
}

const MARKDOWN_EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn", "mdwn", "mdtxt", "mdtext", "rmd"];
/// Max highlighted lines per code block; the rest is plain (keeps huge files snappy).
const MAX_HIGHLIGHT_LINES: usize = 5000;

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

pub enum DecoKind {
    /// Rounded background behind a fenced/indented code block. Holds the code for the copy button.
    CodeBlock { code: String },
    /// Vertical bar left of a blockquote or alert.
    Bar { color: u32 },
    /// Thin line under an h1/h2.
    HeadingRule,
    /// `---` thematic break.
    Rule,
    /// Thin separator above footnotes.
    ThinRule,
    /// Table grid. `cols` are column boundaries relative to the table's left edge.
    Table { cols: Vec<i32>, rows: Vec<i32>, header: bool },
    /// Task list checkbox, drawn over the placeholder character at `start`.
    Check { checked: bool },
    /// Alert icon, drawn over the placeholder character at `start`.
    AlertIcon { kind: AlertType, color: u32 },
}

pub struct Deco {
    pub kind: DecoKind,
    /// Offset of a character on the first line covered.
    pub start: i32,
    /// Offset of a character on the last line covered.
    pub end: i32,
    /// Left edge, relative to the page margin.
    pub indent: i32,
    /// Pixels at the top of the first line that belong to the gap above the block.
    pub top_inset: i32,
}

pub struct Link {
    pub start: i32,
    pub end: i32,
    pub target: String,
}

pub struct Heading {
    pub level: u8,
    pub text: String,
    pub offset: i32,
}

pub struct Rendered {
    pub buffer: TextBuffer,
    pub decos: Vec<Deco>,
    pub links: Vec<Link>,
    pub anchors: HashMap<String, i32>,
    pub headings: Vec<Heading>,
    /// Left-margin tags and their indent relative to the page margin (re-applied on resize).
    pub margin_tags: Vec<(TextTag, i32)>,
    /// Right-margin tags and their extra inset.
    pub right_tags: Vec<(TextTag, i32)>,
    /// (buffer offset, source line) for block starts — used to keep the scroll position across reloads.
    pub blocks: Vec<(i32, usize)>,
    /// True if images were scaled to the width available at render time.
    pub width_sensitive: bool,
}

impl Rendered {
    pub fn link_at(&self, offset: i32) -> Option<&Link> {
        self.links.iter().find(|l| offset >= l.start && offset < l.end)
    }

    pub fn offset_for_line(&self, line: usize) -> Option<i32> {
        self.blocks.iter().find(|(_, l)| *l >= line).or(self.blocks.last()).map(|(o, _)| *o)
    }

    pub fn line_for_offset(&self, offset: i32) -> Option<usize> {
        self.blocks.iter().take_while(|(o, _)| *o <= offset).last().map(|(_, l)| *l)
    }
}

// ---------------------------------------------------------------------------
// Syntax highlighting
// ---------------------------------------------------------------------------

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

pub fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn find_syntax<'a>(ss: &'a SyntaxSet, lang: &str) -> Option<&'a SyntaxReference> {
    let lang = lang.trim().to_ascii_lowercase();
    if lang.is_empty() {
        return None;
    }
    // Aliases for common languages missing from syntect's default set.
    let token = match lang.as_str() {
        "ts" | "typescript" | "tsx" | "jsx" | "mjs" | "cjs" | "json5" | "jsonc" => "js",
        "sh" | "shell" | "zsh" | "ksh" | "console" | "shell-session" | "bash" => "bash",
        "yml" => "yaml",
        "c++" | "hpp" | "cc" | "hh" | "cxx" => "cpp",
        "golang" => "go",
        "py3" | "python3" => "py",
        "dockerfile" | "containerfile" => "bash",
        "make" | "mk" => "makefile",
        other => other,
    };
    ss.find_syntax_by_token(token)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

thread_local! {
    /// Rendered SVG by (dark, source), so re-renders on resize or zoom skip layout.
    static MERMAID: RefCell<HashMap<(bool, String), Option<String>>> = RefCell::new(HashMap::new());
}

fn mermaid_svg(code: &str, pal: &Palette) -> Option<String> {
    let key = (pal.dark, code.to_string());
    if let Some(hit) = MERMAID.with_borrow(|m| m.get(&key).cloned()) {
        return hit;
    }
    use mermaid_rs_renderer::{RenderOptions, Theme};
    let mut theme = if pal.dark { Theme::dark() } else { Theme::mermaid_default() };
    theme.background = css(pal.canvas);
    // Layout measures text with the same font library resvg draws with, so name real families
    // ("Sans" is a Pango alias it doesn't know).
    theme.font_family = format!("'{}', 'DejaVu Sans', 'Liberation Sans', Arial, sans-serif", sans_font());
    let svg = mermaid_rs_renderer::render_with_options(code, RenderOptions { theme, ..Default::default() }).ok();
    MERMAID.with_borrow_mut(|m| {
        if m.len() > 64 {
            m.clear();
        }
        m.insert(key, svg.clone());
    });
    svg
}

/// Draws an SVG at `zoom`, no wider than `max`, on an opaque `bg`. Uses resvg rather than
/// gdk-pixbuf so diagrams don't depend on librsvg's pixbuf loader being installed.
/// Returns the picture and whether it was shrunk to fit `max`.
fn rasterize_svg(svg: &str, zoom: f64, max: i32, bg: u32) -> Option<(gdk_pixbuf::Pixbuf, bool)> {
    use resvg::{tiny_skia, usvg};
    static FONTDB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let fontdb = FONTDB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    });
    let opt = usvg::Options { fontdb: fontdb.clone(), ..Default::default() };
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let size = tree.size();
    let natural = (size.width() as f64 * zoom).round() as i32;
    let w = natural.min(max).max(1);
    let scale = w as f32 / size.width();
    let h = (size.height() * scale).round().max(1.0) as i32;
    let mut pixmap = tiny_skia::Pixmap::new(w as u32, h as u32)?;
    let [_, r, g, b] = bg.to_be_bytes();
    pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, 255));
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    // The background is opaque, so tiny-skia's premultiplied RGBA is also plain RGBA.
    let bytes = gtk::glib::Bytes::from_owned(pixmap.take());
    let pb = gdk_pixbuf::Pixbuf::from_bytes(&bytes, gdk_pixbuf::Colorspace::Rgb, true, 8, w, h, w * 4);
    Some((pb, natural > max))
}

pub fn is_markdown(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => MARKDOWN_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => true,
    }
}

fn options() -> Options<'static> {
    let mut o = Options::default();
    let e = &mut o.extension;
    e.strikethrough = true;
    e.table = true;
    e.autolink = true;
    e.tasklist = true;
    e.footnotes = true;
    e.alerts = true;
    e.shortcodes = true;
    e.description_lists = true;
    e.front_matter_delimiter = Some("---".into());
    o.parse.relaxed_tasklist_matching = true;
    o
}

fn plain_text<'n>(node: &'n AstNode<'n>) -> String {
    let mut s = String::new();
    for d in node.descendants() {
        match &d.data.borrow().value {
            NodeValue::Text(t) => s.push_str(t),
            NodeValue::Code(c) => s.push_str(&c.literal),
            NodeValue::ShortCode(sc) => s.push_str(&sc.emoji),
            NodeValue::SoftBreak | NodeValue::LineBreak => s.push(' '),
            _ => {}
        }
    }
    s
}

fn roman(mut n: usize) -> String {
    const T: &[(usize, &str)] = &[(1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")];
    let mut s = String::new();
    for &(v, r) in T {
        while n >= v {
            s.push_str(r);
            n -= v;
        }
    }
    s
}

fn alpha(n: usize) -> String {
    let mut n = n.max(1);
    let mut s = Vec::new();
    while n > 0 {
        n -= 1;
        s.push(b'a' + (n % 26) as u8);
        n /= 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let ent = &rest[1..e];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "copy" => Some('©'),
                "middot" => Some('·'),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

enum HtmlTok<'a> {
    Text(&'a str),
    Open(String, Vec<(String, String)>, bool),
    Close(String),
}

/// A forgiving tokenizer for the bits of HTML people put in READMEs.
fn html_tokens(src: &str) -> Vec<HtmlTok<'_>> {
    let mut out = Vec::new();
    let mut rest = src;
    while !rest.is_empty() {
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map(|i| &rest[i + 3..]).unwrap_or("");
            continue;
        }
        if rest.starts_with('<') {
            if let Some(end) = rest.find('>') {
                let inner = rest[1..end].trim();
                rest = &rest[end + 1..];
                if let Some(name) = inner.strip_prefix('/') {
                    out.push(HtmlTok::Close(name.trim().to_ascii_lowercase()));
                    continue;
                }
                let self_closing = inner.ends_with('/');
                let inner = inner.trim_end_matches('/');
                let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
                let name = inner[..name_end].to_ascii_lowercase();
                if name.starts_with('!') || name.starts_with('?') {
                    continue;
                }
                out.push(HtmlTok::Open(name, parse_attrs(&inner[name_end..]), self_closing));
                continue;
            }
            out.push(HtmlTok::Text(rest));
            break;
        }
        let next = rest.find('<').unwrap_or(rest.len());
        out.push(HtmlTok::Text(&rest[..next]));
        rest = &rest[next..];
    }
    out
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let name_end = rest.find(|c: char| c == '=' || c.is_whitespace()).unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        let mut value = String::new();
        if let Some(r) = rest.strip_prefix('=') {
            let r = r.trim_start();
            if let Some(q) = r.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let body = &r[1..];
                let end = body.find(q).unwrap_or(body.len());
                value = body[..end].to_string();
                rest = body.get(end + 1..).unwrap_or("").trim_start();
            } else {
                let end = r.find(char::is_whitespace).unwrap_or(r.len());
                value = r[..end].to_string();
                rest = r[end..].trim_start();
            }
        }
        if !name.is_empty() {
            attrs.push((name, decode_entities(&value)));
        }
    }
    attrs
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

struct Para {
    start: i32,
    line_starts: Vec<i32>,
    pa: i32,
    pb: i32,
    inner: i32,
    lm: i32,
    hang: i32,
    extra: Vec<TextTag>,
}

struct ListCtx {
    ordered: bool,
    next: usize,
    tight: bool,
    first: bool,
}

enum Marker {
    Text(String),
    Check(bool),
}

pub struct Renderer<'a> {
    view: &'a TextView,
    pal: &'a Palette,
    zoom: f64,
    base_dir: PathBuf,
    content_width: i32,
    buf: TextBuffer,
    table: TextTagTable,
    tags: HashMap<String, TextTag>,
    inline: Vec<TextTag>,
    indent: i32,
    gap: Option<i32>,
    para: Option<Para>,
    lists: Vec<ListCtx>,
    tight: bool,
    marker: Option<Marker>,
    pending_insets: Vec<usize>,
    links_open: Vec<(i32, String)>,
    html_open: Vec<(String, Option<TextTag>)>,
    html_center: bool,
    source_line: usize,
    anchorizer: Anchorizer,
    footnotes: usize,
    out: Rendered,
}

impl<'a> Renderer<'a> {
    pub fn new(view: &'a TextView, pal: &'a Palette, zoom: f64, base_dir: &Path, content_width: i32) -> Self {
        let table = TextTagTable::new();
        let buf = TextBuffer::new(Some(&table));
        let mut r = Renderer {
            view,
            pal,
            zoom,
            base_dir: base_dir.to_path_buf(),
            content_width,
            buf: buf.clone(),
            table,
            tags: HashMap::new(),
            inline: Vec::new(),
            indent: 0,
            gap: None,
            para: None,
            lists: Vec::new(),
            tight: false,
            marker: None,
            pending_insets: Vec::new(),
            links_open: Vec::new(),
            html_open: Vec::new(),
            html_center: false,
            source_line: 1,
            anchorizer: Anchorizer::new(),
            footnotes: 0,
            out: Rendered {
                buffer: buf,
                decos: Vec::new(),
                links: Vec::new(),
                anchors: HashMap::new(),
                headings: Vec::new(),
                margin_tags: Vec::new(),
                right_tags: Vec::new(),
                blocks: Vec::new(),
                width_sensitive: false,
            },
        };
        r.create_fixed_tags();
        r
    }

    fn s(&self, px: f64) -> i32 {
        (px * self.zoom).round() as i32
    }

    // ----------------------------------------------------------------- tags

    fn add_tag(&mut self, name: &str, f: impl FnOnce(&TextTag)) {
        let tag = TextTag::new(Some(name));
        f(&tag);
        self.table.add(&tag);
        self.tags.insert(name.to_string(), tag);
    }

    /// Fixed tags, created in priority order (later wins).
    fn create_fixed_tags(&mut self) {
        let p = self.pal;
        let (fg_muted, code_bg, accent) = (rgba(p.fg_muted), rgba(p.code_inline), rgba(p.accent));
        let s3 = self.s(3.0);
        let s4 = self.s(4.0);
        self.add_tag("muted", |t| t.set_foreground_rgba(Some(&fg_muted)));
        self.add_tag("code", |t| {
            t.set_family(Some(mono_font()));
            t.set_scale(0.85);
            t.set_background_rgba(Some(&code_bg));
        });
        self.add_tag("pre", |t| {
            t.set_family(Some(mono_font()));
            t.set_scale(0.85);
            t.set_pixels_inside_wrap(s3);
        });
        self.add_tag("small", |t| t.set_scale(0.8));
        self.add_tag("tiny", |t| t.set_scale(0.15));
        self.add_tag("sup", |t| {
            t.set_scale(0.75);
            t.set_rise(4 * pango::SCALE);
        });
        self.add_tag("sub", |t| {
            t.set_scale(0.75);
            t.set_rise(-2 * pango::SCALE);
        });
        self.add_tag("em", |t| t.set_style(pango::Style::Italic));
        self.add_tag("strong", |t| t.set_weight(pango::Weight::Semibold.into_glib()));
        self.add_tag("strike", |t| t.set_strikethrough(true));
        self.add_tag("underline", |t| t.set_underline(pango::Underline::Single));
        self.add_tag("mark", |t| t.set_background_rgba(Some(&rgba(p.hit))));
        self.add_tag("link", |t| t.set_foreground_rgba(Some(&accent)));
        self.add_tag("check", |t| t.set_foreground_rgba(Some(&fg_muted)));
        for (i, scale) in [2.0, 1.5, 1.25, 1.0, 0.875, 0.85].iter().enumerate() {
            let muted = i == 5;
            self.add_tag(&format!("h{}", i + 1), |t| {
                t.set_scale(*scale);
                t.set_weight(pango::Weight::Semibold.into_glib());
                if muted {
                    t.set_foreground_rgba(Some(&fg_muted));
                }
            });
        }
        self.add_tag("th", |t| t.set_weight(pango::Weight::Semibold.into_glib()));
        for (name, color) in [("note", p.accent), ("tip", p.success), ("important", p.done), ("warning", p.attention), ("caution", p.danger)] {
            let c = rgba(color);
            self.add_tag(&format!("alert-{name}"), |t| {
                t.set_foreground_rgba(Some(&c));
                t.set_weight(pango::Weight::Medium.into_glib());
            });
        }
        self.add_tag("center", |t| t.set_justification(gtk::Justification::Center));
        self.add_tag("nowrap", |t| t.set_wrap_mode(gtk::WrapMode::None));
        self.add_tag("li-spacing", |t| t.set_pixels_inside_wrap(s4));
        let (hit, cur) = (rgba(p.hit), rgba(p.hit_current));
        self.add_tag("hit", |t| t.set_background_rgba(Some(&hit)));
        self.add_tag("hit-current", |t| t.set_background_rgba(Some(&cur)));
    }

    fn tag(&self, name: &str) -> TextTag {
        self.tags[name].clone()
    }

    fn dyn_tag(&mut self, name: String, f: impl FnOnce(&TextTag)) -> TextTag {
        if let Some(t) = self.tags.get(&name) {
            return t.clone();
        }
        let tag = TextTag::new(Some(&name));
        f(&tag);
        self.table.add(&tag);
        self.tags.insert(name, tag.clone());
        tag
    }

    fn pa_tag(&mut self, px: i32) -> TextTag {
        self.dyn_tag(format!("pa{px}"), |t| t.set_pixels_above_lines(px))
    }

    fn pb_tag(&mut self, px: i32) -> TextTag {
        self.dyn_tag(format!("pb{px}"), |t| t.set_pixels_below_lines(px))
    }

    fn lm_tag(&mut self, indent: i32) -> TextTag {
        let name = format!("lm{indent}");
        if let Some(t) = self.tags.get(&name) {
            return t.clone();
        }
        let tag = self.dyn_tag(name, |t| t.set_left_margin(indent));
        self.out.margin_tags.push((tag.clone(), indent));
        tag
    }

    fn rm_tag(&mut self, extra: i32) -> TextTag {
        let name = format!("rm{extra}");
        if let Some(t) = self.tags.get(&name) {
            return t.clone();
        }
        let tag = self.dyn_tag(name, |t| t.set_right_margin(extra));
        self.out.right_tags.push((tag.clone(), extra));
        tag
    }

    fn indent_tag(&mut self, px: i32) -> TextTag {
        self.dyn_tag(format!("in{px}"), |t| t.set_indent(px))
    }

    fn color_tag(&mut self, fg: u32) -> TextTag {
        self.dyn_tag(format!("fg{fg:06x}"), |t| t.set_foreground_rgba(Some(&rgba(fg))))
    }

    fn bg_tag(&mut self, bg: u32) -> TextTag {
        self.dyn_tag(format!("bg{bg:06x}"), |t| t.set_background_rgba(Some(&rgba(bg))))
    }

    // ------------------------------------------------------- block plumbing

    fn offset(&self) -> i32 {
        self.buf.end_iter().offset()
    }

    /// Resolves the vertical gap before a block whose own top margin is `top`.
    fn take_gap(&mut self, top: i32) -> i32 {
        match self.gap.take() {
            None => 0,
            Some(g) => g.max(top),
        }
    }

    fn end_block(&mut self, bottom: i32) {
        self.gap = Some(self.gap.unwrap_or(0).max(bottom));
    }

    fn start_para(&mut self, top: i32, extra_pad_top: i32, lm: i32, extra: Vec<TextTag>) -> i32 {
        self.finish_para(0);
        let gap = self.take_gap(top);
        for i in self.pending_insets.drain(..) {
            self.out.decos[i].top_inset = gap;
        }
        let start = self.offset();
        self.out.blocks.push((start, self.source_line));
        let mut extra = extra;
        if self.html_center {
            extra.push(self.tag("center"));
        }
        self.para = Some(Para { start, line_starts: Vec::new(), pa: gap + extra_pad_top, pb: 0, inner: 0, lm, hang: 0, extra });
        if let Some(marker) = self.marker.take() {
            self.insert_marker(marker);
        }
        gap
    }

    fn insert_marker(&mut self, marker: Marker) {
        let text = match &marker {
            Marker::Text(t) => format!("{t}\u{2002}"),
            Marker::Check(_) => "\u{2003}\u{2003}".to_string(),
        };
        let layout = self.view.create_pango_layout(Some(&text));
        let width = layout.pixel_size().0;
        let start = self.offset();
        match marker {
            Marker::Text(_) => {
                let tags = self.inline.clone();
                self.insert_tagged(&text, &tags);
            }
            Marker::Check(checked) => {
                let check = self.tag("check");
                self.insert_tagged(&text, &[check]);
                self.out.decos.push(Deco { kind: DecoKind::Check { checked }, start, end: start, indent: 0, top_inset: 0 });
            }
        }
        if let Some(p) = self.para.as_mut() {
            p.hang = -width;
        }
    }

    fn ensure_para(&mut self) {
        if self.para.is_none() {
            let lm = self.indent;
            self.start_para(0, 0, lm, Vec::new());
        }
    }

    /// Ends the current paragraph: inserts the newline and applies line-level tags.
    fn finish_para(&mut self, bottom_pad: i32) {
        let Some(mut p) = self.para.take() else { return };
        p.pb += bottom_pad;
        let mut end = self.buf.end_iter();
        self.buf.insert(&mut end, "\n");
        let end_off = self.offset();
        let mut starts = vec![p.start];
        starts.extend(p.line_starts.iter().copied());
        let lm = self.lm_tag(p.lm);
        let hang = (p.hang != 0).then(|| self.indent_tag(p.hang));
        let pa = (p.pa > 0).then(|| self.pa_tag(p.pa));
        let pb = (p.pb > 0).then(|| self.pb_tag(p.pb));
        let inner_pb = (p.inner > 0).then(|| self.pb_tag(p.inner));
        let n = starts.len();
        let range = |buf: &TextBuffer, a: i32, b: i32| (buf.iter_at_offset(a), buf.iter_at_offset(b));
        let (s, e) = range(&self.buf, p.start, end_off);
        self.buf.apply_tag(&lm, &s, &e);
        for t in &p.extra {
            self.buf.apply_tag(t, &s, &e);
        }
        for (i, &ls) in starts.iter().enumerate() {
            let le = starts.get(i + 1).copied().unwrap_or(end_off);
            let (s, e) = range(&self.buf, ls, le);
            if i == 0 {
                if let Some(t) = &hang {
                    self.buf.apply_tag(t, &s, &e);
                }
                if let Some(t) = &pa {
                    self.buf.apply_tag(t, &s, &e);
                }
            }
            if i == n - 1 {
                if let Some(t) = &pb {
                    self.buf.apply_tag(t, &s, &e);
                }
            } else if let Some(t) = &inner_pb {
                self.buf.apply_tag(t, &s, &e);
            }
        }
    }

    fn hard_break(&mut self) {
        self.ensure_para();
        let mut end = self.buf.end_iter();
        self.buf.insert(&mut end, "\n");
        let off = self.offset();
        if let Some(p) = self.para.as_mut() {
            p.line_starts.push(off);
        }
    }

    fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.ensure_para();
        let tags = self.inline.clone();
        self.insert_tagged(s, &tags);
    }

    fn insert_tagged(&mut self, s: &str, tags: &[TextTag]) {
        let start = self.offset();
        let mut end = self.buf.end_iter();
        self.buf.insert(&mut end, s);
        if !tags.is_empty() {
            let a = self.buf.iter_at_offset(start);
            let b = self.buf.end_iter();
            for t in tags {
                self.buf.apply_tag(t, &a, &b);
            }
        }
    }

    fn text_with(&mut self, s: &str, extra: &[TextTag]) {
        self.inline.extend_from_slice(extra);
        self.text(s);
        let n = self.inline.len() - extra.len();
        self.inline.truncate(n);
    }

    fn push(&mut self, name: &str) {
        let t = self.tag(name);
        self.inline.push(t);
    }

    fn pop(&mut self) {
        self.inline.pop();
    }

    // ------------------------------------------------------------- entry points

    pub fn markdown(mut self, src: &str) -> Rendered {
        let arena = Arena::new();
        let root = parse_document(&arena, src, &options());
        self.render_children(root);
        self.finish_para(0);
        self.trim_trailing_newline();
        self.out
    }

    /// Shows a source file with syntax highlighting (like a GitHub blob view).
    pub fn source(mut self, name: &str, text: &str, lang: &str) -> Rendered {
        let lm = self.indent;
        let strong = self.tag("strong");
        self.start_para(0, 0, lm, vec![strong]);
        self.text(name);
        let muted = self.tag("muted");
        let small = self.tag("small");
        let lines = text.lines().count();
        self.text_with(&format!("   {lines} lines"), &[muted, small]);
        self.finish_para(0);
        self.end_block(self.s(12.0));
        self.code_block(text, lang);
        self.trim_trailing_newline();
        self.out
    }

    fn trim_trailing_newline(&mut self) {
        let mut end = self.buf.end_iter();
        let mut prev = end.clone();
        if prev.backward_char() && prev.char() == Some('\n') {
            self.buf.delete(&mut prev, &mut end);
        }
    }

    fn render_children<'n>(&mut self, node: &'n AstNode<'n>) {
        for child in node.children() {
            self.render(child);
        }
    }

    fn render<'n>(&mut self, node: &'n AstNode<'n>) {
        let line = node.data.borrow().sourcepos.start.line;
        let value = node.data.borrow().value.clone();
        if value.block() {
            self.source_line = line;
        }
        match value {
            NodeValue::Document => self.render_children(node),
            NodeValue::FrontMatter(_) => {}
            NodeValue::Paragraph => {
                let (top, bottom) = if self.tight { (0, 0) } else { (0, self.s(16.0)) };
                let lm = self.indent;
                self.start_para(top, 0, lm, Vec::new());
                self.render_children(node);
                self.finish_para(0);
                self.end_block(bottom);
            }
            NodeValue::Heading(h) => self.heading(node, h.level),
            NodeValue::ThematicBreak => self.rule(),
            NodeValue::BlockQuote | NodeValue::MultilineBlockQuote(_) => {
                let color = self.pal.border;
                self.quote(node, color, None);
            }
            NodeValue::Alert(a) => {
                let color = match a.alert_type {
                    AlertType::Note => self.pal.accent,
                    AlertType::Tip => self.pal.success,
                    AlertType::Important => self.pal.done,
                    AlertType::Warning => self.pal.attention,
                    AlertType::Caution => self.pal.danger,
                };
                let title = a.title.clone().unwrap_or_else(|| a.alert_type.default_title().to_string());
                self.quote(node, color, Some((a.alert_type, title)));
            }
            NodeValue::List(l) => {
                let nested = !self.lists.is_empty();
                let saved_indent = self.indent;
                let saved_tight = self.tight;
                if !nested || !self.tight {
                    let g = self.take_gap(0);
                    self.gap = Some(g);
                }
                self.indent += self.s(32.0);
                self.lists.push(ListCtx { ordered: l.list_type == ListType::Ordered, next: l.start.max(if l.list_type == ListType::Ordered { 0 } else { 1 }), tight: l.tight, first: true });
                self.render_children(node);
                self.lists.pop();
                self.indent = saved_indent;
                self.tight = saved_tight;
                if !(nested && saved_tight) {
                    self.end_block(self.s(16.0));
                }
            }
            NodeValue::Item(_) => self.item(node, None),
            NodeValue::TaskItem(t) => self.item(node, Some(t.symbol.is_some())),
            NodeValue::CodeBlock(cb) => {
                let lang = cb.info.split([' ', ',', '{']).next().unwrap_or("").to_string();
                let code = cb.literal.strip_suffix('\n').unwrap_or(&cb.literal).to_string();
                if !(lang.eq_ignore_ascii_case("mermaid") && self.mermaid(&code)) {
                    self.code_block(&code, &lang);
                }
            }
            NodeValue::HtmlBlock(h) => {
                self.html(&h.literal, true);
                self.finish_para(0);
                if self.out.blocks.last().map(|(o, _)| *o) != Some(self.offset()) {
                    self.end_block(self.s(16.0));
                }
                self.html_center = false;
            }
            NodeValue::Table(t) => self.table(node, t.num_columns),
            NodeValue::FootnoteDefinition(def) => self.footnote(node, &def.name),
            NodeValue::DescriptionList => {
                self.render_children(node);
                self.end_block(self.s(16.0));
            }
            NodeValue::DescriptionItem(_) => self.render_children(node),
            NodeValue::DescriptionTerm => {
                let lm = self.indent;
                let tags = vec![self.tag("strong"), self.tag("em")];
                self.start_para(self.s(16.0), 0, lm, tags);
                self.render_inline_children_of_blocks(node);
                self.finish_para(0);
                self.end_block(0);
            }
            NodeValue::DescriptionDetails => {
                let saved = self.indent;
                self.indent += self.s(16.0);
                let saved_tight = self.tight;
                self.tight = true;
                self.render_children(node);
                self.tight = saved_tight;
                self.indent = saved;
                self.end_block(self.s(16.0));
            }
            NodeValue::Math(m) if m.display_math => self.code_block(&m.literal, ""),
            NodeValue::Subtext => self.render_children(node),

            // ------------------------------------------------------- inlines
            NodeValue::Text(t) => {
                let t = t.to_string();
                self.text(&t);
            }
            NodeValue::SoftBreak => self.text(" "),
            NodeValue::LineBreak => self.hard_break(),
            NodeValue::Code(c) => {
                let code = self.tag("code");
                self.text_with(&format!("\u{2009}{}\u{2009}", c.literal), &[code]);
            }
            NodeValue::Math(m) => {
                let code = self.tag("code");
                self.text_with(&m.literal, &[code]);
            }
            NodeValue::HtmlInline(h) => self.html(&h, false),
            NodeValue::Emph => self.wrap(node, "em"),
            NodeValue::Strong => self.wrap(node, "strong"),
            NodeValue::Strikethrough => self.wrap(node, "strike"),
            NodeValue::Underline => self.wrap(node, "underline"),
            NodeValue::Superscript => self.wrap(node, "sup"),
            NodeValue::Subscript => self.wrap(node, "sub"),
            NodeValue::Highlight | NodeValue::Insert => self.wrap(node, "mark"),
            NodeValue::Link(l) => {
                self.ensure_para();
                let start = self.offset();
                self.push("link");
                self.render_children(node);
                self.pop();
                let end = self.offset();
                self.out.links.push(Link { start, end, target: l.url.clone() });
            }
            NodeValue::WikiLink(l) => {
                self.ensure_para();
                let start = self.offset();
                self.push("link");
                self.render_children(node);
                self.pop();
                let end = self.offset();
                self.out.links.push(Link { start, end, target: l.url.clone() });
            }
            NodeValue::Image(l) => {
                let alt = plain_text(node);
                self.image(&l.url, &alt, None);
            }
            NodeValue::FootnoteReference(r) => {
                self.ensure_para();
                let start = self.offset();
                let tags = [self.tag("sup"), self.tag("link")];
                self.text_with(&format!("[{}]", r.ix), &tags);
                let end = self.offset();
                self.out.links.push(Link { start, end, target: format!("#fn-{}", r.name) });
            }
            NodeValue::ShortCode(sc) => {
                let e = sc.emoji.clone();
                self.text(&e);
            }
            NodeValue::Escaped | NodeValue::SpoileredText => self.render_children(node),
            NodeValue::EscapedTag(t) => self.text(t),
            NodeValue::Raw(t) => self.text(&t),
            _ => self.render_children(node),
        }
    }

    /// Renders inline content found inside (possibly nested) blocks without starting new paragraphs.
    fn render_inline_children_of_blocks<'n>(&mut self, node: &'n AstNode<'n>) {
        for child in node.children() {
            if matches!(child.data.borrow().value, NodeValue::Paragraph) {
                self.render_inline_children_of_blocks(child);
            } else {
                self.render(child);
            }
        }
    }

    fn wrap<'n>(&mut self, node: &'n AstNode<'n>, tag: &str) {
        self.push(tag);
        self.render_children(node);
        self.pop();
    }

    fn heading<'n>(&mut self, node: &'n AstNode<'n>, level: u8) {
        let level = level.clamp(1, 6);
        let text = plain_text(node);
        let slug = self.anchorizer.anchorize(&text);
        let lm = self.indent;
        let tag = self.tag(&format!("h{level}"));
        self.start_para(self.s(24.0), 0, lm, vec![tag.clone()]);
        let start = self.para.as_ref().map(|p| p.start).unwrap_or(0);
        self.inline.push(tag);
        self.render_children(node);
        self.inline.pop();
        let scale = [2.0, 1.5, 1.25, 1.0, 0.875, 0.85][level as usize - 1];
        let ruled = level <= 2;
        let pad = if ruled { self.s(16.0 * scale * 0.3) + 1 } else { 0 };
        self.finish_para(pad);
        if ruled {
            let indent = self.indent;
            self.out.decos.push(Deco { kind: DecoKind::HeadingRule, start, end: start, indent, top_inset: 0 });
        }
        self.end_block(self.s(16.0));
        self.out.anchors.entry(slug).or_insert(start);
        self.out.headings.push(Heading { level, text, offset: start });
    }

    fn rule(&mut self) {
        let lm = self.indent;
        let tiny = self.tag("tiny");
        let gap = self.start_para(self.s(24.0), 0, lm, vec![tiny]);
        let start = self.para.as_ref().map(|p| p.start).unwrap_or(0);
        self.finish_para(0);
        let indent = self.indent;
        self.out.decos.push(Deco { kind: DecoKind::Rule, start, end: start, indent, top_inset: gap });
        self.end_block(self.s(24.0));
    }

    fn quote<'n>(&mut self, node: &'n AstNode<'n>, color: u32, alert: Option<(AlertType, String)>) {
        self.finish_para(0);
        let gap = self.take_gap(0);
        self.gap = Some(gap);
        let deco_index = self.out.decos.len();
        let indent = self.indent;
        let start = self.offset();
        self.out.decos.push(Deco { kind: DecoKind::Bar { color }, start, end: start, indent, top_inset: 0 });
        self.pending_insets.push(deco_index);
        let saved_indent = self.indent;
        let saved_tight = self.tight;
        self.tight = false;
        self.indent += self.s(20.0);
        let is_alert = alert.is_some();
        if let Some((kind, title)) = alert {
            let name = match kind {
                AlertType::Note => "note",
                AlertType::Tip => "tip",
                AlertType::Important => "important",
                AlertType::Warning => "warning",
                AlertType::Caution => "caution",
            };
            let tag = self.tag(&format!("alert-{name}"));
            let lm = self.indent;
            self.start_para(0, self.s(8.0), lm, vec![tag]);
            let icon_at = self.offset();
            self.text("\u{2003}\u{2002}");
            self.text(&title);
            self.finish_para(0);
            self.out.decos.push(Deco { kind: DecoKind::AlertIcon { kind, color }, start: icon_at, end: icon_at, indent: 0, top_inset: 0 });
            self.end_block(self.s(8.0));
        } else {
            self.push("muted");
        }
        self.render_children(node);
        self.finish_para(0);
        if !is_alert {
            self.pop();
        }
        self.indent = saved_indent;
        self.tight = saved_tight;
        self.pending_insets.retain(|&i| i != deco_index);
        let end = (self.offset() - 1).max(start);
        self.out.decos[deco_index].end = end;
        self.end_block(self.s(16.0));
    }

    fn item<'n>(&mut self, node: &'n AstNode<'n>, task: Option<bool>) {
        let depth = self.lists.len().saturating_sub(1);
        let Some(ctx) = self.lists.last_mut() else {
            self.render_children(node);
            return;
        };
        let marker = match task {
            Some(checked) => Marker::Check(checked),
            None if ctx.ordered => {
                let n = ctx.next;
                ctx.next += 1;
                Marker::Text(match depth {
                    0 => format!("{n}."),
                    1 => format!("{}.", roman(n)),
                    _ => format!("{}.", alpha(n)),
                })
            }
            None => Marker::Text(["•", "◦", "▪"][depth.min(2)].to_string()),
        };
        let tight = ctx.tight;
        let first = std::mem::replace(&mut ctx.first, false);
        if !first {
            let g = self.gap.unwrap_or(0).max(self.s(4.0));
            self.gap = Some(g);
        }
        self.marker = Some(marker);
        let saved = self.tight;
        self.tight = tight;
        self.render_children(node);
        if self.marker.take().is_some() {
            // empty item: still show the marker
            let lm = self.indent;
            self.start_para(0, 0, lm, Vec::new());
            self.finish_para(0);
        }
        self.finish_para(0);
        self.tight = saved;
        if tight {
            self.end_block(0);
        }
    }

    fn code_block(&mut self, code: &str, lang: &str) {
        let indent = self.indent;
        let pad = self.s(16.0);
        let pre = self.tag("pre");
        let rm = self.rm_tag(pad);
        let gap = self.start_para(0, pad, indent + pad, vec![pre, rm]);
        let start = self.para.as_ref().map(|p| p.start).unwrap_or(0);
        let inner = self.s(3.0);
        if let Some(p) = self.para.as_mut() {
            p.inner = inner;
        }
        let ss = syntaxes();
        let syntax = find_syntax(ss, lang);
        match syntax {
            Some(syntax) if code.lines().count() <= MAX_HIGHLIGHT_LINES => {
                let mut hl = HighlightLines::new(syntax, &self.pal.syntax);
                let mut first = true;
                for line in LinesWithEndings::from(code) {
                    if !first {
                        self.hard_break();
                    }
                    first = false;
                    let Ok(regions) = hl.highlight_line(line, ss) else {
                        self.text(line.trim_end_matches('\n'));
                        continue;
                    };
                    for (style, piece) in regions {
                        let piece = piece.trim_end_matches('\n');
                        if piece.is_empty() {
                            continue;
                        }
                        let c = style.foreground;
                        let mut tags = vec![self.color_tag(u32::from_be_bytes([0, c.r, c.g, c.b]))];
                        if style.background.a > 0 {
                            let b = style.background;
                            tags.push(self.bg_tag(u32::from_be_bytes([0, b.r, b.g, b.b])));
                        }
                        if style.font_style.contains(FontStyle::BOLD) {
                            tags.push(self.tag("strong"));
                        }
                        if style.font_style.contains(FontStyle::ITALIC) {
                            tags.push(self.tag("em"));
                        }
                        self.text_with(piece, &tags);
                    }
                }
            }
            _ => {
                for (i, line) in code.split('\n').enumerate() {
                    if i > 0 {
                        self.hard_break();
                    }
                    self.text(line);
                }
            }
        }
        // Make sure an empty code block still produces a line.
        self.ensure_para();
        let end = self.para.as_ref().map(|p| *p.line_starts.last().unwrap_or(&p.start)).unwrap_or(start);
        self.finish_para(pad);
        self.out.decos.push(Deco { kind: DecoKind::CodeBlock { code: code.to_string() }, start, end, indent, top_inset: gap });
        self.end_block(self.s(16.0));
    }

    /// Renders a ```mermaid block as a picture. Returns false (so the caller shows the source as a
    /// code block) if the diagram doesn't parse.
    fn mermaid(&mut self, code: &str) -> bool {
        let Some(svg) = mermaid_svg(code, self.pal) else { return false };
        let max = (self.content_width - self.indent).max(self.s(100.0));
        let Some((pb, capped)) = rasterize_svg(&svg, self.zoom, max, self.pal.canvas) else { return false };
        self.out.width_sensitive |= capped;
        let lm = self.indent;
        self.start_para(0, 0, lm, Vec::new());
        let mut end = self.buf.end_iter();
        self.buf.insert_pixbuf(&mut end, &pb);
        self.finish_para(0);
        self.end_block(self.s(16.0));
        true
    }

    fn table<'n>(&mut self, node: &'n AstNode<'n>, ncols: usize) {
        let rows: Vec<&'n AstNode<'n>> = node.children().collect();
        if rows.is_empty() || ncols == 0 {
            return;
        }
        // Measure natural column widths.
        let pad = self.s(13.0);
        let slack = self.s(10.0);
        let layout = self.view.create_pango_layout(None);
        let mut widths = vec![self.s(40.0); ncols];
        for row in &rows {
            let header = matches!(row.data.borrow().value, NodeValue::TableRow(true));
            for (c, cell) in row.children().enumerate().take(ncols) {
                let text = plain_text(cell).replace('\t', " ");
                layout.set_text(&text);
                let attrs = pango::AttrList::new();
                if header {
                    attrs.insert(pango::AttrInt::new_weight(pango::Weight::Semibold));
                }
                layout.set_attributes(Some(&attrs));
                let w = layout.pixel_size().0 + 2 * pad + slack;
                widths[c] = widths[c].max(w);
            }
        }
        let mut cols = vec![0];
        for w in &widths {
            cols.push(cols.last().unwrap() + w);
        }
        let mut tabs = pango::TabArray::new(ncols as i32, true);
        for c in 0..ncols {
            tabs.set_tab(c as i32, pango::TabAlign::Left, cols[c]);
        }
        let table_id = self.out.decos.len();
        let tabs_tag = self.dyn_tag(format!("tabs{table_id}"), |t| t.set_tabs(Some(&tabs)));
        let nowrap = self.tag("nowrap");
        let indent = self.indent;
        let cell_pad = self.s(6.0);
        let mut row_offsets = Vec::new();
        let mut gap0 = 0;
        let mut has_header = false;
        for (r, row) in rows.iter().enumerate() {
            let header = matches!(row.data.borrow().value, NodeValue::TableRow(true));
            has_header |= header && r == 0;
            let mut extra = vec![tabs_tag.clone(), nowrap.clone()];
            if header {
                extra.push(self.tag("th"));
            }
            let saved_center = std::mem::replace(&mut self.html_center, false);
            let gap = self.start_para(0, cell_pad, indent + pad, extra);
            self.html_center = saved_center;
            if r == 0 {
                gap0 = gap;
            }
            row_offsets.push(self.para.as_ref().map(|p| p.start).unwrap_or(0));
            for (c, cell) in row.children().enumerate().take(ncols) {
                if c > 0 {
                    self.text("\t");
                }
                if header {
                    self.push("th");
                }
                self.table_cell(cell);
                if header {
                    self.pop();
                }
            }
            self.ensure_para();
            self.finish_para(cell_pad);
        }
        let start = row_offsets[0];
        let end = *row_offsets.last().unwrap();
        self.out.decos.push(Deco { kind: DecoKind::Table { cols, rows: row_offsets, header: has_header }, start, end, indent, top_inset: gap0 });
        self.end_block(self.s(16.0));
    }

    fn table_cell<'n>(&mut self, cell: &'n AstNode<'n>) {
        for child in cell.children() {
            let v = child.data.borrow().value.clone();
            match v {
                NodeValue::LineBreak | NodeValue::SoftBreak => self.text(" "),
                NodeValue::Text(t) => {
                    let t = t.replace('\t', " ");
                    self.text(&t);
                }
                _ => self.render(child),
            }
        }
    }

    fn footnote<'n>(&mut self, node: &'n AstNode<'n>, name: &str) {
        if self.footnotes == 0 {
            let lm = self.indent;
            let tiny = self.tag("tiny");
            let gap = self.start_para(self.s(32.0), 0, lm, vec![tiny]);
            let start = self.para.as_ref().map(|p| p.start).unwrap_or(0);
            self.finish_para(0);
            let indent = self.indent;
            self.out.decos.push(Deco { kind: DecoKind::ThinRule, start, end: start, indent, top_inset: gap });
            self.end_block(self.s(8.0));
        }
        self.footnotes += 1;
        let saved_indent = self.indent;
        let saved_tight = self.tight;
        self.indent += self.s(20.0);
        self.tight = true;
        self.marker = Some(Marker::Text(format!("{}.", self.footnotes)));
        let anchor_at = self.offset();
        self.out.anchors.insert(format!("fn-{name}"), anchor_at);
        self.push("small");
        self.push("muted");
        self.render_children(node);
        self.finish_para(0);
        self.pop();
        self.pop();
        self.marker = None;
        self.indent = saved_indent;
        self.tight = saved_tight;
        self.end_block(self.s(6.0));
    }

    fn image(&mut self, url: &str, alt: &str, width_attr: Option<i32>) {
        self.ensure_para();
        let local = !url.contains("://") && !url.starts_with("data:");
        let path = local.then(|| self.base_dir.join(percent_decode(url.split(['?', '#']).next().unwrap_or(""))));
        if let Some(path) = path.filter(|p| p.is_file()) {
            if let Some((_, w, h)) = gdk_pixbuf::Pixbuf::file_info(&path) {
                let max = (self.content_width - self.indent).max(self.s(100.0));
                let mut target = width_attr.map(|w| self.s(w as f64)).unwrap_or(self.s(w as f64)).min(max).max(1);
                if target < w {
                    self.out.width_sensitive |= target == max;
                } else {
                    target = target.min(w.max(1) * 4);
                }
                let th = ((h as f64) * target as f64 / w.max(1) as f64).round().max(1.0) as i32;
                if let Ok(pb) = gdk_pixbuf::Pixbuf::from_file_at_scale(&path, target, th, false) {
                    let mut end = self.buf.end_iter();
                    self.buf.insert_pixbuf(&mut end, &pb);
                    return;
                }
            }
        }
        // Remote or missing image: show its alt text.
        let label = if alt.trim().is_empty() { "image".to_string() } else { alt.trim().to_string() };
        let muted = self.tag("muted");
        self.text_with(&format!("[{label}]"), &[muted]);
    }

    // ------------------------------------------------------------------ html

    fn html(&mut self, src: &str, block: bool) {
        let mut pending_ws = false;
        for tok in html_tokens(src) {
            match tok {
                HtmlTok::Text(t) => {
                    let decoded = decode_entities(t);
                    let mut collapsed = String::with_capacity(decoded.len());
                    for c in decoded.chars() {
                        if c.is_whitespace() && c != '\u{a0}' {
                            pending_ws = true;
                            continue;
                        }
                        if pending_ws && (self.para.is_some() || !collapsed.is_empty()) {
                            collapsed.push(' ');
                        }
                        pending_ws = false;
                        collapsed.push(c);
                    }
                    if !collapsed.is_empty() {
                        self.text(&collapsed);
                    }
                    if !block && pending_ws {
                        self.text(" ");
                        pending_ws = false;
                    }
                }
                HtmlTok::Open(name, attrs, self_closing) => {
                    match name.as_str() {
                        "br" => self.hard_break(),
                        "img" => {
                            let src = attr(&attrs, "src").unwrap_or("").to_string();
                            let alt = attr(&attrs, "alt").unwrap_or("").to_string();
                            let width = attr(&attrs, "width").and_then(|w| w.trim_end_matches("px").parse().ok());
                            self.image(&src, &alt, width);
                        }
                        "hr" => self.rule(),
                        "p" | "div" | "details" | "section" | "center" | "picture" if block => {
                            self.finish_para(0);
                            if name == "center" || attr(&attrs, "align").is_some_and(|a| a.eq_ignore_ascii_case("center")) {
                                self.html_center = true;
                            }
                        }
                        "summary" => {
                            self.finish_para(0);
                            self.push("strong");
                            self.text("▸ ");
                            self.html_open.push(("summary".into(), None));
                        }
                        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                            self.finish_para(0);
                            if attr(&attrs, "align").is_some_and(|a| a.eq_ignore_ascii_case("center")) {
                                self.html_center = true;
                            }
                            let lm = self.indent;
                            let tag = self.tag(&name);
                            self.start_para(self.s(24.0), 0, lm, vec![tag.clone()]);
                            self.inline.push(tag.clone());
                            self.html_open.push((name.clone(), Some(tag)));
                        }
                        "a" if !self_closing => {
                            self.ensure_para();
                            let href = attr(&attrs, "href").unwrap_or("").to_string();
                            let start = self.offset();
                            let tag = (!href.is_empty()).then(|| self.tag("link"));
                            if let Some(t) = &tag {
                                self.inline.push(t.clone());
                            }
                            self.links_open.push((start, href));
                            self.html_open.push(("a".into(), tag));
                        }
                        "b" | "strong" | "i" | "em" | "code" | "kbd" | "tt" | "samp" | "sup" | "sub" | "s" | "del" | "strike" | "u" | "ins" | "mark" | "small"
                            if !self_closing =>
                        {
                            let tag_name = match name.as_str() {
                                "b" | "strong" => "strong",
                                "i" | "em" => "em",
                                "code" | "kbd" | "tt" | "samp" => "code",
                                "s" | "del" | "strike" => "strike",
                                "u" | "ins" => "underline",
                                other => other,
                            };
                            let tag = self.tag(tag_name);
                            self.inline.push(tag.clone());
                            self.html_open.push((name.clone(), Some(tag)));
                        }
                        _ => {}
                    }
                }
                HtmlTok::Close(name) => match name.as_str() {
                    "p" | "div" | "details" | "section" | "center" | "picture" if block => {
                        self.finish_para(0);
                        self.html_center = false;
                        self.end_block(self.s(16.0));
                    }
                    _ => {
                        if let Some(pos) = self.html_open.iter().rposition(|(n, _)| *n == name) {
                            let (n, tag) = self.html_open.remove(pos);
                            if let Some(tag) = tag {
                                if let Some(i) = self.inline.iter().rposition(|t| *t == tag) {
                                    self.inline.remove(i);
                                }
                            }
                            match n.as_str() {
                                "a" => {
                                    if let Some((start, href)) = self.links_open.pop() {
                                        let end = self.offset();
                                        if !href.is_empty() && end > start {
                                            self.out.links.push(Link { start, end, target: href });
                                        }
                                    }
                                }
                                "summary" => {
                                    self.pop();
                                    self.finish_para(0);
                                    self.end_block(self.s(8.0));
                                }
                                h if h.len() == 2 && h.starts_with('h') => {
                                    self.finish_para(0);
                                    self.html_center = false;
                                    self.end_block(self.s(16.0));
                                }
                                _ => {}
                            }
                        }
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities() {
        assert_eq!(decode_entities("a &amp; b &lt;x&gt; &#65;&#x42; &bogus;"), "a & b <x> AB &bogus;");
    }

    #[test]
    fn html_tokenizer() {
        let toks = html_tokens(r#"<p align="center"><img src='a.png' width=100 /> hi<!-- c --></p>"#);
        assert!(matches!(&toks[0], HtmlTok::Open(n, a, false) if n == "p" && attr(a, "align") == Some("center")));
        assert!(matches!(&toks[1], HtmlTok::Open(n, a, true) if n == "img" && attr(a, "src") == Some("a.png") && attr(a, "width") == Some("100")));
        assert!(matches!(&toks[2], HtmlTok::Text(" hi")));
        assert!(matches!(&toks[3], HtmlTok::Close(n) if n == "p"));
    }

    #[test]
    fn numbering() {
        assert_eq!(roman(14), "xiv");
        assert_eq!(alpha(1), "a");
        assert_eq!(alpha(27), "aa");
    }
}
