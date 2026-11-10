//! A stamp the embedding page hands in for one placement (web `pdfcraft:stamp-add`).
//!
//! Unlike the stamp library, an armed stamp is never stored in the settings: it carries the host's
//! signed metadata, lives until one click, and is gone when the tool is left.

use std::sync::Arc;

use crate::{PdfCraftApp, QuickTool, stamps_ui::MAX_STAMP_BYTES};

/// Longest side of an armed stamp, in PDF points (a page is at most 14400 pt).
const MAX_SIDE_PT: f64 = 5000.0;
const MAX_META_BYTES: usize = 64 << 10;
const MAX_ID_BYTES: usize = 200;
/// Longest side of a stamp without a given size, in PDF points (the engine's natural size).
const NATURAL_MAX_PT: f64 = 200.0;
/// PNG pixel limits. The server draws at most 3 px per point, so a 14400 pt page side is 43200 px
/// at the very most, but a stamp is a signature or a label: 8192 px a side and 16 Mpx in all
/// (64 MB decoded as RGBA) stop a small file that expands to gigabytes without limiting real stamps.
const MAX_SIDE_PX: u32 = 8192;
const MAX_PIXELS: u64 = 16 << 20;

/// The most bytes a PNG may have; the bridge checks it before copying the bytes into memory.
pub const MAX_PNG_BYTES: usize = MAX_STAMP_BYTES;

/// What the host asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum StampCommand {
    Add {
        id: String,
        name: String,
        bytes: Vec<u8>,
        width: f64,
        height: f64,
        meta: String,
    },
    /// A sign box (`pdfcraft:sign-box-add`): a FreeText box of [`SIGN_BOX_SIZE`] carrying `role` (and the
    /// `signer` it is assigned to, if given), put at once in the middle of the page being read and left selected.
    AddSignBox {
        id: String,
        role: String,
        signer: Option<String>,
        label: String,
    },
    /// A stamp put at given coordinates at once (`pdfcraft:stamp-place`), with no click. `x`, `y`
    /// are the stamp's top-left corner in PDF points on page `page` (0-based) as shown; `size` is
    /// width and height together, or `None` for the PNG's natural size.
    Place {
        id: String,
        page: usize,
        x: f64,
        y: f64,
        size: Option<(f64, f64)>,
        name: String,
        bytes: Vec<u8>,
        meta: String,
    },
    /// Sign in a sign box (`pdfcraft:sign-in-box`): like `Place`, but the sign box named `annot_id`
    /// (its `/NM`) is removed in the same edit and the stamp is sealed, so it can't be changed.
    SignIn {
        id: String,
        annot_id: String,
        page: usize,
        x: f64,
        y: f64,
        size: Option<(f64, f64)>,
        name: String,
        bytes: Vec<u8>,
        meta: String,
    },
    /// The screen watermark (`pdfcraft:watermark`): `None` or blank takes it away.
    Watermark {
        text: Option<String>,
    },
    Cancel {
        id: String,
    },
}

/// Width and height of a sign box on the page, in PDF points.
pub const SIGN_BOX_SIZE: (f64, f64) = (180.0, 60.0);

/// What the host is told.
#[derive(Clone, Debug, PartialEq)]
pub enum StampEvent {
    Armed { id: String },
    Placed { id: String, page: usize },
    Cancelled { id: String },
    Failed { id: String, error: String },
}

/// Requests from the host, drained every frame.
pub type StampInbox = Arc<std::sync::Mutex<Vec<StampCommand>>>;

/// Tells the embedding page how a stamp request went (web).
pub type HostStampFn = Box<dyn Fn(&StampEvent)>;

/// A validated request, waiting for the click that places it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArmedStamp {
    pub id: String,
    pub name: String,
    pub png: Arc<Vec<u8>>,
    /// Size on the page, in PDF points.
    pub width: f64,
    pub height: f64,
    pub meta: String,
    /// The document that was showing when it was armed; another one cancels it.
    pub doc: Option<pdfcraft_engine::DocId>,
}

/// Check a sign box request: bounded text, nothing else to read.
pub(crate) fn validate_sign_box(id: &str, role: &str, signer: Option<&str>, label: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err("the request id is missing or too long".into());
    }
    if role.trim().is_empty() || role.len() > MAX_ID_BYTES || label.trim().is_empty() || label.len() > MAX_META_BYTES {
        return Err("the sign box needs a role and a label of reasonable length".into());
    }
    // The signer is kept and compared as given: 1 to 200 characters (UTF-16 units, as the page
    // counts), no space at either end, none of the characters a role may not have.
    if let Some(s) = signer
        && (s.is_empty() || s.encode_utf16().count() > MAX_ID_BYTES || s != s.trim() || s.chars().any(is_bad_signer_char))
    {
        return Err("the signer must be 1 to 200 characters, without spaces at either end or control, bidi or zero-width characters".into());
    }
    Ok(())
}

/// The control, bidi and zero-width characters the bridge refuses in a role or signer (its `ROLE_BAD_CHARS`).
fn is_bad_signer_char(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

/// The sign box's rectangle in user space: [`SIGN_BOX_SIZE`] in the middle of the page as shown
/// (upright whatever the page's `/Rotate`), kept inside it when the page is smaller.
pub(crate) fn sign_box_rect(page: &pdfcraft_render::PageInfo) -> [f64; 4] {
    let (w, h) = SIGN_BOX_SIZE;
    let (x, y) = (((f64::from(page.width) - w) / 2.0).max(0.0), ((f64::from(page.height) - h) / 2.0).max(0.0));
    view_rect_to_user(page, x, y, w, h)
}

/// A rectangle given by its top-left corner and size on the page as shown (upright whatever the
/// page's `/Rotate`) as `[x0, y0, x1, y1]` in user space.
pub(crate) fn view_rect_to_user(page: &pdfcraft_render::PageInfo, x: f64, y: f64, w: f64, h: f64) -> [f64; 4] {
    let corners = [(x, y), (x + w, y + h)].map(|(x, y)| page.view_to_user(x as f32, y as f32));
    [corners[0][0].min(corners[1][0]), corners[0][1].min(corners[1][1]), corners[0][0].max(corners[1][0]), corners[0][1].max(corners[1][1])]
        .map(f64::from)
}

/// A stamp's natural size: one PNG pixel is one point, the longer side kept to 200 pt (never enlarged).
pub(crate) fn natural_size(px_w: u32, px_h: u32) -> (f64, f64) {
    let (w, h) = (f64::from(px_w), f64::from(px_h));
    let scale = (NATURAL_MAX_PT / w.max(h)).min(1.0);
    (w * scale, h * scale)
}

/// Check where a `pdfcraft:stamp-place` puts the stamp: finite numbers, the top-left corner on the page.
pub(crate) fn validate_place(page: &pdfcraft_render::PageInfo, x: f64, y: f64, size: Option<(f64, f64)>) -> Result<(), String> {
    let (w, h) = size.unwrap_or((1.0, 1.0));
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) {
        return Err("x, y, width and height must be finite numbers".into());
    }
    if x < 0.0 || y < 0.0 || x >= f64::from(page.width) || y >= f64::from(page.height) {
        return Err("the stamp must start inside the page".into());
    }
    Ok(())
}

