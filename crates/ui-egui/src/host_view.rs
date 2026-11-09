//! Moving the view and marking places for the embedding page (web `pdfcraft:goto`,
//! `pdfcraft:highlight`, `pdfcraft:highlight-clear`), and telling it when the document was first drawn
//! (`pdfcraft:opened`).
//!
//! A highlight here is a screen overlay: it is no annotation, so it is not saved, not in undo and
//! not in `annots-changed`. Pages count from 0 and rectangles are in points on the page as shown
//! (top-left origin, upright whatever the page's `/Rotate`), the same as `stamp-place`.

use std::sync::Arc;

use egui::Color32;

use crate::PdfCraftApp;

const MAX_ID_BYTES: usize = 200;
/// The most rectangles one highlight takes (a text line each; more is a page's worth of lines).
pub const MAX_HIGHLIGHT_RECTS: usize = 2000;
/// The alpha of a colour given without one (`#rrggbb`): the text under it stays readable.
const DEFAULT_ALPHA: u8 = 100;
/// The colour when none is given: yellow, like a find match.
fn default_color() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 214, 0, 110)
}

/// What the host asks for. A rectangle is `[x, y, width, height]`.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewCommand {
    /// Show the page, or the place on it (the rectangle's centre goes to the middle of the window).
    Goto { id: String, page: usize, rect: Option<[f64; 4]> },
    /// Mark places on a page. `transient` must be true: a highlight that is kept would be an annotation.
    Highlight { id: String, page: usize, rects: Vec<[f64; 4]>, color: Option<String>, transient: bool },
    /// Take the transient highlight away.
    ClearHighlight,
}

/// What the host is told.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewEvent {
    /// The document the host opened was drawn for the first time (once for each `pdfcraft:open`).
    Opened,
    GotoDone { id: String },
    GotoFailed { id: String, error: String },
    HighlightDone { id: String },
    HighlightFailed { id: String, error: String },
}

impl ViewCommand {
    /// Read a `pdfcraft:goto`, `pdfcraft:highlight` or `pdfcraft:highlight-clear` message (`kind` is the part after
    /// `pdfcraft:`) from its JSON. A rect is `{ x, y, width, height }`. The error says what was wrong.
    pub fn from_json(kind: &str, v: &serde_json::Value) -> Result<Self, String> {
        let id = || v.get("id").and_then(|x| x.as_str()).map(str::to_string).ok_or_else(|| "id must be a string".to_string());
        let page = || {
            v.get("page")
                .and_then(|x| x.as_u64())
                .filter(|p| *p <= u64::from(u32::MAX))
                .map(|p| p as usize)
                .ok_or_else(|| "page must be a whole number from 0".to_string())
        };
        let rect = |r: &serde_json::Value| {
            let n = |key: &str| r.get(key).and_then(|x| x.as_f64()).ok_or_else(|| format!("a rect needs a number {key}"));
            Ok::<_, String>([n("x")?, n("y")?, n("width")?, n("height")?])
        };
        match kind {
            "goto" => {
                let rect = match v.get("rect") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(r) => Some(rect(r)?),
                };
                Ok(Self::Goto { id: id()?, page: page()?, rect })
            }
            "highlight" => {
                let rects = v.get("rects").and_then(|x| x.as_array()).ok_or("rects must be an array")?;
                if rects.len() > MAX_HIGHLIGHT_RECTS {
                    return Err(format!("at most {MAX_HIGHLIGHT_RECTS} rects"));
                }
                let color = match v.get("color") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(c) => Some(c.as_str().ok_or("color must be a string")?.to_string()),
                };
                let transient = v.get("transient").and_then(|x| x.as_bool()).ok_or("transient must be a boolean")?;
                Ok(Self::Highlight { id: id()?, page: page()?, rects: rects.iter().map(rect).collect::<Result<_, _>>()?, color, transient })
            }
            "highlight-clear" => Ok(Self::ClearHighlight),
            other => Err(format!("unknown view message {other}")),
        }
    }
}

