//! The host page's screen watermark (web `pdfcraft:watermark`): one diagonal text over each page,
//! drawn like WebViewer's `setWatermark({ diagonal })`. It lives on the screen only; nothing here
//! reaches the document or a saved file.
//!
//! The text is turned into glyph outlines here and handed to the renderer, which fills them into
//! the page picture between the page's contents and its annotations, so the annotations lie over
//! the watermark as in WebViewer (and a highlight still multiplies with it).

use std::sync::Arc;

use kurbo::{BezPath, Point};
use pdfcraft_render::ScreenWatermark;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, MetadataProvider as _};

/// The longest text the host may set, in characters.
pub const MAX_TEXT_CHARS: usize = 200;
/// WebViewer draws `48pt` on a canvas in page points, and a canvas `pt` is 4/3 px: 64 page units.
const FONT_UNITS: f32 = 48.0 * 4.0 / 3.0;
/// The line height is `1.5 * fontSize` with the 48 taken as pixels, as WebViewer's.
const LINE_UNITS: f64 = 1.5 * 48.0;
/// `rgba(120, 120, 120, 0.3)`.
const RGB: [f32; 3] = [120.0 / 255.0; 3];
const ALPHA: f32 = 0.3;

/// Largest and smallest font size the host may ask for, in points.
pub const MIN_FONT_SIZE: f64 = 8.0;
pub const MAX_FONT_SIZE: f64 = 400.0;

/// How the host wants the watermark drawn (`pdfcraft:watermark`'s `style`). Missing fields keep the
/// defaults, which are WebViewer's `setWatermark({ diagonal })` look: 48 pt, `rgba(120, 120, 120, 0.3)`, along the page's diagonal.
#[derive(Clone, Debug, PartialEq)]
pub struct WatermarkStyle {
    /// Points.
    pub font_size: f64,
    /// Channels 0..1.
    pub rgb: [f32; 3],
    /// 0..1.
    pub opacity: f32,
    /// Degrees clockwise as shown, or `None` for the page's own diagonal.
    pub rotation: Option<f32>,
}

impl Default for WatermarkStyle {
    fn default() -> Self {
        Self { font_size: 48.0, rgb: RGB, opacity: ALPHA, rotation: None }
    }
}

impl WatermarkStyle {
    /// Read the host's `style` object. A value of the wrong type or outside its range is an error, never ignored:
    /// the host would see a watermark that isn't the one it asked for.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let obj = v.as_object().ok_or("style must be an object")?;
        let mut style = Self::default();
        for (key, value) in obj {
            match key.as_str() {
                "fontSize" => {
                    let n = value.as_f64().filter(|n| (MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(n));
                    style.font_size = n.ok_or_else(|| format!("fontSize must be a number from {MIN_FONT_SIZE} to {MAX_FONT_SIZE}"))?;
                }
                "color" => style.rgb = parse_rgb(value.as_str().ok_or("color must be a string")?)?,
                "opacity" => {
                    let n = value.as_f64().filter(|n| (0.0..=1.0).contains(n)).ok_or("opacity must be a number from 0 to 1")?;
                    style.opacity = n as f32;
                }
                "rotation" => {
                    style.rotation = match value {
                        serde_json::Value::String(s) if s == "diagonal" => None,
                        other => Some(
                            other
                                .as_f64()
                                .filter(|n| (-360.0..=360.0).contains(n))
                                .ok_or("rotation must be a number from -360 to 360 or \"diagonal\"")? as f32,
                        ),
                    }
                }
                other => return Err(format!("unknown style field {other}")),
            }
        }
        Ok(style)
    }

    /// The renderer's identity for the watermark: the plain text for the default look (what a document that
    /// already draws it compares against), the text with the look otherwise.
    fn key(&self, text: &str) -> String {
        if *self == Self::default() {
            text.to_string()
        } else {
            format!("{text}\u{1}{}|{:?}|{}|{:?}", self.font_size, self.rgb, self.opacity, self.rotation)
        }
    }
}

/// `#rgb` or `#rrggbb`.
fn parse_rgb(text: &str) -> Result<[f32; 3], String> {
    let bad = || "color must be #rgb or #rrggbb".to_string();
    let hex = text.strip_prefix('#').ok_or_else(bad)?;
    if !hex.is_ascii() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let channel = |i: usize, len: usize| u8::from_str_radix(&hex[i..i + len], 16).map(|v| if len == 1 { v * 17 } else { v }).map_err(|_| bad());
    let (r, g, b) = match hex.len() {
        3 => (channel(0, 1)?, channel(1, 1)?, channel(2, 1)?),
        6 => (channel(0, 2)?, channel(2, 2)?, channel(4, 2)?),
        _ => return Err(bad()),
    };
    Ok([r, g, b].map(|c| f32::from(c) / 255.0))
}

