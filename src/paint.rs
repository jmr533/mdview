//! Paints the document decorations underneath the text view's text.

use std::f64::consts::PI;

use comrak::nodes::AlertType;
use gtk::cairo::Context;
use gtk::prelude::*;
use gtk::TextView;

use crate::doc::{Deco, DecoKind, Rendered};
use crate::theme::Palette;

pub struct Geometry {
    pub page_margin: i32,
    pub zoom: f64,
}

impl Geometry {
    fn s(&self, px: f64) -> f64 {
        px * self.zoom
    }
}

fn set(cr: &Context, hex: u32) {
    cr.set_source_rgb(
        ((hex >> 16) & 0xff) as f64 / 255.0,
        ((hex >> 8) & 0xff) as f64 / 255.0,
        (hex & 0xff) as f64 / 255.0,
    );
}

fn rounded(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

/// Vertical extent (buffer coords) of a decoration.
pub fn deco_span(tv: &TextView, doc: &Rendered, d: &Deco) -> (i32, i32) {
    let buf = &doc.buffer;
    let (y0, _) = tv.line_yrange(&buf.iter_at_offset(d.start));
    let (y1, h1) = tv.line_yrange(&buf.iter_at_offset(d.end));
    (y0 + d.top_inset, y1 + h1)
}

/// Horizontal extent (buffer coords) of block decorations.
fn deco_x(tv: &TextView, g: &Geometry, d: &Deco) -> (f64, f64) {
    let x = (g.page_margin + d.indent) as f64;
    let right = (tv.allocated_width() - g.page_margin) as f64;
    (x, right.max(x + 1.0))
}

/// The copy button of a code block, in buffer coords: (x, y, size).
pub fn copy_button(tv: &TextView, doc: &Rendered, g: &Geometry, d: &Deco) -> (f64, f64, f64) {
    let (top, _) = deco_span(tv, doc, d);
    let (_, right) = deco_x(tv, g, d);
    let size = g.s(28.0);
    (right - g.s(8.0) - size, top as f64 + g.s(8.0), size)
}

pub fn paint(tv: &TextView, cr: &Context, doc: &Rendered, pal: &Palette, g: &Geometry, hover_code: Option<usize>) {
    let vis = tv.visible_rect();
    let (vy0, vy1) = (vis.y(), vis.y() + vis.height());
    cr.translate(-vis.x() as f64, -vis.y() as f64);

    for (i, d) in doc.decos.iter().enumerate() {
        let (top, bottom) = deco_span(tv, doc, d);
        if bottom < vy0 || top > vy1 {
            continue;
        }
        let (x, right) = deco_x(tv, g, d);
        let (top, bottom) = (top as f64, bottom as f64);
        match &d.kind {
            DecoKind::CodeBlock { .. } => {
                set(cr, pal.canvas_subtle);
                rounded(cr, x, top, right - x, bottom - top, g.s(6.0));
                let _ = cr.fill();
                if hover_code == Some(i) {
                    let (bx, by, size) = copy_button(tv, doc, g, d);
                    set(cr, pal.canvas);
                    rounded(cr, bx, by, size, size, g.s(6.0));
                    let _ = cr.fill_preserve();
                    set(cr, pal.border);
                    cr.set_line_width(1.0);
                    let _ = cr.stroke();
                    copy_icon(cr, bx + size / 2.0, by + size / 2.0, g.s(1.0), pal.fg_muted);
                }
            }
            DecoKind::Bar { color } => {
                set(cr, *color);
                cr.rectangle(x, top, g.s(4.0), bottom - top);
                let _ = cr.fill();
            }
            DecoKind::HeadingRule => {
                set(cr, pal.border_muted);
                cr.rectangle(x, bottom - 1.0, right - x, 1.0);
                let _ = cr.fill();
            }
            DecoKind::Rule => {
                set(cr, pal.border);
                cr.rectangle(x, top, right - x, g.s(4.0));
                let _ = cr.fill();
            }
            DecoKind::ThinRule => {
                set(cr, pal.border);
                cr.rectangle(x, top, right - x, 1.0);
                let _ = cr.fill();
            }
            DecoKind::Table { cols, rows, header } => {
                let w = *cols.last().unwrap_or(&0) as f64;
                let buf = &doc.buffer;
                let spans: Vec<(f64, f64)> = rows
                    .iter()
                    .enumerate()
                    .map(|(r, &off)| {
                        let (y, h) = tv.line_yrange(&buf.iter_at_offset(off));
                        let inset = if r == 0 { d.top_inset } else { 0 };
                        ((y + inset) as f64, (y + h) as f64)
                    })
                    .collect();
                // zebra stripes on every second body row
                set(cr, pal.canvas_subtle);
                for (r, (t, b)) in spans.iter().enumerate() {
                    let body_index = if *header { r as i32 - 1 } else { r as i32 };
                    if body_index >= 0 && body_index % 2 == 1 {
                        cr.rectangle(x, *t, w, b - t);
                    }
                }
                let _ = cr.fill();
                set(cr, pal.border);
                cr.set_line_width(1.0);
                let (t0, b1) = (spans.first().map(|s| s.0).unwrap_or(top), spans.last().map(|s| s.1).unwrap_or(bottom));
                for (t, _) in &spans {
                    cr.move_to(x, t.floor() + 0.5);
                    cr.line_to(x + w, t.floor() + 0.5);
                }
                cr.move_to(x, b1.floor() - 0.5);
                cr.line_to(x + w, b1.floor() - 0.5);
                for c in cols {
                    let cx = (x + *c as f64).floor() + 0.5;
                    let cx = if *c as f64 >= w { cx - 1.0 } else { cx };
                    cr.move_to(cx, t0);
                    cr.line_to(cx, b1);
                }
                let _ = cr.stroke();
            }
            DecoKind::Check { checked } => {
                let r = tv.iter_location(&doc.buffer.iter_at_offset(d.start));
                let size = g.s(14.0).round();
                let bx = r.x() as f64;
                let by = (r.y() as f64 + (r.height() as f64 - size) / 2.0).round();
                rounded(cr, bx + 0.5, by + 0.5, size - 1.0, size - 1.0, g.s(3.0));
                if *checked {
                    set(cr, pal.accent);
                    let _ = cr.fill();
                    set(cr, if pal.dark { pal.canvas } else { 0xffffff });
                    cr.set_line_width(g.s(1.8));
                    cr.move_to(bx + size * 0.25, by + size * 0.52);
                    cr.line_to(bx + size * 0.43, by + size * 0.70);
                    cr.line_to(bx + size * 0.76, by + size * 0.32);
                    let _ = cr.stroke();
                } else {
                    set(cr, pal.canvas);
                    let _ = cr.fill_preserve();
                    set(cr, pal.fg_muted);
                    cr.set_line_width(1.0);
                    let _ = cr.stroke();
                }
            }
            DecoKind::AlertIcon { kind, color } => {
                let r = tv.iter_location(&doc.buffer.iter_at_offset(d.start));
                let size = g.s(16.0);
                let cx = r.x() as f64 + size / 2.0;
                let cy = r.y() as f64 + r.height() as f64 / 2.0;
                alert_icon(cr, *kind, cx, cy, size, *color);
            }
        }
    }
}

fn copy_icon(cr: &Context, cx: f64, cy: f64, s: f64, color: u32) {
    set(cr, color);
    cr.set_line_width(1.2 * s);
    let w = 8.0 * s;
    rounded(cr, cx - 5.0 * s, cy - 2.0 * s, w, w, 1.5 * s);
    let _ = cr.stroke();
    cr.move_to(cx - 2.0 * s, cy - 4.0 * s);
    cr.line_to(cx - 2.0 * s, cy - 5.0 * s);
    cr.arc(cx - 0.5 * s, cy - 5.0 * s, 1.5 * s, PI, 1.5 * PI);
    cr.line_to(cx + 3.5 * s, cy - 6.5 * s);
    cr.arc(cx + 3.5 * s, cy - 5.0 * s, 1.5 * s, 1.5 * PI, 2.0 * PI);
    cr.line_to(cx + 5.0 * s, cy + 1.0 * s);
    cr.arc(cx + 3.5 * s, cy + 1.0 * s, 1.5 * s, 0.0, 0.5 * PI);
    cr.line_to(cx + 3.0 * s, cy + 2.5 * s);
    let _ = cr.stroke();
}

/// Small line icons approximating GitHub's octicons for alerts.
fn alert_icon(cr: &Context, kind: AlertType, cx: f64, cy: f64, size: f64, color: u32) {
    let s = size / 16.0;
    set(cr, color);
    cr.set_line_width(1.5 * s);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    let dot = |cr: &Context, x: f64, y: f64| {
        cr.arc(x, y, 1.0 * s, 0.0, 2.0 * PI);
        let _ = cr.fill();
    };
    match kind {
        AlertType::Note => {
            cr.arc(cx, cy, 7.0 * s, 0.0, 2.0 * PI);
            let _ = cr.stroke();
            cr.move_to(cx, cy - 0.5 * s);
            cr.line_to(cx, cy + 3.5 * s);
            let _ = cr.stroke();
            dot(cr, cx, cy - 3.5 * s);
        }
        AlertType::Tip => {
            cr.arc(cx, cy - 2.0 * s, 5.0 * s, 0.75 * PI, 2.25 * PI);
            cr.line_to(cx + 2.0 * s, cy + 4.0 * s);
            cr.line_to(cx - 2.0 * s, cy + 4.0 * s);
            cr.close_path();
            let _ = cr.stroke();
            cr.move_to(cx - 2.0 * s, cy + 6.5 * s);
            cr.line_to(cx + 2.0 * s, cy + 6.5 * s);
            let _ = cr.stroke();
        }
        AlertType::Important => {
            rounded(cr, cx - 7.0 * s, cy - 7.0 * s, 14.0 * s, 11.0 * s, 1.5 * s);
            let _ = cr.stroke();
            cr.move_to(cx - 3.5 * s, cy + 4.0 * s);
            cr.line_to(cx - 4.5 * s, cy + 7.0 * s);
            cr.line_to(cx - 0.5 * s, cy + 4.0 * s);
            let _ = cr.stroke();
            cr.move_to(cx, cy - 4.0 * s);
            cr.line_to(cx, cy - 0.5 * s);
            let _ = cr.stroke();
            dot(cr, cx, cy + 1.8 * s);
        }
        AlertType::Warning => {
            cr.move_to(cx, cy - 7.0 * s);
            cr.line_to(cx + 7.5 * s, cy + 6.5 * s);
            cr.line_to(cx - 7.5 * s, cy + 6.5 * s);
            cr.close_path();
            let _ = cr.stroke();
            cr.move_to(cx, cy - 2.5 * s);
            cr.line_to(cx, cy + 1.5 * s);
            let _ = cr.stroke();
            dot(cr, cx, cy + 4.0 * s);
        }
        AlertType::Caution => {
            let r = 7.2 * s;
            for k in 0..8 {
                let a = PI / 8.0 + k as f64 * PI / 4.0;
                let (px, py) = (cx + r * a.cos(), cy + r * a.sin());
                if k == 0 {
                    cr.move_to(px, py);
                } else {
                    cr.line_to(px, py);
                }
            }
            cr.close_path();
            let _ = cr.stroke();
            cr.move_to(cx, cy - 3.5 * s);
            cr.line_to(cx, cy + 0.5 * s);
            let _ = cr.stroke();
            dot(cr, cx, cy + 3.0 * s);
        }
    }
}