impl ViewEvent {
    /// The message the page gets (`type` is added by the bridge): its name after `pdfcraft:` and its fields.
    pub fn to_message(&self) -> (&'static str, serde_json::Value) {
        use serde_json::json;
        match self {
            Self::Opened => ("opened", json!({})),
            Self::GotoDone { id } => ("goto-done", json!({ "id": id })),
            Self::GotoFailed { id, error } => ("goto-failed", json!({ "id": id, "error": error })),
            Self::HighlightDone { id } => ("highlight-done", json!({ "id": id })),
            Self::HighlightFailed { id, error } => ("highlight-failed", json!({ "id": id, "error": error })),
        }
    }
}

/// Requests from the host, drained every frame.
pub type ViewInbox = Arc<std::sync::Mutex<Vec<ViewCommand>>>;

/// Tells the embedding page what happened to its view requests (web).
pub type HostViewFn = Box<dyn Fn(&ViewEvent)>;

/// The overlay on one page: view-space rectangles `[x0, y0, x1, y1]` and their colour.
#[derive(Clone, Debug, PartialEq)]
pub struct HostHighlight {
    pub page: usize,
    pub rects: Vec<[f32; 4]>,
    pub color: Color32,
}

/// Read a CSS hex colour: `#rgb`, `#rrggbb` or `#rrggbbaa`. Without an alpha it is [`DEFAULT_ALPHA`].
pub(crate) fn parse_color(text: &str) -> Result<Color32, String> {
    let bad = || format!("color must be #rgb, #rrggbb or #rrggbbaa, not {:?}", text.chars().take(20).collect::<String>());
    let hex = text.strip_prefix('#').ok_or_else(bad)?;
    if !hex.is_ascii() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| bad());
    let (r, g, b, a) = match hex.len() {
        3 => {
            let one = |i: usize| u8::from_str_radix(&hex[i..=i], 16).map(|v| v * 17).map_err(|_| bad());
            (one(0)?, one(1)?, one(2)?, DEFAULT_ALPHA)
        }
        6 => (pair(0)?, pair(2)?, pair(4)?, DEFAULT_ALPHA),
        8 => (pair(0)?, pair(2)?, pair(4)?, pair(6)?),
        _ => return Err(bad()),
    };
    Ok(Color32::from_rgba_unmultiplied(r, g, b, a))
}

fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_ID_BYTES { Err("the request id is missing or too long".into()) } else { Ok(()) }
}

/// A rectangle `[x, y, width, height]` that is finite with sizes of 0 or more.
fn check_rect(r: &[f64; 4]) -> Result<(), String> {
    if !r.iter().all(|v| v.is_finite()) || r[2] < 0.0 || r[3] < 0.0 {
        return Err("a rect needs finite x, y and width, height of 0 or more".into());
    }
    Ok(())
}

impl PdfCraftApp {
    fn emit_view(&self, event: ViewEvent) {
        if let Some(report) = &self.host_view {
            report(&event);
        }
    }

    /// Scroll the active document to a page, or to a place on it. Returns why it was refused.
    fn goto_page(&mut self, page: usize, rect: Option<[f64; 4]>) -> Result<(), String> {
        let (index, doc) = self.active_ids().ok_or("no document is open")?;
        let state = self.session.get(doc).ok_or("no document is open")?;
        let info = state.info.pages.get(page).ok_or("the page is out of range")?;
        match rect {
            None => self.views[index].go_to_page(page),
            Some(r) => {
                check_rect(&r)?;
                let user = info.view_rect_to_user([r[0] as f32, r[1] as f32, (r[0] + r[2]) as f32, (r[1] + r[3]) as f32]);
                self.views[index].go_to_rect(page, user, &state.info);
            }
        }
        Ok(())
    }

    /// Put the transient highlight on the active document, in place of the one before.
    fn set_highlight(&mut self, page: usize, rects: &[[f64; 4]], color: Option<&str>, transient: bool) -> Result<(), String> {
        if !transient {
            return Err("only a transient highlight is supported (transient must be true)".into());
        }
        if rects.len() > MAX_HIGHLIGHT_RECTS {
            return Err(format!("at most {MAX_HIGHLIGHT_RECTS} rects"));
        }
        rects.iter().try_for_each(check_rect)?;
        let color = color.map(parse_color).transpose()?.unwrap_or_else(default_color);
        let (index, doc) = self.active_ids().ok_or("no document is open")?;
        let state = self.session.get(doc).ok_or("no document is open")?;
        if page >= state.info.pages.len() {
            return Err("the page is out of range".into());
        }
        let rects = rects.iter().map(|r| [r[0] as f32, r[1] as f32, (r[0] + r[2]) as f32, (r[1] + r[3]) as f32]).collect();
        self.views[index].host_highlight = Some(HostHighlight { page, rects, color });
        Ok(())
    }