/// Clean a text from the host: at most [`MAX_TEXT_CHARS`] characters, none for blank. `None` clears.
pub(crate) fn clean(text: Option<&str>) -> Option<String> {
    text.filter(|t| !t.trim().is_empty()).map(|t| t.chars().take(MAX_TEXT_CHARS).collect())
}

/// Appends a glyph's outline to a path, moved to `origin` (y down) from skrifa's y-up em units.
struct Pen<'a> {
    path: &'a mut BezPath,
    x: f64,
    baseline: f64,
}

impl Pen<'_> {
    fn at(&self, x: f32, y: f32) -> Point {
        Point::new(self.x + f64::from(x), self.baseline - f64::from(y))
    }
}

impl OutlinePen for Pen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.at(x, y);
        self.path.move_to(p);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.at(x, y);
        self.path.line_to(p);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let (c, p) = (self.at(cx0, cy0), self.at(x, y));
        self.path.quad_to(c, p);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (c0, c1, p) = (self.at(cx0, cy0), self.at(cx1, cy1), self.at(x, y));
        self.path.curve_to(c0, c1, p);
    }
    fn close(&mut self) {
        self.path.close_path();
    }
}

/// The interface's faces in the order egui looks a character up in them.
fn faces(defs: &egui::FontDefinitions) -> Vec<FontRef<'_>> {
    let names = defs.families.get(&egui::FontFamily::Proportional).map(Vec::as_slice).unwrap_or_default();
    names
        .iter()
        .filter_map(|name| defs.font_data.get(name))
        .filter_map(|data| FontRef::from_index(&data.font, data.index).ok())
        .collect()
}

