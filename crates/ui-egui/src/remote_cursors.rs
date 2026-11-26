//! Where the other people in a shared document are: the page each one has most in view, the line
//! they look at on it and the text or comment they have selected, as the parent page sends them
//! (`pdfcraft:collab-cursors`). This side also builds the same description of its own place for
//! `pdfcraft:collab-cursor`. Drawn on the pages (a coloured marker at the page edge, the selection
//! shaded, a name tag) and on the page thumbnails (a dot per person). Draw-only: nothing here takes
//! input.
//!
//! The parent supplies the name and colour (the same ones as its participant list); this side only
//! checks and draws them. A malformed entry is skipped, never an error.
//!
//! One room holds one document, so a person's place is a page of that document and nothing names
//! the document itself.

use egui::{Align2, Color32, CornerRadius, Painter, Pos2, Rect, Stroke, pos2, vec2};
use pdfcraft_render::PageInfo;
use serde_json::{Value, json};

use crate::canvas::PageXform;
use crate::theme;

/// Most people drawn: a longer list is cut (a broken parent can't make a frame slow).
pub const MAX_CURSORS: usize = 200;
/// Most selection quadrilaterals taken from one person.
pub const MAX_QUADS: usize = 64;
/// Longest name drawn, in characters.
pub const MAX_NAME_CHARS: usize = 40;
/// The server drops a `pos` longer than 1024 characters; the quads that don't fit under this are left off.
const MAX_POS_CHARS: usize = 1000;
/// Name tags stacked on one spot before "+N".
const MAX_TAGS: usize = 4;
/// Grey for a colour that isn't `#rrggbb`.
const FALLBACK: [u8; 3] = [128, 128, 128];

/// One other connection's place in the document.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteCursor {
    pub conn: u64,
    pub name: String,
    pub color: [u8; 3],
    /// Only looking (role `view`): drawn with a dashed outline.
    pub view: bool,
    /// The page (0-based) the person has most in view.
    pub page: usize,
    /// The line looked at, from the top (0) to the bottom (1) of the page.
    pub y: Option<f32>,
    /// What is selected on that page, in user space (as `selection_quads` gives).
    pub quads: Vec<[f64; 8]>,
}

/// This editor's own place, as it goes out in `collab-cursor`.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub page: usize,
    pub y: Option<f32>,
    pub quads: Vec<[f64; 8]>,
}

fn round(v: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (v * k).round() / k
}

impl Place {
    /// The place of the page `page` seen from `y` (a fraction of the page height, held to 0..=1 and
    /// to whole percents so a slow scroll doesn't make a message for every pixel) with `quads` selected.
    pub fn new(page: usize, y: Option<f32>, quads: &[[f64; 8]]) -> Self {
        let y = y.filter(|y| y.is_finite()).map(|y| (f64::from(y).clamp(0.0, 1.0) * 100.0).round() as f32 / 100.0);
        let mut place = Place { page, y, quads: quads.iter().take(MAX_QUADS).map(|q| q.map(|v| round(v, 1))).collect() };
        // What the server would drop is cut here, from the end, instead.
        while !place.quads.is_empty() && place.pos().to_string().chars().count() > MAX_POS_CHARS {
            place.quads.pop();
        }
        place
    }

    /// `page:<n>`, n from 1.
    pub fn region(&self) -> String {
        format!("page:{}", self.page + 1)
    }

    pub fn pos(&self) -> Value {
        let mut pos = serde_json::Map::new();
        if let Some(y) = self.y {
            pos.insert("y".into(), json!(round(f64::from(y), 2)));
        }
        if !self.quads.is_empty() {
            pos.insert("sel".into(), json!({ "quads": self.quads }));
        }
        Value::Object(pos)
    }
}