    /// Once a frame: carry out the host's view requests and tell it when the document first appeared.
    pub(crate) fn host_view_tick(&mut self, ctx: &egui::Context) {
        let commands: Vec<ViewCommand> = self.view_inbox.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for command in commands {
            let changed = match command {
                ViewCommand::Goto { id, page, rect } => {
                    let result = check_id(&id).and_then(|()| self.goto_page(page, rect));
                    self.emit_view(match result {
                        Ok(()) => ViewEvent::GotoDone { id },
                        Err(error) => ViewEvent::GotoFailed { id, error },
                    });
                    true
                }
                ViewCommand::Highlight { id, page, rects, color, transient } => {
                    let result = check_id(&id).and_then(|()| self.set_highlight(page, &rects, color.as_deref(), transient));
                    self.emit_view(match result {
                        Ok(()) => ViewEvent::HighlightDone { id },
                        Err(error) => ViewEvent::HighlightFailed { id, error },
                    });
                    true
                }
                ViewCommand::ClearHighlight => {
                    self.views.iter_mut().for_each(|v| v.host_highlight = None);
                    true
                }
            };
            if changed {
                ctx.request_repaint();
            }
        }
        self.report_opened();
    }

    /// `opened`: the document a `pdfcraft:open` brought was drawn. Waits while the file is still on its way
    /// (or a password is asked for); gives up when none came of it.
    fn report_opened(&mut self) {
        let Some(before) = self.opened_wait.as_ref() else { return };
        if self.password_prompt.is_some() || self.host_open_pending() || self.open_in_flight() {
            return;
        }
        match self.views.iter().find(|v| !before.contains(&v.id)) {
            Some(view) if view.first_painted => {
                self.opened_wait = None;
                self.emit_view(ViewEvent::Opened);
            }
            Some(_) => {}
            // The open failed: there is nothing to draw.
            None => self.opened_wait = None,
        }
    }