/// Check a request before arming it: a readable PNG, a sane size, bounded text.
pub(crate) fn validate(id: &str, name: &str, bytes: Vec<u8>, width: f64, height: f64, meta: &str) -> Result<ArmedStamp, String> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err("the request id is missing or too long".into());
    }
    if bytes.len() > MAX_STAMP_BYTES {
        return Err(format!("the stamp is too large (at most {} MB)", MAX_STAMP_BYTES >> 20));
    }
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 && width <= MAX_SIDE_PT && height <= MAX_SIDE_PT) {
        return Err(format!("the stamp size must be between 0 and {MAX_SIDE_PT} pt"));
    }
    if meta.len() > MAX_META_BYTES {
        return Err("the stamp metadata is too large".into());
    }
    // Reads the header only; the engine decodes the picture when it is placed.
    let reader = png::Decoder::new(std::io::Cursor::new(&bytes)).read_info().map_err(|e| format!("not a readable PNG: {e}"))?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w > MAX_SIDE_PX || h > MAX_SIDE_PX || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(format!("the PNG is too large ({w} x {h} px)"));
    }
    let name = if name.trim().is_empty() { "Stamp".to_string() } else { name.trim().chars().take(200).collect() };
    Ok(ArmedStamp { id: id.into(), name, png: Arc::new(bytes), width, height, meta: meta.into(), doc: None })
}

impl PdfCraftApp {
    fn emit_stamp(&self, event: StampEvent) {
        if let Some(report) = &self.host_stamp {
            report(&event);
        }
    }

    /// Drop the armed stamp (if any), leave its tool and tell the host.
    pub(crate) fn disarm_stamp(&mut self) {
        if let Some(armed) = self.armed_stamp.take() {
            if self.quick_tool == QuickTool::HostStamp {
                self.quick_tool = QuickTool::Select;
            }
            self.emit_stamp(StampEvent::Cancelled { id: armed.id });
        }
    }

    /// Put a sign box in the middle of the page being read and select it (the same edit as any
    /// comment, so undo and saving are the usual). Returns the page, or why it was refused.
    fn place_sign_box(&mut self, id: &str, role: String, signer: Option<String>, label: String) -> Result<usize, String> {
        validate_sign_box(id, &role, signer.as_deref(), &label)?;
        let (index, doc) = self.active_ids().ok_or("no document is open")?;
        let state = self.session.get(doc).ok_or("no document is open")?;
        if !state.allows_annotation() {
            return Err("the document doesn't allow comments".into());
        }
        let page = self.views[index].current.min(state.info.pages.len().saturating_sub(1));
        let rect = sign_box_rect(state.info.pages.get(page).ok_or("the document has no pages")?);
        let author = self.comment_prefs.author.clone();
        if self.apply_edit(pdfcraft_engine::Edit::AddSignBox { page, rect, role, signer, label, author }) {
            self.quick_tool = QuickTool::Select;
            // The new box is selected by this program, not by the user: the page is told so.
            self.api_selected = self.views[index].comments.selected.and_then(|(p, i)| self.annot_name(doc, p, i));
            Ok(page)
        } else {
            Err(self.toast.as_ref().map(|(m, _)| m.clone()).unwrap_or_else(|| "the sign box could not be placed".to_string()))
        }
    }

    /// Stamp a PNG at the given place at once (the same edit as a click, so undo and saving are
    /// the usual). Returns the page, or why it was refused.
    #[allow(clippy::too_many_arguments)]
    fn place_stamp(
        &mut self,
        id: &str,
        page: usize,
        x: f64,
        y: f64,
        size: Option<(f64, f64)>,
        name: &str,
        bytes: Vec<u8>,
        meta: &str,
        sign_box: Option<&str>,
    ) -> Result<usize, String> {
        let (_, doc) = self.active_ids().ok_or("no document is open")?;
        let state = self.session.get(doc).ok_or("no document is open")?;
        if !state.allows_annotation() {
            return Err("the document doesn't allow comments".into());
        }
        let info = state.info.pages.get(page).ok_or("the page is out of range")?;
        // The sign box to sign in: it must exist, be a sign box and lie on the page given.
        let box_index = match sign_box {
            None => None,
            Some(annot_id) => {
                let (box_page, index) = state.annot_position(annot_id).ok_or("there is no annotation with that annotId")?;
                if state.sign_box_at(box_page, index).is_none() {
                    return Err("the annotation is not a sign box".into());
                }
                if box_page != page {
                    return Err("the sign box is on another page".into());
                }
                Some(index)
            }
        };
        // Cheap checks first: the size and the place are known before the PNG is read.
        validate_place(info, x, y, size)?;
        let (w, h) = match size {
            Some(size) => size,
            None => {
                let reader = png::Decoder::new(std::io::Cursor::new(&bytes)).read_info().map_err(|e| format!("not a readable PNG: {e}"))?;
                natural_size(reader.info().width, reader.info().height)
            }
        };
        let armed = validate(id, name, bytes, w, h, meta)?;
        let rect = view_rect_to_user(info, x, y, w, h);
        let author = self.comment_prefs.author.clone();
        let file = pdfcraft_engine::MarkFile { name: "stamp.png".into(), bytes: armed.png, page: 0 };
        let edit = match box_index {
            Some(index) => pdfcraft_engine::Edit::SignInBox { page, index, rect, name: armed.name, file, author, meta: armed.meta },
            None => pdfcraft_engine::Edit::AddCustomStamp { page, rect, name: armed.name, file, author, extra: Some(("PpmStamp".into(), armed.meta)) },
        };
        if self.apply_edit(edit) {
            Ok(page)
        } else {
            Err(self.toast.as_ref().map(|(m, _)| m.clone()).unwrap_or_else(|| "the stamp could not be placed".to_string()))
        }
    }

    /// Give every open document the watermark the host set (or none), redrawing those that differ.
    /// A document opened after the message gets it here, in the frame it appears.
    fn sync_watermark(&mut self) {
        let key = self.screen_watermark_outline.as_ref().map(|w| w.key.as_str());
        for index in 0..self.views.len() {
            let id = self.views[index].id;
            if self.session.watermark_key(id) != key && self.session.set_watermark(id, self.screen_watermark_outline.clone()) {
                self.views[index].invalidate_content();
            }
        }
        // The compare overlay is no tab but is drawn like one.
        if let Some(view) = self.compare.as_mut().and_then(|c| c.overlay_view.as_mut())
            && self.session.watermark_key(view.id) != key
            && self.session.set_watermark(view.id, self.screen_watermark_outline.clone())
        {
            view.invalidate_content();
        }
    }

