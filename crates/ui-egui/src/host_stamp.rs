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
    /// A sign box (`pdfcraft:sign-box-add`): a FreeText box of [`SIGN_BOX_SIZE`] carrying `role`,
    /// put at once in the middle of the page being read and left selected.
    AddSignBox {
        id: String,
        role: String,
        label: String,
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
pub(crate) fn validate_sign_box(id: &str, role: &str, label: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err("the request id is missing or too long".into());
    }
    if role.trim().is_empty() || role.len() > MAX_ID_BYTES || label.trim().is_empty() || label.len() > MAX_META_BYTES {
        return Err("the sign box needs a role and a label of reasonable length".into());
    }
    Ok(())
}

/// The sign box's rectangle in user space: [`SIGN_BOX_SIZE`] in the middle of the page as shown
/// (upright whatever the page's `/Rotate`), kept inside it when the page is smaller.
pub(crate) fn sign_box_rect(page: &pdfcraft_render::PageInfo) -> [f64; 4] {
    let (w, h) = SIGN_BOX_SIZE;
    let (x, y) = (((f64::from(page.width) - w) / 2.0).max(0.0), ((f64::from(page.height) - h) / 2.0).max(0.0));
    let corners = [(x, y), (x + w, y + h)].map(|(x, y)| page.view_to_user(x as f32, y as f32));
    [corners[0][0].min(corners[1][0]), corners[0][1].min(corners[1][1]), corners[0][0].max(corners[1][0]), corners[0][1].max(corners[1][1])]
        .map(f64::from)
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
    fn place_sign_box(&mut self, id: &str, role: String, label: String) -> Result<usize, String> {
        validate_sign_box(id, &role, &label)?;
        let (index, doc) = self.active_ids().ok_or("no document is open")?;
        let state = self.session.get(doc).ok_or("no document is open")?;
        if !state.allows_annotation() {
            return Err("the document doesn't allow comments".into());
        }
        let page = self.views[index].current.min(state.info.pages.len().saturating_sub(1));
        let rect = sign_box_rect(state.info.pages.get(page).ok_or("the document has no pages")?);
        let author = self.comment_prefs.author.clone();
        if self.apply_edit(pdfcraft_engine::Edit::AddSignBox { page, rect, role, label, author }) {
            self.quick_tool = QuickTool::Select;
            Ok(page)
        } else {
            Err(self.toast.as_ref().map(|(m, _)| m.clone()).unwrap_or_else(|| "the sign box could not be placed".to_string()))
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
                StampCommand::AddSignBox { id, role, label } => {
                    self.disarm_stamp();
                    match self.place_sign_box(&id, role, label) {
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
        assert!(validate_sign_box("s1", "reviewer", "reviewer 서명란").is_ok());
        assert!(validate_sign_box("", "reviewer", "x").is_err(), "empty id");
        assert!(validate_sign_box("s", " ", "x").is_err(), "empty role");
        assert!(validate_sign_box("s", "r", "").is_err(), "empty label");
        assert!(validate_sign_box("s", "r", &"x".repeat(MAX_META_BYTES + 1)).is_err(), "huge label");
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

    use super::*;

    fn pdf() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>",
            "<< /Type /Page /Parent 2 0 R >>",
        ];
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
        send(&mut h, StampCommand::AddSignBox { id: "sb".into(), role: "reviewer".into(), label: "reviewer 서명란".into() });
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
    fn a_sign_box_request_is_refused_without_a_document_or_with_a_bad_role() {
        let (mut h, ev) = app(false);
        send(&mut h, StampCommand::AddSignBox { id: "a".into(), role: "r".into(), label: "r 서명란".into() });
        let (mut h2, ev2) = app(true);
        send(&mut h2, StampCommand::AddSignBox { id: "b".into(), role: " ".into(), label: "x".into() });
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
}