/// `#rrggbb` as bytes.
fn parse_color(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(h, 16).ok()?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

/// The people in a `collab-cursors` message that have a page position (`page:<n>`), in order.
/// Entries of another region or in a shape that can't be read are left out; the rest are kept.
pub fn parse(m: &Value) -> Vec<RemoteCursor> {
    let Some(list) = m.get("cursors").and_then(Value::as_array) else { return vec![] };
    list.iter().take(MAX_CURSORS).filter_map(parse_one).collect()
}

fn parse_one(c: &Value) -> Option<RemoteCursor> {
    let page = c.get("region")?.as_str()?.strip_prefix("page:")?.parse::<usize>().ok()?.checked_sub(1)?;
    let conn = c.get("conn")?.as_u64()?;
    let name: String = c.get("name").and_then(Value::as_str).unwrap_or_default().chars().filter(|ch| !ch.is_control()).take(MAX_NAME_CHARS).collect();
    let color = c.get("color").and_then(Value::as_str).and_then(parse_color).unwrap_or(FALLBACK);
    let view = c.get("role").and_then(Value::as_str) == Some("view");
    let pos = c.get("pos");
    let y = pos.and_then(|p| p.get("y")).and_then(Value::as_f64).filter(|y| y.is_finite()).map(|y| y.clamp(0.0, 1.0) as f32);
    let quads = pos
        .and_then(|p| p.get("sel"))
        .and_then(|s| s.get("quads"))
        .and_then(Value::as_array)
        .map(|a| a.iter().take(MAX_QUADS).filter_map(parse_quad).collect())
        .unwrap_or_default();
    Some(RemoteCursor { conn, name, color, view, page, y, quads })
}

/// Eight finite numbers, or nothing.
fn parse_quad(v: &Value) -> Option<[f64; 8]> {
    let a = v.as_array().filter(|a| a.len() == 8)?;
    let mut q = [0.0; 8];
    for (slot, n) in q.iter_mut().zip(a) {
        *slot = n.as_f64().filter(|n| n.is_finite())?;
    }
    Some(q)
}

fn color(c: &RemoteCursor, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.color[0], c.color[1], c.color[2], alpha)
}

/// Black or white, whichever reads on `c`.
fn on(c: [u8; 3]) -> Color32 {
    let l = 0.299 * f32::from(c[0]) + 0.587 * f32::from(c[1]) + 0.114 * f32::from(c[2]);
    if l > 150.0 { Color32::BLACK } else { Color32::WHITE }
}

/// The people who have page `page` in view.
pub fn on_page(all: &[RemoteCursor], page: usize) -> impl Iterator<Item = &RemoteCursor> {
    all.iter().filter(move |c| c.page == page)
}

