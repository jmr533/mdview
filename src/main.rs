//! mdview — a fast, lightweight Markdown viewer.
//!
//! Markdown is parsed with comrak and drawn natively with GTK3 (GtkTextView plus a few
//! painted decorations). No web engine is involved, which keeps it quick to start and
//! cheap to draw — including over X11 forwarding and VNC.

mod doc;
mod paint;
mod theme;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use notify::{RecursiveMode, Watcher};

use doc::{DecoKind, Rendered, Renderer};
use theme::{Palette, ThemeChoice, css};

const WELCOME_MD: &str = include_str!("../assets/welcome.md");
const ZOOM_STEPS: &[f64] = &[0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];
const MAX_TEXT_FILE: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Settings, persisted to ~/.config/mdview/settings
// ---------------------------------------------------------------------------

struct Settings {
    theme: ThemeChoice,
    zoom: f64,
    outline: bool,
    width: i32,
    height: i32,
}

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("mdview").join("settings"))
}

fn load_settings() -> Settings {
    let mut s = Settings { theme: ThemeChoice::Auto, zoom: 1.0, outline: false, width: 1000, height: 900 };
    if let Some(text) = settings_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "theme" => s.theme = ThemeChoice::parse(v).unwrap_or(s.theme),
                "zoom" => s.zoom = v.parse().ok().filter(|z| (0.25..=5.0).contains(z)).unwrap_or(1.0),
                "outline" => s.outline = v == "true",
                "width" => s.width = v.parse().ok().filter(|w| (300..=10000).contains(w)).unwrap_or(s.width),
                "height" => s.height = v.parse().ok().filter(|h| (200..=10000).contains(h)).unwrap_or(s.height),
                _ => {}
            }
        }
    }
    s
}