    /// Once a frame: take the host's requests, report placements, and notice when the user left.
    pub(crate) fn host_stamp_tick(&mut self, ctx: &egui::Context) {
        // First, how a click ended: a command that arrived since must not be taken for it.
        let outcome = self.views.iter_mut().find_map(|v| v.host_stamp_outcome.take());
        if let Some(outcome) = outcome
            && let Some(armed) = self.armed_stamp.take()
        {
            match outcome {
                Ok(page) => self.emit_stamp(StampEvent::Placed { id: armed.id, page }),
                Err(error) => self.emit_stamp(StampEvent::Failed { id: armed.id, error }),
            }
        }
        let commands: Vec<StampCommand> = self.stamp_inbox.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for command in commands {
            match command {
                StampCommand::Add { id, name, bytes, width, height, meta } => {
                    // A new request replaces the one still waiting.
                    self.disarm_stamp();
                    let doc = self.active_ids().map(|(_, d)| d);
                    let allowed = doc.and_then(|d| self.session.get(d)).map(|d| d.allows_annotation());
                    let armed = match (doc, allowed, validate(&id, &name, bytes, width, height, &meta)) {
                        (None, _, _) => Err("no document is open".to_string()),
                        (_, Some(false), _) => Err("the document doesn't allow comments".to_string()),
                        (_, _, Err(e)) => Err(e),
                        (doc, _, Ok(a)) => Ok(ArmedStamp { doc, ..a }),
                    };
                    match armed {
                        Ok(a) => {
                            self.armed_stamp = Some(a);
                            self.quick_tool = QuickTool::HostStamp;
                            self.emit_stamp(StampEvent::Armed { id });
                        }
                        Err(error) => self.emit_stamp(StampEvent::Failed { id, error }),
                    }
                }
                StampCommand::AddSignBox { id, role, signer, label } => {
                    self.disarm_stamp();
                    match self.place_sign_box(&id, role, signer, label) {
                        Ok(page) => self.emit_stamp(StampEvent::Placed { id, page }),
                        Err(error) => self.emit_stamp(StampEvent::Failed { id, error }),
                    }
                }
                StampCommand::Place { id, page, x, y, size, name, bytes, meta } => {
                    self.disarm_stamp();
                    match self.place_stamp(&id, page, x, y, size, &name, bytes, &meta, None) {
                        Ok(page) => self.emit_stamp(StampEvent::Placed { id, page }),
                        Err(error) => self.emit_stamp(StampEvent::Failed { id, error }),
                    }
                }
                StampCommand::SignIn { id, annot_id, page, x, y, size, name, bytes, meta } => {
                    self.disarm_stamp();
                    match self.place_stamp(&id, page, x, y, size, &name, bytes, &meta, Some(&annot_id)) {
                        Ok(page) => self.emit_stamp(StampEvent::Placed { id, page }),
                        Err(error) => self.emit_stamp(StampEvent::Failed { id, error }),
                    }
                }
                StampCommand::Watermark { text } => {
                    self.screen_watermark = crate::watermark::clean(text.as_deref());
                    self.screen_watermark_outline = self.screen_watermark.as_deref().and_then(crate::watermark::build);
                }
                StampCommand::Cancel { id } => {
                    if self.armed_stamp.as_ref().is_some_and(|a| a.id == id) {
                        self.disarm_stamp();
                    }
                }
            }
        }
        self.sync_watermark();
        let Some(armed) = self.armed_stamp.as_ref() else { return };
        // The click's edit may not have been applied yet: its outcome decides, not the tool.
        let placing = self.views.iter().any(|v| matches!(v.pending_edit, Some(pdfcraft_engine::Edit::AddCustomStamp { extra: Some(_), .. })));
        if placing {
            return;
        }
        if self.dialog.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.quick_tool = QuickTool::Select;
        }
        // Escape, another tool, another tab or another document all end it.
        let doc = self.active_ids().map(|(_, d)| d);
        if self.quick_tool != QuickTool::HostStamp || doc != armed.doc {
            self.disarm_stamp();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 2, 2);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[255u8; 16]).unwrap();
        drop(w);
        out
    }

    #[test]
    fn a_good_request_is_armed_with_its_point_size() {
        let a = validate("r1", " Approved ", png_bytes(), 120.0, 40.5, "{\"sig\":\"x\"}").unwrap();
        assert_eq!((a.id.as_str(), a.name.as_str(), a.width, a.height, a.meta.as_str()), ("r1", "Approved", 120.0, 40.5, "{\"sig\":\"x\"}"));
    }

    #[test]
    fn bad_sign_box_requests_are_refused() {
        assert!(validate_sign_box("s1", "reviewer", None, "reviewer 서명란").is_ok());
        assert!(validate_sign_box("", "reviewer", None, "x").is_err(), "empty id");
        assert!(validate_sign_box("s", " ", None, "x").is_err(), "empty role");
        assert!(validate_sign_box("s", "r", None, "").is_err(), "empty label");
        assert!(validate_sign_box("s", "r", None, &"x".repeat(MAX_META_BYTES + 1)).is_err(), "huge label");
        assert!(validate_sign_box("s", "r", Some("u-kim"), "x").is_ok());
        assert!(validate_sign_box("s", "r", Some(" "), "x").is_err(), "blank signer");
        assert!(validate_sign_box("s", "r", Some(""), "x").is_err(), "empty signer");
        assert!(validate_sign_box("s", "r", Some("kim "), "x").is_err(), "space at the end");
        assert!(validate_sign_box("s", "r", Some(" kim"), "x").is_err(), "space at the start");
        assert!(validate_sign_box("s", "r", Some("\u{202e}kim"), "x").is_err(), "bidi");
        assert!(validate_sign_box("s", "r", Some("kim\u{200b}"), "x").is_err(), "zero width");
        assert!(validate_sign_box("s", "r", Some("ki\tm"), "x").is_err(), "control");
        assert!(validate_sign_box("s", "r", Some("김 철수"), "x").is_ok(), "inner space is fine");
        assert!(validate_sign_box("s", "r", Some(&"x".repeat(MAX_ID_BYTES + 1)), "x").is_err(), "long signer");
    }

    fn page(width: f32, height: f32, rotation: u16, crop: [f32; 4]) -> pdfcraft_render::PageInfo {
        pdfcraft_render::PageInfo { width, height, label: "1".into(), crop, rotation }
    }

    #[test]
    fn the_sign_box_sits_in_the_middle_of_the_page_as_shown() {
        // 200 x 300 pt: 180 x 60 in the middle, like /viewer's placeSignField.
        assert_eq!(sign_box_rect(&page(200.0, 300.0, 0, [0.0, 0.0, 200.0, 300.0])), [10.0, 120.0, 190.0, 180.0]);
        // Shown turned 90 degrees (300 x 200): upright as shown, so 60 wide and 180 tall in the file.
        assert_eq!(sign_box_rect(&page(300.0, 200.0, 90, [0.0, 0.0, 200.0, 300.0])), [70.0, 60.0, 130.0, 240.0]);
        // A page narrower than the box: the box starts at the page's edge.
        assert_eq!(sign_box_rect(&page(100.0, 100.0, 0, [0.0, 0.0, 100.0, 100.0])), [0.0, 20.0, 180.0, 80.0]);
    }

    /// Page transforms run in f32, so rectangles match to a hundredth of a point.
    pub(super) fn assert_rect(got: [f64; 4], want: [f64; 4]) {
        assert!(got.iter().zip(want).all(|(g, w)| (g - w).abs() < 0.01), "{got:?} != {want:?}");
    }

    #[test]
    fn a_placed_stamp_rect_is_its_top_left_corner_and_size_as_shown() {
        let p0 = page(200.0, 300.0, 0, [0.0, 0.0, 200.0, 300.0]);
        // Top-left (10, 20), 50 x 30 as shown: user space has y up from the bottom.
        assert_rect(view_rect_to_user(&p0, 10.0, 20.0, 50.0, 30.0), [10.0, 250.0, 60.0, 280.0]);
        // Shown turned 90 degrees (300 x 200): the same box is 30 wide and 50 tall in the file.
        let p90 = page(300.0, 200.0, 90, [0.0, 0.0, 200.0, 300.0]);
        let r = view_rect_to_user(&p90, 10.0, 20.0, 50.0, 30.0);
        assert_rect([r[2] - r[0], r[3] - r[1], 0.0, 0.0], [30.0, 50.0, 0.0, 0.0]);
        assert_rect(r, [20.0, 10.0, 50.0, 60.0]);
    }

    #[test]
    fn a_stamp_without_a_size_is_natural_and_at_most_200_pt() {
        assert_eq!(natural_size(120, 40), (120.0, 40.0), "small stays 1 px = 1 pt");
        assert_eq!(natural_size(400, 100), (200.0, 50.0), "long side cut to 200, ratio kept");
        assert_eq!(natural_size(100, 800), (25.0, 200.0));
        assert_eq!(natural_size(200, 200), (200.0, 200.0));
    }

    #[test]
    fn bad_places_are_refused() {
        let p = page(200.0, 300.0, 0, [0.0, 0.0, 200.0, 300.0]);
        assert!(validate_place(&p, 0.0, 0.0, None).is_ok());
        assert!(validate_place(&p, 199.9, 299.9, Some((50.0, 50.0))).is_ok(), "may reach past the far edge");
        assert!(validate_place(&p, -1.0, 0.0, None).is_err(), "left of the page");
        assert!(validate_place(&p, 0.0, -0.1, None).is_err(), "above the page");
        assert!(validate_place(&p, 200.0, 10.0, None).is_err(), "at the right edge");
        assert!(validate_place(&p, 10.0, 300.0, None).is_err(), "at the bottom edge");
        assert!(validate_place(&p, f64::NAN, 0.0, None).is_err());
        assert!(validate_place(&p, 0.0, f64::INFINITY, None).is_err());
        assert!(validate_place(&p, 0.0, 0.0, Some((f64::NAN, 5.0))).is_err());
        assert!(validate_place(&p, 0.0, 0.0, Some((5.0, f64::INFINITY))).is_err());
        // Turned pages are checked against the page as shown (300 wide, 200 tall).
        let turned = page(300.0, 200.0, 90, [0.0, 0.0, 200.0, 300.0]);
        assert!(validate_place(&turned, 250.0, 150.0, None).is_ok());
        assert!(validate_place(&turned, 150.0, 250.0, None).is_err());
    }

    #[test]
    fn bad_requests_are_refused() {
        let good = png_bytes();
        assert!(validate("", "n", good.clone(), 10.0, 10.0, "{}").is_err(), "empty id");
        assert!(validate("r", "n", b"not a png".to_vec(), 10.0, 10.0, "{}").is_err(), "not a PNG");
        assert!(validate("r", "n", good.clone(), 0.0, 10.0, "{}").is_err(), "zero width");
        assert!(validate("r", "n", good.clone(), f64::NAN, 10.0, "{}").is_err(), "NaN width");
        assert!(validate("r", "n", good.clone(), 10.0, MAX_SIDE_PT + 1.0, "{}").is_err(), "too tall");
        assert!(validate("r", "n", good.clone(), 10.0, 10.0, &"x".repeat(MAX_META_BYTES + 1)).is_err(), "huge meta");
        assert!(validate("r", "n", vec![0; MAX_STAMP_BYTES + 1], 10.0, 10.0, "{}").is_err(), "huge file");
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod flow_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use egui_kittest::Harness;

    use super::tests::assert_rect;

    use super::*;

    fn pdf() -> Vec<u8> {
        pdf_pages(1)
    }

    fn pdf_pages(n: usize) -> Vec<u8> {
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + i)).collect();
        let mut objects = vec!["<< /Type /Catalog /Pages 2 0 R >>".to_string(), format!("<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 200 300] >>", kids.join(" "))];
        objects.extend((0..n).map(|_| "<< /Type /Page /Parent 2 0 R >>".to_string()));
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
        out
    }

    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 2, 2);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[255u8; 16]).unwrap();
        drop(w);
        out
    }

    fn add(id: &str, bytes: Vec<u8>) -> StampCommand {
        StampCommand::Add { id: id.into(), name: "Approved".into(), bytes, width: 90.0, height: 30.0, meta: "{}".into() }
    }

    fn app(with_doc: bool) -> (Harness<'static, PdfCraftApp>, Rc<RefCell<Vec<StampEvent>>>) {
        let events: Rc<RefCell<Vec<StampEvent>>> = Rc::default();
        let sink = events.clone();
        let mut harness = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(move |_| {
            let mut app = PdfCraftApp::new();
            if with_doc {
                app.open_bytes("a.pdf", None, pdf()).unwrap();
            }
            app.host_stamp = Some(Box::new(move |e| sink.borrow_mut().push(e.clone())));
            // A host-lock build shows nothing and opens nothing until a host takes saves.
            app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
            app
        });
        harness.run_steps(3);
        (harness, events)
    }

    fn send(h: &mut Harness<'static, PdfCraftApp>, c: StampCommand) {
        h.state().stamp_inbox.lock().unwrap().push(c);
        h.run_steps(2);
    }

    #[test]
    fn arm_replace_cancel_and_place_are_reported() {
        let (mut h, ev) = app(true);
        send(&mut h, add("a", png()));
        assert_eq!(h.state().quick_tool, QuickTool::HostStamp);
        send(&mut h, add("b", png()));
        send(&mut h, StampCommand::Cancel { id: "a".into() });
        assert_eq!(h.state().quick_tool, QuickTool::HostStamp, "cancelling a replaced id changes nothing");
        // The edit was accepted: the view notes the page, and the canvas went back to selecting.
        h.state_mut().views[0].host_stamp_outcome = Some(Ok(0));
        h.state_mut().quick_tool = QuickTool::Select;
        h.run_steps(2);
        assert!(h.state().armed_stamp.is_none());
        send(&mut h, add("c", png()));
        send(&mut h, StampCommand::Cancel { id: "c".into() });
        assert_eq!(h.state().quick_tool, QuickTool::Select);
        send(&mut h, add("d", png()));
        h.state_mut().quick_tool = QuickTool::Hand;
        h.run_steps(2);
        let want = vec![
            StampEvent::Armed { id: "a".into() },
            StampEvent::Cancelled { id: "a".into() },
            StampEvent::Armed { id: "b".into() },
            StampEvent::Placed { id: "b".into(), page: 0 },
            StampEvent::Armed { id: "c".into() },
            StampEvent::Cancelled { id: "c".into() },
            StampEvent::Armed { id: "d".into() },
            StampEvent::Cancelled { id: "d".into() },
        ];
        assert_eq!(*ev.borrow(), want);
    }

    #[test]
    fn bad_png_and_missing_document_fail_without_arming() {
        let (mut h, ev) = app(true);
        send(&mut h, add("x", b"nope".to_vec()));
        assert!(h.state().armed_stamp.is_none() && h.state().quick_tool != QuickTool::HostStamp);
        assert!(matches!(ev.borrow().as_slice(), [StampEvent::Failed { id, .. }] if id == "x"));
        let (mut h, ev) = app(false);
        send(&mut h, add("y", png()));
        assert!(matches!(ev.borrow().as_slice(), [StampEvent::Failed { id, error }] if id == "y" && error.contains("no document")));
    }

    /// A click edit for the armed stamp, as the canvas queues it.
    fn click_edit(bytes: Vec<u8>) -> pdfcraft_engine::Edit {
        pdfcraft_engine::Edit::AddCustomStamp {
            page: 0,
            rect: [10.0, 10.0, 60.0, 40.0],
            name: "Approved".into(),
            file: pdfcraft_engine::MarkFile { name: "stamp.png".into(), bytes: Arc::new(bytes), page: 0 },
            author: "Ada".into(),
            extra: Some(("PpmStamp".into(), "{}".into())),
        }
    }

    #[test]
    fn an_accepted_click_is_placed_and_a_refused_one_fails() {
        let (mut h, ev) = app(true);
        send(&mut h, add("ok", png()));
        h.state_mut().views[0].pending_edit = Some(click_edit(png()));
        h.state_mut().quick_tool = QuickTool::Select;
        h.run_steps(3);
        // The header is fine, the data is cut off: the armed check passes, the engine refuses.
        let mut broken = png();
        broken.truncate(broken.len() - 25);
        send(&mut h, add("broken", broken.clone()));
        h.state_mut().views[0].pending_edit = Some(click_edit(broken));
        h.state_mut().quick_tool = QuickTool::Select;
        h.run_steps(3);
        let events = ev.borrow();
        assert_eq!(
            events[..3],
            [StampEvent::Armed { id: "ok".into() }, StampEvent::Placed { id: "ok".into(), page: 0 }, StampEvent::Armed { id: "broken".into() }]
        );
        assert!(matches!(&events[3], StampEvent::Failed { id, .. } if id == "broken"), "{events:?}");
        assert_eq!(events.len(), 4, "no placed after a failure: {events:?}");
    }

    #[test]
    fn a_sign_box_request_is_placed_at_once_selected_and_reported() {
        let (mut h, ev) = app(true);
        send(&mut h, StampCommand::AddSignBox { id: "sb".into(), role: "reviewer".into(), signer: None, label: "reviewer 서명란".into() });
        // No arming and no click: placed on the page being read, which is left on the Select tool.
        assert_eq!(*ev.borrow(), vec![StampEvent::Placed { id: "sb".into(), page: 0 }]);
        assert_eq!(h.state().quick_tool, QuickTool::Select);
        assert!(h.state().armed_stamp.is_none());
        let (i, doc) = h.state().active_ids().unwrap();
        let info = &h.state().session.get(doc).unwrap().info;
        let note = info.annotations.iter().find(|a| a.page == 0 && !a.is_reply()).expect("the sign box");
        assert_eq!(note.rect, [10.0, 120.0, 190.0, 180.0]);
        assert_eq!(h.state().views[i].comments.selected, Some((0, note.index)));
        // The box is in the file with its role, and undo takes it away again (the usual edit flow).
        let saved = h.state().session.save_bytes(doc).unwrap();
        assert!(saved.windows(11).any(|w| w == b"PpmSignRole"));
        assert!(h.state_mut().session.undo(doc).is_ok());
        let saved = h.state().session.save_bytes(doc).unwrap();
        assert!(!saved.windows(11).any(|w| w == b"PpmSignRole"));
    }

    #[test]
    fn a_sign_box_request_with_a_signer_stores_it_and_one_without_does_not() {
        let (mut h, ev) = app(true);
        send(&mut h, StampCommand::AddSignBox { id: "s1".into(), role: "reviewer".into(), signer: Some("u-kim".into()), label: "x".into() });
        assert_eq!(*ev.borrow(), vec![StampEvent::Placed { id: "s1".into(), page: 0 }]);
        let (_, doc) = h.state().active_ids().unwrap();
        let saved = h.state().session.save_bytes(doc).unwrap();
        assert!(saved.windows(9).any(|w| w == b"PpmSigner"));
        assert!(h.state_mut().session.undo(doc).is_ok());
        let saved = h.state().session.save_bytes(doc).unwrap();
        assert!(!saved.windows(9).any(|w| w == b"PpmSigner"));
    }

    fn place(id: &str, page: usize, x: f64, y: f64, size: Option<(f64, f64)>, bytes: Vec<u8>) -> StampCommand {
        StampCommand::Place { id: id.into(), page, x, y, size, name: "Approved".into(), bytes, meta: "{\"sig\":1}".into() }
    }

    #[test]
    fn a_place_request_stamps_at_once_without_arming_and_undo_takes_it_away() {
        let (mut h, ev) = app(true);
        send(&mut h, add("armed", png()));
        send(&mut h, place("p", 0, 10.0, 20.0, Some((50.0, 30.0)), png()));
        // The waiting armed stamp is cancelled first; then the stamp is placed with no click.
        assert_eq!(
            *ev.borrow(),
            vec![
                StampEvent::Armed { id: "armed".into() },
                StampEvent::Cancelled { id: "armed".into() },
                StampEvent::Placed { id: "p".into(), page: 0 }
            ]
        );
        assert!(h.state().armed_stamp.is_none());
        let (_, doc) = h.state().active_ids().unwrap();
        let note = h.state().session.get(doc).unwrap().info.annotations.iter().find(|a| a.page == 0).cloned().expect("the stamp");
        assert_rect(note.rect.map(f64::from), [10.0, 250.0, 60.0, 280.0]);
        let saved = h.state().session.save_bytes(doc).unwrap();
        assert!(saved.windows(8).any(|w| w == b"PpmStamp"));
        assert!(h.state_mut().session.undo(doc).is_ok());
        assert!(!h.state().session.save_bytes(doc).unwrap().windows(8).any(|w| w == b"PpmStamp"));
    }

    #[test]
    fn a_place_request_without_a_size_uses_the_natural_one() {
        let (mut h, ev) = app(true);
        send(&mut h, place("n", 0, 0.0, 0.0, None, png()));
        assert_eq!(*ev.borrow(), vec![StampEvent::Placed { id: "n".into(), page: 0 }]);
        let (_, doc) = h.state().active_ids().unwrap();
        let note = h.state().session.get(doc).unwrap().info.annotations.iter().find(|a| a.page == 0).cloned().expect("the stamp");
        // The 2 x 2 px PNG is 2 x 2 pt, top-left at the page's top-left.
        assert_rect(note.rect.map(f64::from), [0.0, 298.0, 2.0, 300.0]);
    }

    #[test]
    fn a_bad_place_request_fails_and_adds_nothing() {
        let (mut h, ev) = app(true);
        send(&mut h, place("page", 1, 0.0, 0.0, None, png()));
        send(&mut h, place("out", 0, 200.0, 0.0, None, png()));
        send(&mut h, place("nan", 0, f64::NAN, 0.0, None, png()));
        send(&mut h, place("png", 0, 0.0, 0.0, None, b"nope".to_vec()));
        send(&mut h, place("size", 0, 0.0, 0.0, Some((0.0, 10.0)), png()));
        send(&mut h, place("", 0, 0.0, 0.0, None, png()));
        let events = ev.borrow();
        assert_eq!(events.len(), 6, "{events:?}");
        assert!(events.iter().all(|e| matches!(e, StampEvent::Failed { .. })), "{events:?}");
        let (_, doc) = h.state().active_ids().unwrap();
        assert!(h.state().session.get(doc).unwrap().info.annotations.is_empty());
        let (mut h, ev) = app(false);
        send(&mut h, place("x", 0, 0.0, 0.0, None, png()));
        assert!(matches!(ev.borrow().as_slice(), [StampEvent::Failed { error, .. }] if error.contains("no document")));
    }

    #[test]
    fn a_place_request_is_refused_when_the_engine_refuses_the_picture() {
        let (mut h, ev) = app(true);
        // The header is fine, the data is cut off: the checks pass, the engine refuses.
        let mut broken = png();
        broken.truncate(broken.len() - 25);
        send(&mut h, place("broken", 0, 5.0, 5.0, Some((20.0, 20.0)), broken));
        assert!(matches!(ev.borrow().as_slice(), [StampEvent::Failed { id, .. }] if id == "broken"), "{:?}", ev.borrow());
    }

    #[test]
    fn a_sign_box_request_is_refused_without_a_document_or_with_a_bad_role() {
        let (mut h, ev) = app(false);
        send(&mut h, StampCommand::AddSignBox { id: "a".into(), role: "r".into(), signer: None, label: "r 서명란".into() });
        let (mut h2, ev2) = app(true);
        send(&mut h2, StampCommand::AddSignBox { id: "b".into(), role: " ".into(), signer: None, label: "x".into() });
        assert!(matches!(&ev.borrow()[..], [StampEvent::Failed { id, .. }] if id == "a"), "{:?}", ev.borrow());
        assert!(matches!(&ev2.borrow()[..], [StampEvent::Failed { id, .. }] if id == "b"), "{:?}", ev2.borrow());
    }

    #[test]
    fn the_watermark_follows_every_document_and_never_reaches_the_file() {
        let (mut h, _) = app(true);
        let (_, doc) = h.state().active_ids().unwrap();
        let before = h.state().session.save_bytes(doc).unwrap();
        send(&mut h, StampCommand::Watermark { text: Some("기밀 WATERMARK".into()) });
        h.run_steps(3);
        assert_eq!(h.state().screen_watermark.as_deref(), Some("기밀 WATERMARK"));
        assert_eq!(h.state().session.save_bytes(doc).unwrap(), before);
        // The open document draws it, and so does one opened after the message.
        assert_eq!(h.state().session.watermark_key(doc), Some("기밀 WATERMARK"));
        let blank = h.state_mut().session.create_blank(200.0, 200.0, 1).unwrap();
        h.state_mut().open_bytes("later.pdf", None, blank.as_ref().clone()).unwrap();
        h.run_steps(3);
        let keys: Vec<_> = h.state().views.iter().map(|v| h.state().session.watermark_key(v.id).map(str::to_string)).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| k.as_deref() == Some("기밀 WATERMARK")), "{keys:?}");
        send(&mut h, StampCommand::Watermark { text: Some("  ".into()) });
        h.run_steps(3);
        assert_eq!(h.state().screen_watermark, None);
        assert!(h.state().views.iter().all(|v| h.state().session.watermark_key(v.id).is_none()));
    }

    #[test]
    fn a_new_request_in_the_frame_after_a_click_does_not_steal_its_report() {
        let (mut h, ev) = app(true);
        send(&mut h, add("a", png()));
        // The click's outcome and the next request are both waiting when the tick runs.
        h.state_mut().views[0].host_stamp_outcome = Some(Ok(0));
        h.state_mut().quick_tool = QuickTool::Select;
        h.state().stamp_inbox.lock().unwrap().push(add("b", png()));
        h.run_steps(2);
        assert_eq!(
            *ev.borrow(),
            vec![StampEvent::Armed { id: "a".into() }, StampEvent::Placed { id: "a".into(), page: 0 }, StampEvent::Armed { id: "b".into() }]
        );
        assert_eq!(h.state().armed_stamp.as_ref().map(|a| a.id.as_str()), Some("b"));
    }

    #[test]
    fn a_document_that_forbids_comments_is_refused() {
        let mut session = pdfcraft_engine::Session::new();
        let id = session.open("p.pdf", None, Arc::new(pdf()), None).unwrap();
        let protection =
            pdfcraft_engine::Protection { permissions_password: Some("owner".into()), changes: pdfcraft_engine::Changes::None, ..Default::default() };
        session.apply(id, pdfcraft_engine::Edit::Protect(protection)).unwrap();
        let locked = session.save_bytes(id).unwrap().as_ref().clone();
        let events: Rc<RefCell<Vec<StampEvent>>> = Rc::default();
        let sink = events.clone();
        let mut h = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(move |_| {
            let mut app = PdfCraftApp::new();
            app.open_bytes("locked.pdf", None, locked).unwrap();
            app.host_stamp = Some(Box::new(move |e| sink.borrow_mut().push(e.clone())));
            app
        });
        h.run_steps(3);
        send(&mut h, add("z", png()));
        assert!(h.state().armed_stamp.is_none());
        assert!(
            matches!(events.borrow().as_slice(), [StampEvent::Failed { id, error }] if id == "z" && error.contains("comments")),
            "{:?}",
            events.borrow()
        );
    }

    #[test]
    fn a_file_opened_by_the_host_ends_the_armed_stamp() {
        let (mut h, ev) = app(true);
        send(&mut h, add("a", png()));
        h.state().startup_inbox.lock().unwrap().push(("b.pdf".into(), pdf()));
        h.run_steps(3);
        assert_eq!(*ev.borrow(), vec![StampEvent::Armed { id: "a".into() }, StampEvent::Cancelled { id: "a".into() }]);
    }

    /// The names of the open documents, in tab order.
    fn tab_names(h: &Harness<'static, PdfCraftApp>) -> Vec<String> {
        let state = h.state();
        state.views.iter().filter_map(|v| state.session.get(v.id)).map(|d| d.name.clone()).collect()
    }

    #[test]
    fn an_open_without_replace_adds_a_tab_and_one_with_replace_closes_the_others() {
        let (mut h, _) = app(true);
        h.state().startup_inbox.lock().unwrap().push(("b.pdf".into(), pdf()));
        h.run_steps(3);
        assert_eq!(tab_names(&h), vec!["a.pdf", "b.pdf"], "no replace: a tab is added as before");
        h.state().startup_inbox.lock().unwrap().push(("c.pdf".into(), pdf()));
        h.state().ownership_inbox.lock().unwrap().push(pdfcraft_engine::Ownership { replace: true, ..Default::default() });
        h.run_steps(3);
        assert_eq!(tab_names(&h), vec!["c.pdf"], "replace: the others are closed");
        assert_eq!(h.state().active, Some(0));
    }

    #[test]
    fn huge_pixel_dimensions_are_refused_from_the_header_alone() {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, MAX_SIDE_PX + 1, 1);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::One);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&vec![0u8; (MAX_SIDE_PX as usize + 1).div_ceil(8)]).unwrap();
        drop(w);
        assert!(validate("r", "n", out, 10.0, 10.0, "{}").unwrap_err().contains("too large"));
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 8192, 4096);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::One);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&vec![0u8; 1024 * 4096]).unwrap();
        drop(w);
        assert!(validate("r", "n", out, 10.0, 10.0, "{}").unwrap_err().contains("too large"), "32 Mpx in all");
    }
    // ── signing in a sign box (`pdfcraft:sign-in-box`) ─────────────────────────────────────────

    use crate::host_annots::{AnnotCommand, AnnotEvent};

    type Heard = Rc<RefCell<Vec<AnnotEvent>>>;

    /// A document the page shares (`annotSync`, actor "alice"), with what both sides hear.
    fn shared_app() -> (Harness<'static, PdfCraftApp>, Rc<RefCell<Vec<StampEvent>>>, Heard) {
        shared_app_pages(1)
    }

    fn shared_app_pages(pages: usize) -> (Harness<'static, PdfCraftApp>, Rc<RefCell<Vec<StampEvent>>>, Heard) {
        let events: Rc<RefCell<Vec<StampEvent>>> = Rc::default();
        let heard: Heard = Rc::default();
        let (sink, listen) = (events.clone(), heard.clone());
        let mut app = PdfCraftApp::new();
        app.host_stamp = Some(Box::new(move |e| sink.borrow_mut().push(e.clone())));
        app.host_annots = Some(Box::new(move |e| listen.borrow_mut().push(e.clone())));
        app.ownership_inbox.lock().unwrap().push(pdfcraft_engine::Ownership { actor: Some("alice".into()), annot_sync: true, ..Default::default() });
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf_pages(pages)));
        let mut harness = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(move |_| app);
        harness.run_steps(4);
        (harness, events, heard)
    }

    /// A sign box another user (u-a) placed, as the server's list carries it.
    const REMOTE_BOX: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?><xfdf xmlns=\"http://ns.adobe.com/xfdf/\" xml:space=\"preserve\"><fields /><annots><freetext page=\"0\" rect=\"20,200,200,260\" flags=\"print\" name=\"vb\" subject=\"reviewer\" date=\"D:20261026120000+09'00'\" creationdate=\"D:20261026120000+09'00'\" title=\"Ada\"><trn-custom-data bytes=\"{&quot;ppmSignRole&quot;:&quot;reviewer&quot;,&quot;authorId&quot;:&quot;u-a&quot;}\"/><contents>reviewer</contents><defaultappearance>0 0 0 rg /Helvetica 12 Tf</defaultappearance></freetext></annots></xfdf>";

    fn sign_in(id: &str, annot_id: &str) -> StampCommand {
        StampCommand::SignIn {
            id: id.into(),
            annot_id: annot_id.into(),
            page: 0,
            x: 20.0,
            y: 40.0,
            size: Some((90.0, 30.0)),
            name: "Signature".into(),
            bytes: png(),
            meta: "{\"signBoxId\":\"vb\"}".into(),
        }
    }

    /// The annotation count and the stamp's lock state on page 0.
    fn page_notes(h: &Harness<'static, PdfCraftApp>) -> Vec<(String, bool)> {
        let (_, doc) = h.state().active_ids().unwrap();
        h.state().session.get(doc).unwrap().info.annotations.iter().map(|a| (a.subtype.clone(), a.locked)).collect()
    }

    fn remote_box(h: &mut Harness<'static, PdfCraftApp>) {
        let item = pdfcraft_engine::RemoteAnnot { annot_id: "vb".into(), author_id: "u-a".into(), xfdf: REMOTE_BOX.into() };
        h.state().annot_inbox.lock().unwrap().push(AnnotCommand::Reset { items: vec![item], seq: 0 });
        h.run_steps(3);
    }

    /// Signing in someone else's sign box: the box goes, a sealed stamp takes its place, and the
    /// page hears both (the box's delete and the stamp's upsert) in `annots-changed`.
    #[test]
    fn signing_in_replaces_the_box_with_a_sealed_stamp_and_the_page_hears_both() {
        let (mut h, ev, heard) = shared_app();
        remote_box(&mut h);
        assert_eq!(page_notes(&h), vec![("FreeText".to_string(), false)]);
        heard.borrow_mut().clear();
        send(&mut h, sign_in("s1", "vb"));
        h.run_steps(3);
        assert_eq!(*ev.borrow(), vec![StampEvent::Placed { id: "s1".into(), page: 0 }]);
        assert_eq!(page_notes(&h), vec![("Stamp".to_string(), true)], "the box is gone and the stamp is locked");
        let changes: Vec<_> = heard.borrow().iter().filter_map(|e| if let AnnotEvent::Changed { changes, .. } = e { Some(changes.clone()) } else { None }).collect();
        assert!(changes.iter().any(|c| c.deletes.contains(&"vb".to_string())), "the box's delete is sent: {changes:?}");
        assert!(changes.iter().any(|c| c.upserts.iter().any(|u| u.subtype.eq_ignore_ascii_case("stamp") && u.xfdf.contains("lockedcontents"))), "{changes:?}");
        assert!(
            !heard.borrow().iter().any(|e| matches!(e, AnnotEvent::SignBoxSelected { .. })),
            "nothing is selected, so no sign box is reported"
        );
        // The stamp is where it was asked, and it keeps the sign box's id in its metadata.
        let (_, doc) = h.state().active_ids().unwrap();
        let note = h.state().session.get(doc).unwrap().info.annotations[0].clone();
        assert_rect(note.rect.map(f64::from), [20.0, 230.0, 110.0, 260.0]);
    }

    /// Nothing is signed in a thing that is not a sign box, or not there; the document stays as it was.
    #[test]
    fn signing_in_something_else_is_refused() {
        let (mut h, ev, _) = shared_app();
        remote_box(&mut h);
        send(&mut h, sign_in("none", "nope"));
        // A stamp is not a sign box.
        send(&mut h, place("st", 0, 5.0, 5.0, Some((20.0, 20.0)), png()));
        let (_, doc) = h.state().active_ids().unwrap();
        let session = &h.state().session;
        let at = session.get(doc).unwrap().info.annotations.iter().find(|a| a.subtype == "Stamp").map(|a| (a.page, a.index)).expect("the stamp was placed");
        let stamp_id = session.get(doc).unwrap().annot_name_at(at.0, at.1).expect("the stamp has a name");
        send(&mut h, sign_in("stamp", &stamp_id));
        let events = ev.borrow();
        assert!(matches!(&events[0], StampEvent::Failed { id, .. } if id == "none"), "{events:?}");
        let for_stamp: Vec<_> = events.iter().filter(|e| matches!(e, StampEvent::Failed { id, .. } | StampEvent::Placed { id, .. } if id == "stamp")).collect();
        assert!(matches!(for_stamp.as_slice(), [StampEvent::Failed { .. }]), "exactly one failure for the stamp, no placement: {events:?}");
        drop(events);
        assert_eq!(page_notes(&h).iter().filter(|(t, _)| t == "Stamp").count(), 1, "no second stamp");
        assert!(page_notes(&h).iter().any(|(t, _)| t == "FreeText"), "the box is still there");
        // A box the user has locked is not signed in either.
        let locked = pdfcraft_engine::RemoteAnnot { annot_id: "vb".into(), author_id: "u-a".into(), xfdf: REMOTE_BOX.replace("flags=\"print\"", "flags=\"print,locked\"") };
        h.state().annot_inbox.lock().unwrap().push(AnnotCommand::Reset { items: vec![locked], seq: 0 });
        h.run_steps(3);
        assert!(page_notes(&h).iter().any(|(t, l)| t == "FreeText" && *l), "{:?}", page_notes(&h));
        ev.borrow_mut().clear();
        send(&mut h, sign_in("locked", "vb"));
        assert!(matches!(ev.borrow().as_slice(), [StampEvent::Failed { id, .. }] if id == "locked"), "{:?}", ev.borrow());
    }

    /// A sign box on another page than the stamp is asked for is not signed in; the box stays.
    #[test]
    fn signing_in_a_box_on_another_page_is_refused() {
        let (mut h, ev, _) = shared_app_pages(2);
        remote_box(&mut h);
        ev.borrow_mut().clear();
        let StampCommand::SignIn { id, annot_id, x, y, size, name, bytes, meta, .. } = sign_in("far", "vb") else { unreachable!() };
        send(&mut h, StampCommand::SignIn { id, annot_id, page: 1, x, y, size, name, bytes, meta });
        assert!(matches!(ev.borrow().as_slice(), [StampEvent::Failed { id, error }] if id == "far" && error.contains("another page")), "{:?}", ev.borrow());
        assert_eq!(page_notes(&h), vec![("FreeText".to_string(), false)], "the box is untouched");
    }

    /// The only deletion of someone else's annotation that reaches the page is a sign box a
    /// signature replaced: one the server itself takes away is not echoed back as a delete.
    #[test]
    fn another_users_annotation_taken_away_by_the_server_is_not_sent_as_a_delete() {
        let (mut h, _, heard) = shared_app();
        remote_box(&mut h);
        h.state().annot_inbox.lock().unwrap().push(AnnotCommand::Reset { items: vec![], seq: 1 });
        h.run_steps(3);
        assert!(page_notes(&h).is_empty(), "{:?}", page_notes(&h));
        let sent: Vec<_> = heard.borrow().iter().filter_map(|e| if let AnnotEvent::Changed { changes, .. } = e { Some(changes.deletes.clone()) } else { None }).flatten().collect();
        assert!(!sent.contains(&"vb".to_string()), "{sent:?}");
    }

    /// A sealed stamp: no move, resize, delete, unlock, text or status edit, from the user's side
    /// (`apply_edit`) or from the page's (`annots-apply`/`annots-reset`); undo and redo leave it.
    #[test]
    fn a_signature_cannot_be_changed_unlocked_deleted_or_undone() {
        use pdfcraft_engine::Edit;
        let (mut h, _, _) = shared_app();
        remote_box(&mut h);
        // An earlier edit that undo could otherwise reach back to.
        send(&mut h, place("st", 0, 150.0, 5.0, Some((20.0, 20.0)), png()));
        send(&mut h, sign_in("s1", "vb"));
        h.run_steps(3);
        let (_, doc) = h.state().active_ids().unwrap();
        let stamp = h
            .state()
            .session
            .get(doc)
            .unwrap()
            .info
            .annotations
            .iter()
            .find(|a| a.locked && a.subtype == "Stamp")
            .cloned()
            .expect("the signature");
        let (page, index) = (stamp.page, stamp.index);
        let nm = stamp.name.clone().expect("a name");
        let refused = [
            Edit::MoveAnnotation { page, index, dx: 5.0, dy: 5.0 },
            Edit::ResizeAnnotation { page, index, rect: [0.0, 0.0, 300.0, 300.0] },
            Edit::DeleteAnnotation { page, index },
            Edit::LockAnnotation { page, index, locked: false },
            Edit::SetAnnotationContents { page, index, text: "x".into() },
            Edit::SetAnnotationStatus { page, index, state: pdfcraft_engine::ReviewState::Rejected, author: "alice".into() },
            Edit::MarkAnnotation { page, index, marked: true, author: "alice".into() },
        ];
        let before = h.state().session.save_bytes(doc).unwrap();
        for edit in refused {
            let label = format!("{edit:?}");
            assert!(h.state_mut().session.apply(doc, edit).is_err(), "{label} must be refused");
            assert!(!h.state_mut().apply_edit(Edit::DeleteAnnotation { page, index }));
        }
        assert_eq!(h.state().session.save_bytes(doc).unwrap(), before, "refused edits changed nothing");
        // The page's messages can neither replace it nor take it away (a reset without it included).
        let remote = pdfcraft_engine::RemoteAnnot { annot_id: nm.clone(), author_id: "u-a".into(), xfdf: REMOTE_BOX.replace("name=\"vb\"", &format!("name=\"{nm}\"")) };
        h.state_mut().session.annots_apply(doc, vec![remote], vec![nm.clone()]).unwrap();
        h.state_mut().session.annots_reset(doc, vec![]).unwrap();
        assert_eq!(page_notes(&h).iter().filter(|(t, l)| t == "Stamp" && *l).count(), 1, "the signature is still there");
        assert!(h.state().session.get(doc).unwrap().info.annotations.iter().all(|a| a.subtype == "Stamp"), "no box or copy came back");
        // History starts at the signature: undo and redo find nothing to undo.
        assert!(h.state_mut().session.undo(doc).is_err());
        assert!(h.state_mut().session.redo(doc).is_err());
        assert!(page_notes(&h).iter().any(|(t, l)| t == "Stamp" && *l));
    }

    /// Saved and opened again, the signature is still locked and sealed; a locked copy keeps refusing.
    #[test]
    fn a_signature_stays_sealed_after_saving_and_reopening() {
        let (mut h, ev) = app(true);
        send(&mut h, StampCommand::AddSignBox { id: "b".into(), role: "r".into(), signer: None, label: "r 서명란".into() });
        let (_, doc) = h.state().active_ids().unwrap();
        let index = h.state().session.get(doc).unwrap().info.annotations[0].index;
        let nm = h.state().session.get(doc).unwrap().annot_name_at(0, index).expect("the box has a name");
        send(&mut h, sign_in("s1", &nm));
        assert_eq!(ev.borrow().last(), Some(&StampEvent::Placed { id: "s1".into(), page: 0 }), "{:?}", ev.borrow());
        let saved = h.state().session.save_bytes(doc).unwrap().as_ref().clone();
        assert!(saved.windows(8).any(|w| w == b"PpmStamp"), "stamp key");
        // The server reads the signature from this dictionary: /PpmStamp carries the meta JSON
        // (with signBoxId) and /F has Locked(128)|LockedContents(512), both in the same object.
        let text = String::from_utf8_lossy(&saved);
        let at = text.rfind("/PpmStamp").expect("stamp key in the saved file");
        let object_start = text[..at].rfind(" obj").expect("inside an object");
        let object_end = at + text[at..].find("endobj").expect("object end");
        let stamp = &text[object_start..object_end];
        assert!(stamp.contains("signBoxId") && stamp.contains("vb"), "meta JSON: {stamp}");
        let flags: i64 = stamp.split("/F ").nth(1).and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()).expect("/F");
        assert_eq!(flags & (128 | 512), 128 | 512, "Locked and LockedContents in /F={flags}");
        let reopened = h.state_mut().session.open_new("again.pdf", std::sync::Arc::new(saved)).unwrap_or_else(|e| panic!("{e}"));
        assert!(h.state().session.get(reopened).unwrap().info.annotations.iter().all(|a| a.subtype == "Stamp" && a.locked));
        let notes = h.state().session.get(reopened).unwrap().info.annotations.clone();
        assert_eq!(notes.len(), 1);
        let (page, index) = (notes[0].page, notes[0].index);
        use pdfcraft_engine::Edit;
        assert!(h.state_mut().session.apply(reopened, Edit::LockAnnotation { page, index, locked: false }).is_err());
        assert!(h.state_mut().session.apply(reopened, Edit::DeleteAnnotation { page, index }).is_err());
        assert!(h.state_mut().session.apply(reopened, Edit::MoveAnnotation { page, index, dx: 1.0, dy: 1.0 }).is_err());
    }

    /// `source`: a sign box the program adds (and selects) is reported as `api`; the user's own
    /// click on a box is `user`.
    #[test]
    fn a_sign_box_selection_is_api_when_the_program_chose_it_and_user_otherwise() {
        let (mut h, _, heard) = shared_app();
        send(&mut h, StampCommand::AddSignBox { id: "b1".into(), role: "r".into(), signer: None, label: "r 서명란".into() });
        h.run_steps(3);
        let sources = |heard: &Heard| -> Vec<bool> {
            heard.borrow().iter().filter_map(|e| if let AnnotEvent::SignBoxSelected { from_api, .. } = e { Some(*from_api) } else { None }).collect()
        };
        assert_eq!(sources(&heard), vec![true], "added by the program");
        // The user picks something else, then the box again: those are theirs.
        let (i, doc) = h.state().active_ids().unwrap();
        let index = h.state().session.get(doc).unwrap().info.annotations[0].index;
        h.state_mut().views[i].comments.selected = None;
        h.run_steps(3);
        h.state_mut().views[i].comments.selected = Some((0, index));
        h.run_steps(3);
        assert_eq!(sources(&heard), vec![true, false]);
        // `annots-select` is the program's again.
        h.state_mut().views[i].comments.selected = None;
        h.run_steps(3);
        let nm = h.state().session.get(doc).unwrap().annot_name_at(0, index).unwrap();
        h.state().annot_inbox.lock().unwrap().push(AnnotCommand::Select { annot_id: nm });
        h.run_steps(3);
        assert_eq!(sources(&heard), vec![true, false, true]);
    }
}