/// The watermark for `text` (lines split on `\n`): each line centred on the origin with its top
/// `(n - lines / 2) * 72` points below it, upright, in the interface's fonts at 64 points.
/// `None` when no character of the text has a glyph.
pub(crate) fn build(text: &str, style: &WatermarkStyle) -> Option<Arc<ScreenWatermark>> {
    let defs = crate::theme::font_definitions();
    let faces = faces(&defs);
    // The default 48 pt is 64 page units (see FONT_UNITS); the line height follows the font size.
    let scale = style.font_size / 48.0;
    let size = Size::new(FONT_UNITS * scale as f32);
    let line_units = LINE_UNITS * scale;
    let ascent = f64::from(faces.first()?.metrics(size, LocationRef::default()).ascent);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut outline = BezPath::new();
    let mut missing = 0usize;
    for (n, line) in lines.iter().enumerate() {
        // Each character from the first face that has it, laid out left to right.
        let mut glyphs = Vec::new();
        let mut width = 0.0f64;
        for c in line.chars() {
            let Some((face, gid)) = faces.iter().find_map(|f| f.charmap().map(c).map(|g| (f, g))) else {
                // Whitespace and controls have no ink; anything else is a glyph the fonts lack.
                missing += usize::from(!c.is_whitespace() && !c.is_control());
                continue;
            };
            let advance = f64::from(face.glyph_metrics(size, LocationRef::default()).advance_width(gid).unwrap_or(0.0));
            glyphs.push((face, gid, width));
            width += advance;
        }
        let top = (n as f64 - lines.len() as f64 / 2.0) * line_units;
        for (face, gid, x) in glyphs {
            let Some(glyph) = face.outline_glyphs().get(gid) else { continue };
            let mut pen = Pen { path: &mut outline, x: x - width / 2.0, baseline: top + ascent };
            // A glyph that fails to draw leaves at most a partial shape; nothing else is lost.
            let _ = glyph.draw(DrawSettings::unhinted(size, LocationRef::default()), &mut pen);
        }
    }
    if missing > 0 {
        log::warn!("screen watermark: {missing} character(s) have no glyph in the interface fonts and are left out");
    }
    if outline.elements().is_empty() {
        log::warn!("screen watermark: no character of the text has a glyph in the interface fonts; nothing is drawn");
    }
    (!outline.elements().is_empty())
        .then(|| Arc::new(ScreenWatermark { key: style.key(text), outline, rgb: style.rgb, alpha: style.opacity, rotation: style.rotation }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurbo::Shape;

    #[test]
    fn blank_or_missing_text_clears_and_long_text_is_cut() {
        assert_eq!(clean(None), None);
        assert_eq!(clean(Some("  \n ")), None);
        assert_eq!(clean(Some("기밀")), Some("기밀".to_string()));
        assert_eq!(clean(Some(&"가".repeat(500))).map(|t| t.chars().count()), Some(MAX_TEXT_CHARS));
    }

    #[test]
    fn the_outline_is_centred_on_the_origin_and_lines_stack_like_webviewer() {
        let one = build("CONFIDENTIAL", &WatermarkStyle::default()).expect("latin text has glyphs").outline.bounding_box();
        assert!((one.center().x).abs() < 8.0, "{one:?}");
        // 64 points of font: capitals are about 46 points tall, so the line is not tiny or huge.
        assert!(one.height() > 30.0 && one.height() < 70.0, "{one:?}");
        let two = build("A\nA", &WatermarkStyle::default()).expect("latin text has glyphs").outline.bounding_box();
        // Two lines 72 points apart.
        assert!((two.height() - (one.height().min(70.0) + 72.0)).abs() < 40.0, "{two:?}");
        assert!(two.height() > 72.0, "{two:?}");
    }

    #[test]
    fn korean_text_has_outlines_and_an_unmappable_text_has_none() {
        // Hangul has a face only in builds that embed craft-fonts.
        if !pdfcraft_fonts::ui_korean_fonts().is_empty() {
            assert!(build("기밀 문서", &WatermarkStyle::default()).is_some());
        }
        assert!(build("\n", &WatermarkStyle::default()).is_none());
    }

    #[test]
    fn the_style_reads_its_fields_and_refuses_what_is_off() {
        use serde_json::json;
        assert_eq!(WatermarkStyle::from_json(&json!({})), Ok(WatermarkStyle::default()));
        let s = WatermarkStyle::from_json(&json!({ "fontSize": 24, "color": "#f00", "opacity": 0.5, "rotation": -30 })).unwrap();
        assert_eq!((s.font_size, s.rgb, s.opacity, s.rotation), (24.0, [1.0, 0.0, 0.0], 0.5, Some(-30.0)));
        let s = WatermarkStyle::from_json(&json!({ "color": "#336699", "rotation": "diagonal" })).unwrap();
        assert_eq!((s.rgb, s.rotation), ([0.2, 0.4, 0.6], None));
        for bad in [
            json!(null),
            json!({ "fontSize": 7.9 }),
            json!({ "fontSize": 401 }),
            json!({ "fontSize": "48" }),
            json!({ "color": "red" }),
            json!({ "color": "#ff000080" }),
            json!({ "color": "#gg0000" }),
            json!({ "opacity": 1.1 }),
            json!({ "opacity": -0.1 }),
            json!({ "rotation": 361 }),
            json!({ "rotation": "vertical" }),
            json!({ "size": 10 }),
        ] {
            assert!(WatermarkStyle::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_style_sets_size_colour_opacity_and_turn_of_the_outline() {
        let plain = build("CONFIDENTIAL", &WatermarkStyle::default()).unwrap();
        assert_eq!(plain.key, "CONFIDENTIAL", "the default look keeps the text as its key");
        assert_eq!((plain.rgb, plain.alpha, plain.rotation), ([120.0 / 255.0; 3], 0.3, None));
        let style = WatermarkStyle { font_size: 24.0, rgb: [1.0, 0.0, 0.0], opacity: 0.8, rotation: Some(90.0) };
        let small = build("CONFIDENTIAL", &style).unwrap();
        assert_ne!(small.key, plain.key, "a changed look is another watermark, so it is drawn again");
        assert_eq!((small.rgb, small.alpha, small.rotation), ([1.0, 0.0, 0.0], 0.8, Some(90.0)));
        let ratio = small.outline.bounding_box().width() / plain.outline.bounding_box().width();
        assert!((ratio - 0.5).abs() < 0.02, "24 pt is half of 48 pt: {ratio}");
        let big = build("A\nA", &WatermarkStyle { font_size: 96.0, ..WatermarkStyle::default() }).unwrap();
        let normal = build("A\nA", &WatermarkStyle::default()).unwrap();
        assert!(big.outline.bounding_box().height() > 1.9 * normal.outline.bounding_box().height(), "the lines are spaced by the size too");
    }
}