/// A quadrilateral in user space as the rectangle it covers in view space (points from the page's
/// top left), or `None` for one that isn't finite or lies off the page.
pub fn quad_view_rect(p: &PageInfo, q: &[f64; 8]) -> Option<[f32; 4]> {
    if q.iter().any(|v| !v.is_finite() || v.abs() > 1.0e7) {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for i in 0..4 {
        let [x, y] = p.user_to_view(q[2 * i] as f32, q[2 * i + 1] as f32);
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
    }
    // Wholly outside the page: the other copy of the document is not this one (or this page moved).
    let off = x1 < 0.0 || y1 < 0.0 || x0 > p.width || y0 > p.height;
    (x1 > x0 && y1 > y0 && !off).then_some([x0, y0, x1, y1])
}

/// A name tag whose bottom-left corner is at `at`; `row` stacks tags that share a spot upward.
fn tag(p: &Painter, at: Pos2, c: &RemoteCursor, row: usize) {
    let text = if c.name.is_empty() { "?" } else { c.name.as_str() };
    let galley = p.layout_no_wrap(text.to_string(), theme::medium(11.0), on(c.color));
    let size = galley.size() + vec2(8.0, 4.0);
    let r = Rect::from_min_size(pos2(at.x, at.y - size.y * (row as f32 + 1.0)), size);
    p.rect_filled(r, CornerRadius::same(3), color(c, 255));
    if c.view {
        // A viewer's tag has a dashed border (no icon): it is told apart from an editor's by shape.
        let b = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
        p.extend(egui::Shape::dashed_line(&b, Stroke::new(1.0, on(c.color)), 3.0, 2.0));
    }
    p.galley(r.min + vec2(4.0, 2.0), galley, on(c.color));
}

/// What the other people have on page `page`: their selections shaded, a triangle at the page's left
/// edge on the line each looks at, and a name tag where each is (at the selection, else by the
/// triangle). Tags that share a spot stack, [`MAX_TAGS`] of them, then "+N". Draws only inside `clip`.
pub fn paint_on_page(p: &Painter, clip: Rect, xf: &PageXform, info: &PageInfo, page: usize, all: &[RemoteCursor]) {
    // Tags per spot: (the spot's rounded corner, how many are there).
    let mut spots: Vec<((i32, i32), usize)> = vec![];
    let mut put = |p: &Painter, at: Pos2, c: &RemoteCursor| {
        let key = (at.x.round() as i32, at.y.round() as i32);
        let n = match spots.iter_mut().find(|(k, _)| *k == key) {
            Some(s) => {
                s.1 += 1;
                s.1 - 1
            }
            None => {
                spots.push((key, 1));
                0
            }
        };
        if n < MAX_TAGS {
            tag(p, at, c, n);
        } else if n == MAX_TAGS {
            p.text(pos2(at.x, at.y - 16.0 * (MAX_TAGS as f32 + 1.0)), Align2::LEFT_BOTTOM, "+", theme::medium(11.0), Color32::GRAY);
        }
    };
    for c in on_page(all, page) {
        let mut anchored = false;
        for q in &c.quads {
            let Some(g) = quad_view_rect(info, q) else { continue };
            let r = xf.view_rect(g);
            if !r.intersects(clip) {
                continue;
            }
            p.rect_filled(r, CornerRadius::same(1), color(c, 56));
            if c.view {
                p.rect_stroke(r, CornerRadius::same(1), Stroke::new(1.0, color(c, 200)), egui::StrokeKind::Inside);
            }
            if !anchored {
                anchored = true;
                put(p, r.left_top(), c);
            }
        }
        // The line looked at: a triangle on the page's left edge (or at the top when no line is known).
        let at = pos2(xf.rect.left(), xf.rect.top() + c.y.unwrap_or(0.0) * xf.rect.height());
        if clip.expand2(vec2(12.0, 0.0)).contains(at) {
            let tri = vec![at, at + vec2(-9.0, -6.0), at + vec2(-9.0, 6.0)];
            p.add(egui::Shape::convex_polygon(tri, color(c, 255), Stroke::new(1.0, Color32::WHITE)));
            if !anchored {
                put(p, at + vec2(4.0, 0.0), c);
            }
        }
    }
}

/// A coloured dot per person on page `page` along the bottom-right edge of its thumbnail `img` (at
/// most [`MAX_TAGS`], then "+N"). The names, for the thumbnail's tooltip, come back.
pub fn paint_on_thumb(p: &Painter, img: Rect, all: &[RemoteCursor], page: usize) -> Option<String> {
    let here: Vec<&RemoteCursor> = on_page(all, page).collect();
    if here.is_empty() {
        return None;
    }
    for (i, c) in here.iter().take(MAX_TAGS).enumerate() {
        let at = pos2(img.right() - 8.0 - 12.0 * i as f32, img.bottom() - 8.0);
        p.circle(at, 5.0, color(c, 255), Stroke::new(1.5, Color32::WHITE));
        if c.view {
            p.circle_stroke(at, 7.0, Stroke::new(1.0, color(c, 255)));
        }
    }
    if here.len() > MAX_TAGS {
        p.text(pos2(img.right() - 8.0 - 12.0 * MAX_TAGS as f32, img.bottom() - 8.0), Align2::RIGHT_CENTER, format!("+{}", here.len() - MAX_TAGS), theme::medium(10.0), Color32::GRAY);
    }
    Some(here.iter().map(|c| if c.name.is_empty() { "?" } else { c.name.as_str() }).collect::<Vec<_>>().join(", "))
}

impl crate::PdfCraftApp {
    /// Once a frame: take the page's latest list of other people's places (kept only for a file opened
    /// with `cursors`), hand it to the tabs, and tell the page where this editor is if that changed.
    pub(crate) fn host_cursors_tick(&mut self) {
        let heard = self.cursors_inbox.lock().ok().and_then(|mut q| q.take());
        if self.host_cursors {
            if let Some(list) = heard {
                self.remote_cursors = std::sync::Arc::new(list);
            }
            for v in &mut self.views {
                if !std::sync::Arc::ptr_eq(&v.remote_cursors, &self.remote_cursors) {
                    v.remote_cursors = self.remote_cursors.clone();
                }
            }
        }
        if !self.host_cursors || self.host_cursor.is_none() {
            return;
        }
        let place = self.own_place();
        if self.host_cursor_last.as_ref() == Some(&place) {
            return;
        }
        if let Some(report) = self.host_cursor.as_mut() {
            match &place {
                Some(p) => report(Some((&p.region(), &p.pos()))),
                None => report(None),
            }
        }
        self.host_cursor_last = Some(place);
    }

    /// The page the shown document has most in view, the line at its top and the selected text or
    /// comment when that is on the same page. `None` while no document is shown.
    fn own_place(&self) -> Option<Place> {
        let view = self.active.and_then(|i| self.views.get(i))?;
        let info = &self.session.get(view.id)?.info;
        let page = view.current.min(info.pages.len().checked_sub(1)?);
        let mut quads: Vec<[f64; 8]> = vec![];
        if let Some((sel_page, q)) = view.selection_quads(info) {
            if sel_page == page {
                quads = q;
            }
        } else if let Some((ap, ai)) = view.comments.selected.filter(|(ap, _)| *ap == page)
            && let Some(a) = info.annotations.iter().find(|a| a.page == ap && a.index == ai)
        {
            quads = if a.quads.is_empty() {
                let [x0, y0, x1, y1] = a.rect;
                vec![[x0, y1, x1, y1, x0, y0, x1, y0].map(f64::from)]
            } else {
                a.quads.iter().map(|q| q.map(f64::from)).collect()
            };
        }
        Some(Place::new(page, view.view_y, &quads))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(conn: u64, region: &str, pos: Value) -> Value {
        json!({"conn": conn, "user": "u", "name": "Kim", "color": "#ff8800", "role": "edit", "region": region, "pos": pos})
    }

    fn page() -> PageInfo {
        PageInfo { width: 600.0, height: 800.0, label: "1".into(), crop: [0.0, 0.0, 600.0, 800.0], rotation: 0 }
    }

    #[test]
    fn reads_page_positions_and_skips_the_rest() {
        let q = [10.0, 700.0, 110.0, 700.0, 10.0, 680.0, 110.0, 680.0];
        let m = json!({"cursors": [
            entry(1, "page:3", json!({"y": 0.25, "sel": {"quads": [q]}})),
            entry(2, "doc", json!({})),
            entry(3, "page:x", json!({})),
            entry(4, "page:0", json!({})),
            json!({"conn": 5, "region": "page:1"}),
            json!("junk"),
        ]});
        let got = parse(&m);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].conn, got[0].page, got[0].y, got[0].quads.clone()), (1, 2, Some(0.25), vec![q]));
        assert_eq!(got[0].color, [255, 136, 0]);
        assert!(!got[0].view);
        // No position and no name: still marks the page, grey.
        assert_eq!((got[1].page, got[1].y, got[1].quads.len(), got[1].name.as_str(), got[1].color), (0, None, 0, "", FALLBACK));
    }

    #[test]
    fn guards_the_size_name_colour_and_quads() {
        let many: Vec<Value> = (0..300).map(|i| entry(i, "page:1", json!({}))).collect();
        assert_eq!(parse(&json!({"cursors": many})).len(), MAX_CURSORS);
        let quads: Vec<[f64; 8]> = (0..500).map(|i| [f64::from(i); 8]).collect();
        let long = json!({"conn": 1, "name": "x".repeat(500), "color": "red", "role": "view", "region": "page:2", "pos": {"y": 7.5, "sel": {"quads": quads}}});
        let got = parse(&json!({"cursors": [long]}));
        assert_eq!(got[0].name.chars().count(), MAX_NAME_CHARS);
        assert_eq!(got[0].color, FALLBACK);
        assert!(got[0].view);
        assert_eq!(got[0].quads.len(), MAX_QUADS);
        assert_eq!(got[0].y, Some(1.0), "a line past the page is held to its end");
        // Not a list, wrong shapes: nothing, never a panic.
        assert!(parse(&json!({"cursors": 5})).is_empty());
        assert!(parse(&json!({})).is_empty());
        let odd = json!({"conn": 1, "region": "page:2", "pos": {"y": "a", "sel": {"quads": [[1, 2], "x", [0, 0, 0, 0, 0, 0, 0, null], [1, 2, 3, 4, 5, 6, 7, 8]]}}});
        let got = parse(&json!({"cursors": [odd]}));
        assert_eq!((got[0].y, got[0].quads.clone()), (None, vec![[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]]));
        // Control characters can't ride in a name.
        let ctl = json!({"conn": 1, "name": "a\u{0}b\nc", "region": "page:2"});
        assert_eq!(parse(&json!({"cursors": [ctl]}))[0].name, "abc");
    }

    #[test]
    fn colours_parse_only_as_hex() {
        assert_eq!(parse_color("#00ff7F"), Some([0, 255, 127]));
        for bad in ["00ff7f", "#fff", "#gggggg", "#00ff7f00", ""] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_people_on_the_page_count() {
        let m = json!({"cursors": [entry(1, "page:1", json!({})), entry(2, "page:2", json!({})), entry(3, "page:1", json!({}))]});
        let all = parse(&m);
        assert_eq!(on_page(&all, 0).count(), 2);
        assert_eq!(on_page(&all, 8).count(), 0);
    }

    #[test]
    fn the_own_place_is_a_region_and_a_small_pos() {
        let q = [10.04, 700.0, 110.0, 700.0, 10.0, 680.0, 110.0, 680.0];
        let p = Place::new(4, Some(0.3333), &[q]);
        assert_eq!(p.region(), "page:5");
        assert_eq!(p.pos(), json!({"y": 0.33, "sel": {"quads": [[10.0, 700.0, 110.0, 700.0, 10.0, 680.0, 110.0, 680.0]]}}));
        assert_eq!(Place::new(0, None, &[]).pos(), json!({}), "nothing known: an empty object");
        assert_eq!(Place::new(0, Some(f32::NAN), &[]).y, None);
        assert_eq!(Place::new(0, Some(9.0), &[]).y, Some(1.0));
        // A long selection is cut to what the server keeps (1024 characters).
        let many: Vec<[f64; 8]> = (0..200).map(|i| [1234.5 + f64::from(i); 8]).collect();
        let big = Place::new(0, Some(0.5), &many);
        assert!(!big.quads.is_empty() && big.quads.len() < MAX_QUADS);
        assert!(big.pos().to_string().chars().count() <= 1024);
        // What was sent reads back as the same place.
        let back = parse(&json!({"cursors": [{"conn": 1, "region": big.region(), "pos": big.pos()}]}));
        assert_eq!((back[0].page, back[0].y, back[0].quads.clone()), (0, Some(0.5), big.quads.clone()));
    }

    #[test]
    fn a_quad_becomes_a_view_rectangle_or_nothing() {
        let p = page();
        // The text layer's rectangle, to user space and back.
        let q = [10.0, 700.0, 110.0, 700.0, 10.0, 680.0, 110.0, 680.0];
        let got = quad_view_rect(&p, &q).expect("on the page");
        assert!([10.0, 100.0, 110.0, 120.0].iter().zip(got).all(|(a, b)| (a - b).abs() < 0.01), "{got:?}");
        let rotated = PageInfo { width: 800.0, height: 600.0, crop: [0.0, 0.0, 600.0, 800.0], rotation: 90, ..page() };
        assert!(quad_view_rect(&rotated, &q).is_some());
        assert_eq!(quad_view_rect(&p, &[9000.0; 8]), None, "a point is no area and is off the page");
        assert_eq!(quad_view_rect(&p, &[f64::NAN; 8]), None);
        assert_eq!(quad_view_rect(&p, &[0.0, 5000.0, 10.0, 5000.0, 0.0, 5100.0, 10.0, 5100.0]), None, "below the page");
        assert_eq!(quad_view_rect(&p, &[1.0e300; 8]), None);
    }
}