fn save_settings(s: &Settings) {
    if let Some(p) = settings_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(
            p,
            format!(
                "theme={}\nzoom={}\noutline={}\nwidth={}\nheight={}\n",
                s.theme.as_str(),
                s.zoom,
                s.outline,
                s.width,
                s.height
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct Ui {
    window: gtk::Window,
    header: gtk::HeaderBar,
    back: gtk::Button,
    forward: gtk::Button,
    open_btn: gtk::Button,
    theme_btn: gtk::Button,
    outline_btn: gtk::ToggleButton,
    search_btn: gtk::ToggleButton,
    prev_btn: gtk::Button,
    next_btn: gtk::Button,
    tv: gtk::TextView,
    scroller: gtk::ScrolledWindow,
    outline_rev: gtk::Revealer,
    outline_list: gtk::ListBox,
    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    search_label: gtk::Label,
    toast_rev: gtk::Revealer,
    toast_label: gtk::Label,
    css: gtk::CssProvider,
}

struct State {
    settings: Settings,
    palette: Palette,
    system_dark: bool,
    doc: Option<Rendered>,
    /// The file or folder shown; `None` for the welcome page.
    current: Option<PathBuf>,
    page_margin: i32,
    last_width: i32,
    back: Vec<(Option<PathBuf>, Option<usize>)>,
    forward: Vec<(Option<PathBuf>, Option<usize>)>,
    hits: Vec<(i32, i32)>,
    hit: usize,
    hover_code: Option<usize>,
    hover_link: bool,
    watcher: Option<notify::RecommendedWatcher>,
    watched_dir: Option<PathBuf>,
    reload_pending: bool,
    resize_pending: bool,
    toast_gen: u32,
    syncing_outline: bool,
}

struct App {
    ui: Ui,
    st: RefCell<State>,
}

fn s(zoom: f64, px: f64) -> i32 {
    (px * zoom).round() as i32
}

fn css_family(list: &str) -> String {
    list.split(',')
        .map(|f| f.trim())
        .map(|f| if f.contains('-') && !f.contains(' ') { f.to_string() } else { format!("\"{f}\"") })
        .collect::<Vec<_>>()
        .join(", ")
}

fn absolute(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::env::current_dir().map(|d| d.join(path)).unwrap_or(path.to_path_buf()))
}

fn home_relative(p: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

fn encode_path(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn md_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\`*_[]<>|#".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn human_size(n: u64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
    }
}

/// A folder is shown as a Markdown table of its entries, followed by its README.
fn dir_markdown(dir: &Path) -> String {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let meta = std::fs::metadata(e.path()).ok();
            if meta.as_ref().is_some_and(|m| m.is_dir()) {
                dirs.push(name);
            } else {
                files.push((name, meta.map(|m| m.len()).unwrap_or(0)));
            }
        }
    }
    dirs.sort_by_key(|n| n.to_lowercase());
    files.sort_by_key(|(n, _)| n.to_lowercase());
    let title = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
    let mut md = format!("# {}\n\n`{}`\n\n| Name | Size |\n|:--|--:|\n", md_escape(&title), home_relative(dir));
    if dir.parent().is_some() {
        md.push_str("| [**..**](..) | |\n");
    }
    for d in &dirs {
        md.push_str(&format!("| **[{}/]({}/)** | |\n", md_escape(d), encode_path(d)));
    }
    for (f, size) in &files {
        md.push_str(&format!("| [{}]({}) | {} |\n", md_escape(f), encode_path(f), human_size(*size)));
    }
    let readme = files.iter().map(|(f, _)| f).find(|f| {
        let l = f.to_lowercase();
        l == "readme.md" || l == "readme.markdown" || l == "readme"
    });
    if let Some(readme) = readme {
        if let Ok(text) = std::fs::read(dir.join(readme)) {
            md.push_str("\n\n---\n\n");
            md.push_str(&String::from_utf8_lossy(&text));
        }
    }
    md
}

enum Content {
    Doc(Rendered),
    External,
    Error(String),
}

impl App {
    fn st(&self) -> std::cell::Ref<'_, State> {
        self.st.borrow()
    }

    fn st_mut(&self) -> std::cell::RefMut<'_, State> {
        self.st.borrow_mut()
    }

    fn zoom(&self) -> f64 {
        self.st().settings.zoom
    }

    fn base_dir(&self) -> PathBuf {
        match &self.st().current {
            Some(p) if p.is_dir() => p.clone(),
            Some(p) => p.parent().map(Path::to_path_buf).unwrap_or_default(),
            None => std::env::current_dir().unwrap_or_default(),
        }
    }

    // ------------------------------------------------------------ theming

    fn apply_theme(&self) {
        {
            let mut st = self.st_mut();
            let dark = match st.settings.theme {
                ThemeChoice::Auto => st.system_dark,
                ThemeChoice::Light => false,
                ThemeChoice::Dark => true,
            };
            st.palette = if dark { theme::dark() } else { theme::light() };
            if let Some(settings) = gtk::Settings::default() {
                settings.set_gtk_application_prefer_dark_theme(dark);
            }
        }
        self.load_css();
    }

    fn load_css(&self) {
        let st = self.st();
        let p = &st.palette;
        let z = st.settings.zoom;
        let data = format!(
            r#"
textview.md {{ font-family: {sans}; font-size: {pt:.2}pt; }}
textview.md, textview.md text {{ background-color: transparent; background-image: none; color: {fg}; }}
textview.md text selection {{ background-color: {sel}; color: {fg}; }}
.md-page {{ background-color: {canvas}; }}
.md-outline, .md-outline list, .md-outline viewport {{ background-color: {canvas}; }}
.md-outline {{ border-left: 1px solid {border_muted}; }}
.md-outline row {{ padding: 4px 10px; margin: 1px 8px; border-radius: 6px; color: {fg_muted}; background-color: transparent; }}
.md-outline row:hover {{ background-color: {subtle}; color: {fg}; }}
.md-outline row:selected {{ background-color: {code}; color: {fg}; box-shadow: inset 2px 0 {accent}; }}
.md-outline-title {{ font-weight: bold; color: {fg}; padding: 14px 18px 8px 18px; }}
.md-toast {{ background-color: {fg}; color: {canvas}; border-radius: 6px; padding: 6px 14px; margin-bottom: 24px; }}
.md-search-count {{ color: {fg_muted}; margin: 0 8px; }}
"#,
            sans = css_family(doc::sans_font()),
            pt = 12.0 * z,
            fg = css(p.fg),
            fg_muted = css(p.fg_muted),
            sel = css(p.selection),
            canvas = css(p.canvas),
            subtle = css(p.canvas_subtle),
            code = css(p.code_inline),
            border_muted = css(p.border_muted),
            accent = css(p.accent),
        );
        if let Err(e) = self.ui.css.load_from_data(data.as_bytes()) {
            eprintln!("mdview: css: {e}");
        }
        let tv = &self.ui.tv;
        tv.set_pixels_inside_wrap(s(z, 5.0));
        tv.set_top_margin(s(z, 28.0));
        tv.set_bottom_margin(s(z, 64.0));
    }

    // ------------------------------------------------------------ loading

    fn build(&self, path: Option<&Path>) -> Content {
        let tv = &self.ui.tv;
        let st = self.st();
        let zoom = st.settings.zoom;
        let pal = &st.palette;
        let cw = {
            let w = tv.allocated_width();
            let w = if w > 50 { w } else { st.settings.width };
            (w - 2 * page_margin(w, zoom)).max(100)
        };
        let Some(path) = path else {
            let cwd = std::env::current_dir().unwrap_or_default();
            return Content::Doc(Renderer::new(tv, pal, zoom, &cwd, cw).markdown(WELCOME_MD));
        };
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => return Content::Error(format!("{}: {e}", path.display())),
        };
        if meta.is_dir() {
            return Content::Doc(Renderer::new(tv, pal, zoom, path, cw).markdown(&dir_markdown(path)));
        }
        let base = path.parent().unwrap_or(Path::new("/"));
        if doc::is_markdown(path) {
            return match std::fs::read(path) {
                Ok(bytes) => Content::Doc(Renderer::new(tv, pal, zoom, base, cw).markdown(&String::from_utf8_lossy(&bytes))),
                Err(e) => Content::Error(format!("{}: {e}", path.display())),
            };
        }
        if meta.len() <= MAX_TEXT_FILE {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    return Content::Doc(Renderer::new(tv, pal, zoom, base, cw).source(&name, text, ext));
                }
            }
        }
        Content::External
    }

    /// Opens `path` (or the welcome page). `restore` is a source line to scroll to.
    fn load(self: &Rc<Self>, path: Option<PathBuf>, push_history: bool, restore: Option<usize>, fragment: Option<String>) {
        let path = path.map(|p| absolute(&p));
        let t0 = std::time::Instant::now();
        let content = self.build(path.as_deref());
        if std::env::var_os("MDVIEW_TIMING").is_some() {
            eprintln!("mdview: rendered in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
        }
        match content {
            Content::External => {
                if let Some(p) = &path {
                    self.open_external_file(p);
                }
            }
            Content::Error(msg) => self.toast(&msg),
            Content::Doc(doc) => {
                if push_history {
                    let top = self.top_line();
                    let mut st = self.st_mut();
                    let cur = st.current.clone();
                    st.back.push((cur, top));
                    st.forward.clear();
                }
                self.st_mut().current = path.clone();
                self.set_doc(doc);
                self.watch(path.as_deref());
                self.update_chrome();
                if let Some(frag) = fragment {
                    self.scroll_to_anchor(&frag);
                } else if let Some(line) = restore {
                    self.scroll_to_line(line);
                }
            }
        }
    }

    /// Re-renders the current document, keeping the scroll position.
    fn rerender(self: &Rc<Self>) {
        let top = self.top_line();
        let path = self.st().current.clone();
        if let Content::Doc(doc) = self.build(path.as_deref()) {
            let query = self.ui.search_entry.text().to_string();
            self.set_doc(doc);
            if let Some(line) = top {
                self.scroll_to_line(line);
            }
            if self.ui.search_bar.is_search_mode() && !query.is_empty() {
                self.search(&query, false);
            }
        }
    }

    fn set_doc(self: &Rc<Self>, doc: Rendered) {
        let tv = &self.ui.tv;
        tv.set_buffer(Some(&doc.buffer));
        {
            let mut st = self.st_mut();
            st.doc = Some(doc);
            st.hits.clear();
            st.hover_code = None;
            st.page_margin = -1;
        }
        self.update_margins();
        self.rebuild_outline();
        self.ui.search_label.set_text("");
        let mut start = self.ui.tv.buffer().map(|b| b.start_iter());
        if let (Some(buf), Some(it)) = (tv.buffer(), start.as_mut()) {
            buf.place_cursor(it);
            if let Some(mark) = buf.create_mark(None, it, true) {
                tv.scroll_to_mark(&mark, 0.0, true, 0.0, 0.0);
            }
        }
    }

    fn update_chrome(&self) {
        let st = self.st();
        let (title, subtitle) = match &st.current {
            None => ("mdview".to_string(), String::new()),
            Some(p) => (
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into()),
                p.parent().map(home_relative).unwrap_or_default(),
            ),
        };
        self.ui.header.set_title(Some(&title));
        self.ui.header.set_subtitle(if subtitle.is_empty() { None } else { Some(&subtitle) });
        self.ui.window.set_title(&if st.current.is_some() { format!("{title} — mdview") } else { title });
        self.ui.back.set_sensitive(!st.back.is_empty());
        self.ui.forward.set_sensitive(!st.forward.is_empty());
    }

    fn update_margins(self: &Rc<Self>) {
        let w = self.ui.tv.allocated_width();
        if w <= 50 {
            return;
        }
        let mut st = self.st_mut();
        let pm = page_margin(w, st.settings.zoom);
        let width_changed = (w - st.last_width).abs() > 24;
        if width_changed {
            st.last_width = w;
        }
        if pm != st.page_margin {
            st.page_margin = pm;
            self.ui.tv.set_left_margin(pm);
            self.ui.tv.set_right_margin(pm);
            if let Some(doc) = &st.doc {
                for (tag, indent) in &doc.margin_tags {
                    tag.set_left_margin(pm + indent);
                }
                for (tag, extra) in &doc.right_tags {
                    tag.set_right_margin(pm + extra);
                }
            }
        }
        let sensitive = st.doc.as_ref().is_some_and(|d| d.width_sensitive);
        if width_changed && sensitive && !st.resize_pending {
            st.resize_pending = true;
            let app = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(200), move || {
                app.st_mut().resize_pending = false;
                app.rerender();
            });
        }
    }

    // ------------------------------------------------------------ scrolling

    fn top_line(&self) -> Option<usize> {
        let st = self.st();
        let doc = st.doc.as_ref()?;
        let vis = self.ui.tv.visible_rect();
        let (it, _) = self.ui.tv.line_at_y(vis.y() + 4);
        doc.line_for_offset(it.offset())
    }

    fn scroll_to_offset(&self, offset: i32) {
        let Some(buf) = self.ui.tv.buffer() else { return };
        let it = buf.iter_at_offset(offset);
        if let Some(mark) = buf.create_mark(None, &it, true) {
            self.ui.tv.scroll_to_mark(&mark, 0.0, true, 0.0, 0.0);
        }
    }

    fn scroll_to_line(&self, line: usize) {
        let off = self.st().doc.as_ref().and_then(|d| d.offset_for_line(line));
        if let Some(off) = off {
            self.scroll_to_offset(off);
        }
    }

    fn scroll_to_anchor(&self, anchor: &str) -> bool {
        let anchor = doc::percent_decode(anchor);
        let off = {
            let st = self.st();
            let Some(d) = st.doc.as_ref() else { return false };
            d.anchors
                .get(&anchor)
                .or_else(|| d.anchors.get(&anchor.to_lowercase()))
                .or_else(|| anchor.strip_prefix("user-content-").and_then(|a| d.anchors.get(a)))
                .copied()
        };
        match off {
            Some(off) => {
                self.scroll_to_offset(off);
                true
            }
            None => false,
        }
    }

    // ------------------------------------------------------------ outline

    fn rebuild_outline(self: &Rc<Self>) {
        let list = &self.ui.outline_list;
        for child in list.children() {
            list.remove(&child);
        }
        let st = self.st();
        let Some(doc) = st.doc.as_ref() else { return };
        let mut any = false;
        for h in doc.headings.iter().filter(|h| h.level <= 4) {
            let label = gtk::Label::new(Some(h.text.trim()));
            label.set_xalign(0.0);
            label.set_ellipsize(pango::EllipsizeMode::End);
            label.set_margin_start(((h.level as i32 - 1) * 14).max(0));
            if h.level == 1 {
                let attrs = pango::AttrList::new();
                attrs.insert(pango::AttrInt::new_weight(pango::Weight::Semibold));
                label.set_attributes(Some(&attrs));
            }
            label.set_tooltip_text(Some(h.text.trim()));
            list.add(&label);
            any = true;
        }
        if !any {
            let label = gtk::Label::new(Some("No headings"));
            label.set_xalign(0.0);
            label.set_sensitive(false);
            list.add(&label);
        }
        list.show_all();
        drop(st);
        self.sync_outline();
    }

    fn outline_offsets(&self) -> Vec<i32> {
        self.st().doc.as_ref().map(|d| d.headings.iter().filter(|h| h.level <= 4).map(|h| h.offset).collect()).unwrap_or_default()
    }

    fn sync_outline(&self) {
        if !self.ui.outline_rev.reveals_child() {
            return;
        }
        let offsets = self.outline_offsets();
        let Some(buf) = self.ui.tv.buffer() else { return };
        let y = self.ui.tv.visible_rect().y() + s(self.zoom(), 60.0);
        let mut idx = None;
        for (i, off) in offsets.iter().enumerate() {
            let (ly, _) = self.ui.tv.line_yrange(&buf.iter_at_offset(*off));
            if ly <= y {
                idx = Some(i);
            } else {
                break;
            }
        }
        // At the very bottom, the last heading that is on screen is the current one.
        let adj = self.ui.scroller.vadjustment();
        if adj.value() + adj.page_size() >= adj.upper() - 2.0 {
            let bottom = self.ui.tv.visible_rect().y() + self.ui.tv.visible_rect().height();
            for (i, off) in offsets.iter().enumerate().skip(idx.map_or(0, |i| i + 1)) {
                let (ly, _) = self.ui.tv.line_yrange(&buf.iter_at_offset(*off));
                if ly < bottom {
                    idx = Some(i);
                }
            }
        }
        let idx = idx.or(if offsets.is_empty() { None } else { Some(0) });
        self.st_mut().syncing_outline = true;
        match idx.and_then(|i| self.ui.outline_list.row_at_index(i as i32)) {
            Some(row) => self.ui.outline_list.select_row(Some(&row)),
            None => self.ui.outline_list.unselect_all(),
        }
        self.st_mut().syncing_outline = false;
    }

    fn set_outline(self: &Rc<Self>, open: bool) {
        self.ui.outline_rev.set_reveal_child(open);
        if self.ui.outline_btn.is_active() != open {
            self.ui.outline_btn.set_active(open);
        }
        let changed = {
            let mut st = self.st_mut();
            let changed = st.settings.outline != open;
            st.settings.outline = open;
            changed
        };
        if changed {
            save_settings(&self.st().settings);
        }
        self.sync_outline();
    }

    // ------------------------------------------------------------ search

    fn search(&self, query: &str, scroll: bool) {
        let Some(buf) = self.ui.tv.buffer() else { return };
        let (start, end) = buf.bounds();
        buf.remove_tag_by_name("hit", &start, &end);
        buf.remove_tag_by_name("hit-current", &start, &end);
        let mut hits = Vec::new();
        if !query.is_empty() {
            let Some(table) = buf.tag_table() else { return };
            let n = table.size();
            for name in ["hit", "hit-current"] {
                if let Some(t) = table.lookup(name) {
                    t.set_priority(n - 1);
                }
            }
            let mut it = buf.start_iter();
            while let Some((a, b)) = it.forward_search(query, gtk::TextSearchFlags::CASE_INSENSITIVE | gtk::TextSearchFlags::TEXT_ONLY, None) {
                buf.apply_tag_by_name("hit", &a, &b);
                hits.push((a.offset(), b.offset()));
                it = b;
                if hits.len() >= 5000 {
                    break;
                }
            }
        }
        let top = {
            let vis = self.ui.tv.visible_rect();
            self.ui.tv.line_at_y(vis.y()).0.offset()
        };
        let first = hits.iter().position(|(a, _)| *a >= top).unwrap_or(0);
        let empty = hits.is_empty();
        {
            let mut st = self.st_mut();
            st.hits = hits;
            st.hit = first;
        }
        if empty {
            self.ui.search_label.set_text(if query.is_empty() { "" } else { "No results" });
        } else {
            self.go_hit(first, scroll);
        }
    }

    fn go_hit(&self, i: usize, scroll: bool) {
        let Some(buf) = self.ui.tv.buffer() else { return };
        let (hits_len, (a, b), prev) = {
            let st = self.st();
            if st.hits.is_empty() {
                return;
            }
            let i = i % st.hits.len();
            (st.hits.len(), st.hits[i], st.hits.get(st.hit).copied())
        };
        if let Some((pa, pb)) = prev {
            buf.remove_tag_by_name("hit-current", &buf.iter_at_offset(pa), &buf.iter_at_offset(pb));
        }
        let i = i % hits_len;
        self.st_mut().hit = i;
        let (sa, sb) = (buf.iter_at_offset(a), buf.iter_at_offset(b));
        buf.apply_tag_by_name("hit-current", &sa, &sb);
        self.ui.search_label.set_text(&format!("{} of {}{}", i + 1, hits_len, if hits_len >= 5000 { "+" } else { "" }));
        if scroll {
            if let Some(mark) = buf.create_mark(None, &sa, true) {
                self.ui.tv.scroll_to_mark(&mark, 0.1, true, 0.0, 0.35);
            }
        }
    }

    fn step_hit(&self, delta: i64) {
        let (len, cur) = {
            let st = self.st();
            (st.hits.len(), st.hit)
        };
        if len == 0 {
            let q = self.ui.search_entry.text().to_string();
            self.search(&q, true);
            return;
        }
        let next = (cur as i64 + delta).rem_euclid(len as i64) as usize;
        self.go_hit(next, true);
    }

    fn set_search(&self, on: bool) {
        self.ui.search_bar.set_search_mode(on);
        if on {
            self.ui.search_entry.grab_focus();
            self.ui.search_entry.select_region(0, -1);
        }
    }

    // ------------------------------------------------------------ actions

    fn toast(self: &Rc<Self>, text: &str) {
        self.ui.toast_label.set_text(text);
        self.ui.toast_rev.set_reveal_child(true);
        let generation = {
            let mut st = self.st_mut();
            st.toast_gen += 1;
            st.toast_gen
        };
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(1400), move || {
            if app.st().toast_gen == generation {
                app.ui.toast_rev.set_reveal_child(false);
            }
        });
    }

    fn set_zoom(self: &Rc<Self>, dir: i32) {
        let z = {
            let mut st = self.st_mut();
            let cur = st.settings.zoom;
            st.settings.zoom = match dir {
                0 => 1.0,
                d if d > 0 => ZOOM_STEPS.iter().copied().find(|&z| z > cur + 0.001).unwrap_or(cur),
                _ => ZOOM_STEPS.iter().rev().copied().find(|&z| z < cur - 0.001).unwrap_or(cur),
            };
            save_settings(&st.settings);
            st.settings.zoom
        };
        self.load_css();
        self.rerender();
        self.toast(&format!("Zoom {}%", (z * 100.0).round()));
    }

    fn cycle_theme(self: &Rc<Self>) {
        let t = {
            let mut st = self.st_mut();
            st.settings.theme = st.settings.theme.next();
            save_settings(&st.settings);
            st.settings.theme
        };
        self.apply_theme();
        self.rerender();
        self.toast(match t {
            ThemeChoice::Auto => "Theme: follow system",
            ThemeChoice::Light => "Theme: light",
            ThemeChoice::Dark => "Theme: dark",
        });
    }

    fn open_dialog(self: &Rc<Self>) {
        let dialog = gtk::FileChooserNative::new(
            Some("Open Markdown File"),
            Some(&self.ui.window),
            gtk::FileChooserAction::Open,
            Some("_Open"),
            Some("_Cancel"),
        );
        let md = gtk::FileFilter::new();
        md.set_name(Some("Markdown"));
        for ext in ["md", "markdown", "mdown", "mkd", "mkdn", "mdwn", "txt"] {
            md.add_pattern(&format!("*.{ext}"));
            md.add_pattern(&format!("*.{}", ext.to_uppercase()));
        }
        dialog.add_filter(md);
        let all = gtk::FileFilter::new();
        all.set_name(Some("All files"));
        all.add_pattern("*");
        dialog.add_filter(all);
        let _ = dialog.set_current_folder(self.base_dir());
        let result = if dialog.run() == gtk::ResponseType::Accept { dialog.filename() } else { None };
        dialog.destroy();
        if let Some(p) = result {
            self.load(Some(p), true, None, None);
        }
    }

    fn go_back(self: &Rc<Self>) {
        let entry = self.st_mut().back.pop();
        if let Some((path, line)) = entry {
            let top = self.top_line();
            let cur = self.st().current.clone();
            self.st_mut().forward.push((cur, top));
            self.load(path, false, line, None);
        }
    }

    fn go_forward(self: &Rc<Self>) {
        let entry = self.st_mut().forward.pop();
        if let Some((path, line)) = entry {
            let top = self.top_line();
            let cur = self.st().current.clone();
            self.st_mut().back.push((cur, top));
            self.load(path, false, line, None);
        }
    }

    fn open_external_uri(&self, uri: &str) {
        if let Err(e) = gtk::show_uri_on_window(Some(&self.ui.window), uri, gtk::current_event_time()) {
            eprintln!("mdview: cannot open {uri}: {e}");
        }
    }

    fn open_external_file(&self, path: &Path) {
        if let Ok(uri) = glib::filename_to_uri(path, None) {
            self.open_external_uri(&uri);
        }
    }

    fn follow_link(self: &Rc<Self>, target: &str) {
        let target = target.trim();
        if target == "mdview:open" {
            self.open_dialog();
            return;
        }
        if let Some(anchor) = target.strip_prefix('#') {
            if !self.scroll_to_anchor(anchor) {
                self.toast("Heading not found");
            }
            return;
        }
        let lower = target.to_ascii_lowercase();
        if ["http://", "https://", "mailto:", "ftp://"].iter().any(|p| lower.starts_with(p)) {
            self.toast("Opening in browser…");
            self.open_external_uri(target);
            return;
        }
        let (path_part, fragment) = match target.split_once('#') {
            Some((p, f)) => (p, Some(f.to_string())),
            None => (target, None),
        };
        let path_part = path_part.split('?').next().unwrap_or("");
        let decoded = doc::percent_decode(path_part.strip_prefix("file://").unwrap_or(path_part));
        if decoded.is_empty() {
            return;
        }
        let p = Path::new(&decoded);
        let full = if p.is_absolute() { p.to_path_buf() } else { self.base_dir().join(p) };
        if !full.exists() {
            self.toast(&format!("Not found: {}", home_relative(&full)));
            return;
        }
        self.load(Some(full), true, None, fragment);
    }

    fn copy_text(self: &Rc<Self>, text: &str) {
        let clipboard = gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD);
        clipboard.set_text(text);
        self.toast("Copied");
    }

    // ------------------------------------------------------------ live reload

    fn watch(&self, path: Option<&Path>) {
        let dir = path.map(|p| if p.is_dir() { p.to_path_buf() } else { p.parent().map(Path::to_path_buf).unwrap_or_default() });
        let mut st = self.st_mut();
        if dir == st.watched_dir {
            return;
        }
        let old = st.watched_dir.take();
        if let Some(w) = st.watcher.as_mut() {
            if let Some(old) = &old {
                let _ = w.unwatch(old);
            }
            if let Some(new) = &dir {
                if let Err(e) = w.watch(new, RecursiveMode::NonRecursive) {
                    eprintln!("mdview: cannot watch {}: {e}", new.display());
                }
            }
        }
        st.watched_dir = dir;
    }

    fn on_fs_change(self: &Rc<Self>, paths: Vec<PathBuf>) {
        let relevant = {
            let st = self.st();
            match &st.current {
                Some(cur) if cur.is_dir() => true,
                Some(cur) => paths.iter().any(|p| p.file_name() == cur.file_name()),
                None => false,
            }
        };
        if !relevant || self.st().reload_pending {
            return;
        }
        self.st_mut().reload_pending = true;
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(80), move || {
            app.st_mut().reload_pending = false;
            let exists = app.st().current.as_ref().is_some_and(|p| p.exists());
            if exists {
                app.rerender();
            }
        });
    }
}

fn page_margin(width: i32, zoom: f64) -> i32 {
    let max_content = s(zoom, 900.0);
    ((width - max_content) / 2).max(s(zoom.min(1.25), 28.0))
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const HELP: &str = "\
mdview — a fast Markdown viewer

USAGE:
    mdview [OPTIONS] [PATH]

PATH may be a Markdown file, any text file, or a folder (shows a listing and its README).

OPTIONS:
    -t, --theme <auto|light|dark>   Color theme (remembered between runs)
    -h, --help                      Show this help
    -V, --version                   Show version

KEYS:
    Ctrl+O open · Ctrl+F or / find · Ctrl+R reload · Ctrl+T theme · F9 outline
    Ctrl +/-/0 zoom · Alt+←/→ back/forward · Ctrl+Q quit
";

struct Args {
    path: Option<PathBuf>,
    theme: Option<ThemeChoice>,
}

fn parse_args() -> Args {
    let mut args = Args { path: None, theme: None };
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                print!("{HELP}");
                std::process::exit(0);
            }
            Some("-V" | "--version") => {
                println!("mdview {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Some("-t" | "--theme") => {
                let v = it.next().and_then(|v| v.into_string().ok()).unwrap_or_default();
                args.theme = Some(ThemeChoice::parse(&v).unwrap_or_else(|| {
                    eprintln!("mdview: invalid theme '{v}' (expected auto, light or dark)");
                    std::process::exit(2);
                }));
            }
            Some(s) if s.starts_with("--theme=") => args.theme = ThemeChoice::parse(&s[8..]),
            Some(s) if s.starts_with('-') && s.len() > 1 => {
                eprintln!("mdview: unknown option '{s}'\n\n{HELP}");
                std::process::exit(2);
            }
            _ => args.path = Some(PathBuf::from(arg)),
        }
    }
    args
}

// ---------------------------------------------------------------------------
// UI construction
// ---------------------------------------------------------------------------

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(Some(icon), gtk::IconSize::Button);
    b.set_tooltip_text(Some(tooltip));
    b.set_focus_on_click(false);
    b
}

fn icon_toggle(icon: &str, tooltip: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::new();
    b.set_image(Some(&gtk::Image::from_icon_name(Some(icon), gtk::IconSize::Button)));
    b.set_tooltip_text(Some(tooltip));
    b.set_focus_on_click(false);
    b
}

fn build_ui(settings: &Settings) -> Ui {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_default_size(settings.width, settings.height);
    window.set_title("mdview");
    window.set_icon_name(Some("text-x-generic"));

    let header = gtk::HeaderBar::new();
    header.set_show_close_button(true);
    header.set_title(Some("mdview"));
    let back = icon_button("go-previous-symbolic", "Back (Alt+Left)");
    let forward = icon_button("go-next-symbolic", "Forward (Alt+Right)");
    let nav = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    nav.style_context().add_class("linked");
    nav.add(&back);
    nav.add(&forward);
    header.pack_start(&nav);
    let open = icon_button("document-open-symbolic", "Open (Ctrl+O)");
    header.pack_start(&open);
    let outline_btn = icon_toggle("view-list-symbolic", "Outline (F9)");
    let theme_btn = icon_button("display-brightness-symbolic", "Theme: system / light / dark (Ctrl+T)");
    let search_btn = icon_toggle("edit-find-symbolic", "Find (Ctrl+F)");
    header.pack_end(&outline_btn);
    header.pack_end(&theme_btn);
    header.pack_end(&search_btn);
    window.set_titlebar(Some(&header));

    let tv = gtk::TextView::new();
    tv.set_editable(false);
    tv.set_cursor_visible(false);
    tv.set_wrap_mode(gtk::WrapMode::WordChar);
    tv.style_context().add_class("md");

    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.style_context().add_class("md-page");
    scroller.add(&tv);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);

    let search_entry = gtk::SearchEntry::new();
    search_entry.set_width_chars(36);
    let search_label = gtk::Label::new(None);
    search_label.style_context().add_class("md-search-count");
    let prev_btn = icon_button("go-up-symbolic", "Previous (Shift+Enter)");
    let next_btn = icon_button("go-down-symbolic", "Next (Enter)");
    let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    search_box.add(&search_entry);
    search_box.add(&search_label);
    let arrows = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    arrows.style_context().add_class("linked");
    arrows.add(&prev_btn);
    arrows.add(&next_btn);
    search_box.add(&arrows);
    let search_bar = gtk::SearchBar::new();
    search_bar.add(&search_box);
    search_bar.connect_entry(&search_entry);
    search_bar.set_show_close_button(true);

    let outline_list = gtk::ListBox::new();
    outline_list.set_selection_mode(gtk::SelectionMode::Single);
    let outline_title = gtk::Label::new(Some("Outline"));
    outline_title.set_xalign(0.0);
    outline_title.style_context().add_class("md-outline-title");
    let outline_scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    outline_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    outline_scroll.add(&outline_list);
    outline_scroll.set_vexpand(true);
    let outline_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outline_box.style_context().add_class("md-outline");
    outline_box.set_size_request(260, -1);
    outline_box.add(&outline_title);
    outline_box.add(&outline_scroll);
    let outline_rev = gtk::Revealer::new();
    outline_rev.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
    outline_rev.set_transition_duration(150);
    outline_rev.add(&outline_box);

    let toast_label = gtk::Label::new(None);
    toast_label.style_context().add_class("md-toast");
    let toast_rev = gtk::Revealer::new();
    toast_rev.set_transition_type(gtk::RevealerTransitionType::Crossfade);
    toast_rev.set_halign(gtk::Align::Center);
    toast_rev.set_valign(gtk::Align::End);
    toast_rev.add(&toast_label);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add(&search_bar);
    content.add(&scroller);
    let overlay = gtk::Overlay::new();
    overlay.add(&content);
    overlay.add_overlay(&toast_rev);
    overlay.set_overlay_pass_through(&toast_rev, true);
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.add(&overlay);
    overlay.set_hexpand(true);
    body.add(&outline_rev);
    window.add(&body);

    let css = gtk::CssProvider::new();
    if let Some(screen) = gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(&screen, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    Ui {
        window,
        header,
        back,
        forward,
        open_btn: open,
        theme_btn,
        outline_btn,
        search_btn,
        prev_btn,
        next_btn,
        tv,
        scroller,
        outline_rev,
        outline_list,
        search_bar,
        search_entry,
        search_label,
        toast_rev,
        toast_label,
        css,
    }
}

fn system_prefers_dark() -> bool {
    if std::env::var("GTK_THEME").is_ok_and(|t| t.to_lowercase().contains("dark")) {
        return true;
    }
    gtk::Settings::default().is_some_and(|s| {
        s.is_gtk_application_prefer_dark_theme()
            || s.gtk_theme_name().is_some_and(|n| n.to_lowercase().contains("dark"))
    })
}

fn connect_signals(app: &Rc<App>) {
    let ui = &app.ui;

    ui.window.connect_delete_event(glib::clone!(@weak app => @default-return glib::Propagation::Proceed, move |w, _| {
        let (width, height) = w.size();
        let mut st = app.st_mut();
        st.settings.width = width;
        st.settings.height = height;
        save_settings(&st.settings);
        glib::Propagation::Proceed
    }));
    ui.window.connect_destroy(|_| gtk::main_quit());

    ui.back.connect_clicked(glib::clone!(@weak app => move |_| app.go_back()));
    ui.forward.connect_clicked(glib::clone!(@weak app => move |_| app.go_forward()));
    ui.open_btn.connect_clicked(glib::clone!(@weak app => move |_| app.open_dialog()));
    ui.theme_btn.connect_clicked(glib::clone!(@weak app => move |_| app.cycle_theme()));
    ui.prev_btn.connect_clicked(glib::clone!(@weak app => move |_| app.step_hit(-1)));
    ui.next_btn.connect_clicked(glib::clone!(@weak app => move |_| app.step_hit(1)));
    ui.outline_btn.connect_toggled(glib::clone!(@weak app => move |b| app.set_outline(b.is_active())));
    ui.search_btn.connect_toggled(glib::clone!(@weak app => move |b| {
        if b.is_active() != app.ui.search_bar.is_search_mode() {
            app.set_search(b.is_active());
        }
    }));

    // search
    ui.search_bar.connect_search_mode_enabled_notify(glib::clone!(@weak app => move |bar| {
        let on = bar.is_search_mode();
        if app.ui.search_btn.is_active() != on {
            app.ui.search_btn.set_active(on);
        }
        if !on {
            app.search("", false);
            app.ui.tv.grab_focus();
        }
    }));
    ui.search_entry.connect_search_changed(glib::clone!(@weak app => move |e| app.search(&e.text(), true)));
    ui.search_entry.connect_activate(glib::clone!(@weak app => move |_| app.step_hit(1)));
    ui.search_entry.connect_next_match(glib::clone!(@weak app => move |_| app.step_hit(1)));
    ui.search_entry.connect_previous_match(glib::clone!(@weak app => move |_| app.step_hit(-1)));
    ui.search_entry.connect_key_press_event(glib::clone!(@weak app => @default-return glib::Propagation::Proceed, move |_, ev| {
        let k = ev.keyval();
        if (k == gdk::keys::constants::Return || k == gdk::keys::constants::KP_Enter) && ev.state().contains(gdk::ModifierType::SHIFT_MASK) {
            app.step_hit(-1);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }));

    // outline
    ui.outline_list.connect_row_activated(glib::clone!(@weak app => move |_, row| {
        let offsets = app.outline_offsets();
        if let Some(off) = offsets.get(row.index() as usize) {
            app.scroll_to_offset(*off);
        }
    }));
    ui.scroller.vadjustment().connect_value_changed(glib::clone!(@weak app => move |_| {
        if !app.st().syncing_outline {
            app.sync_outline();
        }
    }));

    // drawing
    let tv = &ui.tv;
    tv.connect_draw(glib::clone!(@weak app => @default-return glib::Propagation::Proceed, move |tv, cr| {
        let Some(win) = TextViewExt::window(tv, gtk::TextWindowType::Text) else { return glib::Propagation::Proceed };
        if !gtk::cairo_should_draw_window(cr, &win) {
            return glib::Propagation::Proceed;
        }
        let Ok(st) = app.st.try_borrow() else { return glib::Propagation::Proceed };
        let _ = cr.save();
        gtk::cairo_transform_to_window(cr, tv, &win);
        let c = st.palette.canvas;
        cr.set_source_rgb(((c >> 16) & 0xff) as f64 / 255.0, ((c >> 8) & 0xff) as f64 / 255.0, (c & 0xff) as f64 / 255.0);
        let _ = cr.paint();
        if let Some(doc) = &st.doc {
            let g = paint::Geometry { page_margin: st.page_margin.max(0), zoom: st.settings.zoom };
            paint::paint(tv, cr, doc, &st.palette, &g, st.hover_code);
        }
        let _ = cr.restore();
        glib::Propagation::Proceed
    }));
    tv.connect_size_allocate(glib::clone!(@weak app => move |_, _| app.update_margins()));

    // links, copy buttons, cursor
    tv.connect_motion_notify_event(glib::clone!(@weak app => @default-return glib::Propagation::Proceed, move |tv, ev| {
        let (x, y) = ev.position();
        let (bx, by) = tv.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let (link, code) = {
            let Ok(st) = app.st.try_borrow() else { return glib::Propagation::Proceed };
            let Some(doc) = &st.doc else { return glib::Propagation::Proceed };
            let link = tv.iter_at_location(bx, by).and_then(|it| {
                let r = tv.iter_location(&it);
                (bx >= r.x() - 2 && bx <= r.x() + r.width() + 2 && by >= r.y() && by <= r.y() + r.height())
                    .then(|| doc.link_at(it.offset()).map(|l| l.target.clone()))
                    .flatten()
            });
            let g = paint::Geometry { page_margin: st.page_margin.max(0), zoom: st.settings.zoom };
            let right = tv.allocated_width() - st.page_margin;
            let code = doc.decos.iter().position(|d| {
                matches!(d.kind, DecoKind::CodeBlock { .. }) && {
                    let (t, b) = paint::deco_span(tv, doc, d);
                    let x0 = g.page_margin + d.indent;
                    by >= t && by <= b && bx >= x0 && bx <= right
                }
            });
            (link, code)
        };
        let has_link = link.is_some();
        tv.set_tooltip_text(link.as_deref());
        let mut st = app.st_mut();
        if has_link != st.hover_link {
            st.hover_link = has_link;
            if let Some(win) = TextViewExt::window(tv, gtk::TextWindowType::Text) {
                let cursor = gdk::Cursor::from_name(&tv.display(), if has_link { "pointer" } else { "text" });
                win.set_cursor(cursor.as_ref());
            }
        }
        if code != st.hover_code {
            st.hover_code = code;
            tv.queue_draw();
        }
        glib::Propagation::Proceed
    }));
    tv.connect_leave_notify_event(glib::clone!(@weak app => @default-return glib::Propagation::Proceed, move |tv, _| {
        let mut st = app.st_mut();
        if st.hover_code.take().is_some() {
            tv.queue_draw();
        }
        glib::Propagation::Proceed
    }));
    tv.connect_button_release_event(glib::clone!(@weak app => @default-return glib::Propagation::Proceed, move |tv, ev| {
        if ev.button() != 1 {
            if ev.button() == 8 {
                app.go_back();
            } else if ev.button() == 9 {
                app.go_forward();
            }
            return glib::Propagation::Proceed;
        }
        if tv.buffer().is_some_and(|b| b.has_selection()) {
            return glib::Propagation::Proceed;
        }
        let (x, y) = ev.position();
        let (bx, by) = tv.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        enum Action { Copy(String), Follow(String) }
        let action = {
            let Ok(st) = app.st.try_borrow() else { return glib::Propagation::Proceed };
            let Some(doc) = &st.doc else { return glib::Propagation::Proceed };
            let g = paint::Geometry { page_margin: st.page_margin.max(0), zoom: st.settings.zoom };
            let copy = st.hover_code.and_then(|i| doc.decos.get(i)).and_then(|d| {
                let (cx, cy, size) = paint::copy_button(tv, doc, &g, d);
                let (fx, fy) = (bx as f64, by as f64);
                match &d.kind {
                    DecoKind::CodeBlock { code } if fx >= cx && fx <= cx + size && fy >= cy && fy <= cy + size => Some(Action::Copy(code.clone())),
                    _ => None,
                }
            });
            copy.or_else(|| {
                tv.iter_at_location(bx, by).and_then(|it| {
                    let r = tv.iter_location(&it);
                    (bx >= r.x() - 2 && bx <= r.x() + r.width() + 2 && by >= r.y() && by <= r.y() + r.height())
                        .then(|| doc.link_at(it.offset()).map(|l| Action::Follow(l.target.clone())))
                        .flatten()
                })
            })
        };
        match action {
            Some(Action::Copy(code)) => app.copy_text(&code),
            Some(Action::Follow(target)) => {
                // Defer so GTK finishes handling this click before the buffer is replaced.
                glib::idle_add_local_once(glib::clone!(@weak app => move || app.follow_link(&target)));
            }
            None => {}
        }
        glib::Propagation::Proceed
    }));

    // drag and drop
    tv.drag_dest_set(
        gtk::DestDefaults::ALL,
        &[gtk::TargetEntry::new("text/uri-list", gtk::TargetFlags::OTHER_APP, 0)],
        gdk::DragAction::COPY,
    );
    tv.connect_drag_data_received(glib::clone!(@weak app => move |tv, _, _, _, data, _, _| {
        tv.stop_signal_emission_by_name("drag-data-received");
        if let Some(path) = data.uris().first().and_then(|u| glib::filename_from_uri(u).ok()).map(|(p, _)| p) {
            app.load(Some(path), true, None, None);
        }
    }));

    // keyboard
    ui.window.connect_key_press_event(glib::clone!(@weak app => @default-return glib::Propagation::Proceed, move |_, ev| {
        use gdk::keys::constants as k;
        let state = ev.state();
        let ctrl = state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::META_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let alt = state.contains(gdk::ModifierType::MOD1_MASK);
        let key = ev.keyval();
        let lower = key.to_lower();
        let in_entry = app.ui.search_entry.has_focus();
        let handled = if ctrl && shift && lower == k::o {
            app.set_outline(!app.ui.outline_rev.reveals_child());
            true
        } else if ctrl && !alt {
            if lower == k::o { app.open_dialog(); true }
            else if lower == k::f { app.set_search(true); true }
            else if lower == k::r { app.rerender(); app.toast("Reloaded"); true }
            else if lower == k::t { app.cycle_theme(); true }
            else if lower == k::q || lower == k::w { app.ui.window.close(); true }
            else if key == k::plus || key == k::equal || key == k::KP_Add { app.set_zoom(1); true }
            else if key == k::minus || key == k::underscore || key == k::KP_Subtract { app.set_zoom(-1); true }
            else if key == k::_0 || key == k::KP_0 { app.set_zoom(0); true }
            else if lower == k::g { app.step_hit(if shift { -1 } else { 1 }); true }
            else if key == k::bracketleft { app.go_back(); true }
            else if key == k::bracketright { app.go_forward(); true }
            else { false }
        } else if alt && key == k::Left { app.go_back(); true }
        else if alt && key == k::Right { app.go_forward(); true }
        else if key == k::F5 { app.rerender(); app.toast("Reloaded"); true }
        else if key == k::F9 { app.set_outline(!app.ui.outline_rev.reveals_child()); true }
        else if key == k::F3 { app.step_hit(if shift { -1 } else { 1 }); true }
        else if !in_entry && key == k::slash { app.set_search(true); true }
        else if !in_entry && key == k::BackSpace { app.go_back(); true }
        else if key == k::Escape && app.ui.search_bar.is_search_mode() { app.set_search(false); true }
        else { false };
        if handled { glib::Propagation::Stop } else { glib::Propagation::Proceed }
    }));
}

fn main() {
    let start = std::time::Instant::now();
    let args = parse_args();
    if gtk::init().is_err() {
        eprintln!("mdview: cannot open a display (is DISPLAY set? try ssh -X / -Y)");
        std::process::exit(1);
    }

    // Load syntax definitions while the window comes up.
    std::thread::spawn(|| {
        doc::syntaxes();
    });

    let mut settings = load_settings();
    if let Some(t) = args.theme {
        settings.theme = t;
        save_settings(&settings);
    }
    let system_dark = system_prefers_dark();
    let outline = settings.outline;
    let ui = build_ui(&settings);
    doc::init_fonts(&ui.tv.pango_context());
    let app = Rc::new(App {
        ui,
        st: RefCell::new(State {
            settings,
            palette: theme::light(),
            system_dark,
            doc: None,
            current: None,
            page_margin: -1,
            last_width: 0,
            back: Vec::new(),
            forward: Vec::new(),
            hits: Vec::new(),
            hit: 0,
            hover_code: None,
            hover_link: false,
            watcher: None,
            watched_dir: None,
            reload_pending: false,
            resize_pending: false,
            toast_gen: 0,
            syncing_outline: false,
        }),
    });
    app.apply_theme();
    connect_signals(&app);

    // File watching: notify's thread → GTK main loop.
    #[allow(deprecated)]
    let (tx, rx) = glib::MainContext::channel::<Vec<PathBuf>>(glib::Priority::DEFAULT);
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if !matches!(ev.kind, notify::EventKind::Access(_)) {
                let _ = tx.send(ev.paths);
            }
        }
    });
    match watcher {
        Ok(w) => app.st_mut().watcher = Some(w),
        Err(e) => eprintln!("mdview: live reload unavailable: {e}"),
    }
    #[allow(deprecated)]
    rx.attach(None, glib::clone!(@weak app => @default-return glib::ControlFlow::Break, move |paths| {
        app.on_fs_change(paths);
        glib::ControlFlow::Continue
    }));

    app.ui.window.show_all();
    app.ui.outline_rev.set_reveal_child(false);
    app.load(args.path, false, None, None);
    if outline {
        app.set_outline(true);
    }
    app.ui.tv.grab_focus();
    if std::env::var_os("MDVIEW_TIMING").is_some() {
        app.ui.window.connect_draw(move |_, _| {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| eprintln!("mdview: first frame after {:.1} ms", start.elapsed().as_secs_f64() * 1000.0));
            glib::Propagation::Proceed
        });
    }
    gtk::main();
}