    /// A file of the host's is about to be opened: the next view that appears is the one `opened` is for. The newest
    /// open wins; a document opened behind the one showing is told when it is first drawn, unless a later open came.
    pub(crate) fn expect_opened(&mut self) {
        if self.host_view.is_some() {
            self.opened_wait = Some(self.views.iter().map(|v| v.id).collect());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_css_hex() {
        assert_eq!(parse_color("#ff0000").unwrap(), Color32::from_rgba_unmultiplied(255, 0, 0, DEFAULT_ALPHA));
        assert_eq!(parse_color("#f00").unwrap(), Color32::from_rgba_unmultiplied(255, 0, 0, DEFAULT_ALPHA));
        assert_eq!(parse_color("#00ff0080").unwrap(), Color32::from_rgba_unmultiplied(0, 255, 0, 0x80));
        for bad in ["", "ff0000", "#ff00", "#gg0000", "#ff00000", "red", "#ééé", "#ff0000 "] {
            assert!(parse_color(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn rects_must_be_finite_and_not_negative() {
        assert!(check_rect(&[0.0, 0.0, 10.0, 10.0]).is_ok());
        assert!(check_rect(&[-5.0, -5.0, 0.0, 0.0]).is_ok());
        assert!(check_rect(&[0.0, 0.0, -1.0, 1.0]).is_err());
        assert!(check_rect(&[f64::NAN, 0.0, 1.0, 1.0]).is_err());
        assert!(check_rect(&[0.0, f64::INFINITY, 1.0, 1.0]).is_err());
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_every_message_shape() {
        let goto = ViewCommand::from_json("goto", &json!({"id": "g", "page": 2, "rect": {"x": 1, "y": 2, "width": 3, "height": 4}})).unwrap();
        assert_eq!(goto, ViewCommand::Goto { id: "g".into(), page: 2, rect: Some([1.0, 2.0, 3.0, 4.0]) });
        let bare = ViewCommand::from_json("goto", &json!({"id": "g", "page": 0})).unwrap();
        assert_eq!(bare, ViewCommand::Goto { id: "g".into(), page: 0, rect: None });
        let h = ViewCommand::from_json(
            "highlight",
            &json!({"id": "h", "page": 1, "rects": [{"x": 0, "y": 0, "width": 5, "height": 5}], "color": "#ff0", "transient": true}),
        )
        .unwrap();
        assert_eq!(
            h,
            ViewCommand::Highlight { id: "h".into(), page: 1, rects: vec![[0.0, 0.0, 5.0, 5.0]], color: Some("#ff0".into()), transient: true }
        );
        assert_eq!(ViewCommand::from_json("highlight-clear", &json!({})).unwrap(), ViewCommand::ClearHighlight);
    }

    #[test]
    fn refuses_wrong_shapes() {
        for (kind, v) in [
            ("goto", json!({"page": 0})),
            ("goto", json!({"id": "g"})),
            ("goto", json!({"id": "g", "page": -1})),
            ("goto", json!({"id": "g", "page": 1.5})),
            ("goto", json!({"id": "g", "page": "1"})),
            ("goto", json!({"id": "g", "page": 0, "rect": {"x": 1}})),
            ("highlight", json!({"id": "h", "page": 0, "rects": {}, "transient": true})),
            ("highlight", json!({"id": "h", "page": 0, "rects": [], "transient": "yes"})),
            ("highlight", json!({"id": "h", "page": 0, "rects": [], "color": 3, "transient": true})),
            ("nope", json!({})),
        ] {
            assert!(ViewCommand::from_json(kind, &v).is_err(), "{kind} {v} must be refused");
        }
    }

    #[test]
    fn events_become_messages() {
        assert_eq!(ViewEvent::Opened.to_message().0, "opened");
        assert_eq!(ViewEvent::GotoFailed { id: "g".into(), error: "e".into() }.to_message(), ("goto-failed", json!({"id": "g", "error": "e"})));
        assert_eq!(ViewEvent::HighlightDone { id: "h".into() }.to_message(), ("highlight-done", json!({"id": "h"})));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod flow_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use egui_kittest::Harness;

    use super::*;

    fn pdf_pages(n: usize) -> Vec<u8> {
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + i)).collect();
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 200 300] >>", kids.join(" ")),
        ];
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

    type Events = Rc<RefCell<Vec<ViewEvent>>>;

    fn app(with_doc: bool) -> (Harness<'static, PdfCraftApp>, Events) {
        let events: Events = Rc::default();
        let sink = events.clone();
        let mut harness = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(move |_| {
            let mut app = PdfCraftApp::new();
            if with_doc {
                app.open_bytes("a.pdf", None, pdf_pages(3)).unwrap();
            }
            app.host_view = Some(Box::new(move |e| sink.borrow_mut().push(e.clone())));
            app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
            app
        });
        harness.run_steps(3);
        (harness, events)
    }

    fn send(h: &mut Harness<'static, PdfCraftApp>, c: ViewCommand) {
        h.state().view_inbox.lock().unwrap().push(c);
        h.run_steps(2);
    }

    fn goto(id: &str, page: usize, rect: Option<[f64; 4]>) -> ViewCommand {
        ViewCommand::Goto { id: id.into(), page, rect }
    }

    fn highlight(id: &str, page: usize, transient: bool) -> ViewCommand {
        ViewCommand::Highlight { id: id.into(), page, rects: vec![[10.0, 20.0, 50.0, 12.0]], color: Some("#ff0000".into()), transient }
    }

    #[test]
    fn goto_moves_to_the_page_and_reports() {
        let (mut h, ev) = app(true);
        send(&mut h, goto("a", 2, None));
        assert_eq!(h.state().views[0].current, 2);
        send(&mut h, goto("b", 1, Some([10.0, 10.0, 20.0, 20.0])));
        assert_eq!(h.state().views[0].current, 1);
        send(&mut h, goto("c", 9, None));
        send(&mut h, goto("d", 0, Some([f64::NAN, 0.0, 1.0, 1.0])));
        send(&mut h, goto("", 0, None));
        let got = ev.borrow().clone();
        assert_eq!(got[0], ViewEvent::GotoDone { id: "a".into() });
        assert_eq!(got[1], ViewEvent::GotoDone { id: "b".into() });
        for (event, id) in got[2..].iter().zip(["c", "d", ""]) {
            assert!(matches!(event, ViewEvent::GotoFailed { id: i, .. } if i == id), "{event:?}");
        }
        assert_eq!(h.state().views[0].current, 1, "a refused goto moves nothing");
    }

    #[test]
    fn goto_without_a_document_fails() {
        let (mut h, ev) = app(false);
        send(&mut h, goto("a", 0, None));
        send(&mut h, highlight("b", 0, true));
        let got = ev.borrow().clone();
        assert!(matches!(&got[0], ViewEvent::GotoFailed { error, .. } if error.contains("no document")));
        assert!(matches!(&got[1], ViewEvent::HighlightFailed { error, .. } if error.contains("no document")));
    }

    #[test]
    fn a_transient_highlight_is_an_overlay_that_the_next_one_or_clear_removes() {
        let (mut h, ev) = app(true);
        send(&mut h, highlight("a", 1, true));
        let shown = h.state().views[0].host_highlight.clone().expect("highlight set");
        assert_eq!((shown.page, shown.rects.clone()), (1, vec![[10.0, 20.0, 60.0, 32.0]]));
        assert_eq!(shown.color, Color32::from_rgba_unmultiplied(255, 0, 0, 100));
        // An overlay is no edit: nothing to save and nothing in undo.
        assert!(!h.state().views.is_empty());
        let (_, doc) = h.state().active_ids().unwrap();
        let (generation, undo) = {
            let d = h.state().session.get(doc).unwrap();
            (d.edit_generation(), d.can_undo().map(str::to_string))
        };
        assert_eq!(undo, None, "a highlight is no edit");
        send(&mut h, highlight("b", 2, true));
        assert_eq!(h.state().session.get(doc).unwrap().edit_generation(), generation, "the document is untouched");
        assert_eq!(h.state().views[0].host_highlight.as_ref().unwrap().page, 2, "the next highlight replaces it");
        send(&mut h, ViewCommand::ClearHighlight);
        assert!(h.state().views[0].host_highlight.is_none());
        // Refusals leave what is there alone.
        send(&mut h, highlight("c", 0, true));
        send(&mut h, highlight("d", 0, false));
        send(&mut h, highlight("e", 7, true));
        send(&mut h, ViewCommand::Highlight { id: "f".into(), page: 0, rects: vec![], color: Some("red".into()), transient: true });
        assert_eq!(h.state().views[0].host_highlight.as_ref().unwrap().page, 0);
        let got = ev.borrow().clone();
        assert_eq!(got[0], ViewEvent::HighlightDone { id: "a".into() });
        assert_eq!(got[1], ViewEvent::HighlightDone { id: "b".into() });
        assert_eq!(got[2], ViewEvent::HighlightDone { id: "c".into() });
        for (event, id) in got[3..].iter().zip(["d", "e", "f"]) {
            assert!(matches!(event, ViewEvent::HighlightFailed { id: i, .. } if i == id), "{event:?}");
        }
    }

    #[test]
    fn opened_comes_once_after_the_first_page_is_drawn() {
        let (mut h, ev) = app(false);
        h.state().startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf_pages(2)));
        for _ in 0..60 {
            h.run_steps(1);
            if !ev.borrow().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        h.run_steps(5);
        assert_eq!(ev.borrow().as_slice(), [ViewEvent::Opened], "exactly one opened");
        assert!(h.state().views[0].first_painted);
        // A file opened behind the one showing is not drawn, so it is not "opened" yet.
        h.state().startup_inbox.lock().unwrap().push(("b.pdf".into(), pdf_pages(1)));
        h.run_steps(20);
        assert_eq!(h.state().views.len(), 2);
        assert_eq!(ev.borrow().as_slice(), [ViewEvent::Opened]);
        assert!(h.state().opened_wait.is_some());
    }

    #[test]
    fn a_failed_open_reports_no_opened() {
        let (mut h, ev) = app(false);
        h.state().startup_inbox.lock().unwrap().push(("bad.pdf".into(), b"not a pdf".to_vec()));
        h.run_steps(10);
        assert!(ev.borrow().is_empty());
        assert!(h.state().opened_wait.is_none(), "it stops waiting");
    }
}
