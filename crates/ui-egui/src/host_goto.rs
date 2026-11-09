//! PPM: `pdfcraft:goto` from the embedding page — show a page, and a rectangle on it, of the document
//! the host opened. The page is told `goto-done` or `goto-failed` (`not-found` · `error`), as the
//! office apps answer their `goto`. Pages count from 1 in the message (as `annots-changed`), from 0 here.

use std::sync::{Arc, Mutex};

/// A request from the host. `page` counts from 0; `rect` is in user space (PDF points, origin
/// bottom-left: `[x0, y0, x1, y1]`), the space XFDF `rect` uses.
#[derive(Clone, Debug, PartialEq)]
pub struct GotoCommand {
    pub id: String,
    pub page: usize,
    pub rect: Option<[f64; 4]>,
}

/// Why a `goto` did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GotoFail {
    /// No document is open, or the page is past the end.
    NotFound,
    /// The request could not be carried out (a view without page information).
    Error,
}

impl GotoFail {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not-found",
            Self::Error => "error",
        }
    }
}

/// What the host is told.
#[derive(Clone, Debug, PartialEq)]
pub enum GotoEvent {
    Done { id: String },
    Failed { id: String, reason: GotoFail },
}

impl GotoEvent {
    /// The message the page gets (`type` is added by the bridge).
    pub fn to_message(&self) -> (&'static str, serde_json::Value) {
        use serde_json::json;
        match self {
            Self::Done { id } => ("pdfcraft:goto-done", json!({ "id": id })),
            Self::Failed { id, reason } => ("pdfcraft:goto-failed", json!({ "id": id, "reason": reason.as_str() })),
        }
    }
}

/// Requests from the host, drained every frame.
pub type GotoInbox = Arc<Mutex<Vec<GotoCommand>>>;
/// Hands `goto` results to the embedding page (web).
pub type HostGotoFn = Box<dyn Fn(&GotoEvent)>;

/// An id the page sent; longer ones are cut, as the other host ids are.
const ID_MAX: usize = 128;
/// Pages past this are refused before any lookup (a `page` of 1e18 is not a page).
const PAGE_MAX: f64 = 1_000_000.0;

impl GotoCommand {
    /// Read a `pdfcraft:goto` message. `location` is `{ kind: "pdf", page, rect? }` with `page` from 1
    /// and `rect` four finite numbers; a location of another kind, or none, is an error.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let id = v.get("id").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).ok_or("id must be a non-empty string")?;
        let id: String = id.chars().take(ID_MAX).collect();
        let loc = v.get("location").ok_or("location must be an object")?;
        if loc.get("kind").and_then(|k| k.as_str()) != Some("pdf") {
            return Err("location.kind must be \"pdf\"".into());
        }
        let page = loc.get("page").and_then(|p| p.as_f64()).ok_or("location.page must be a number")?;
        if !(page.is_finite() && page >= 1.0 && page.fract() == 0.0 && page <= PAGE_MAX) {
            return Err("location.page must be a whole number from 1".into());
        }
        let rect = match loc.get("rect") {
            None | Some(serde_json::Value::Null) => None,
            Some(r) => {
                let nums: Vec<f64> =
                    r.as_array().ok_or("location.rect must be an array of 4 numbers")?.iter().map(|n| n.as_f64().unwrap_or(f64::NAN)).collect();
                if nums.len() != 4 || nums.iter().any(|n| !n.is_finite()) {
                    return Err("location.rect must be an array of 4 finite numbers".into());
                }
                // Any corner order is accepted; the view takes the box they span.
                Some([nums[0].min(nums[2]), nums[1].min(nums[3]), nums[0].max(nums[2]), nums[1].max(nums[3])])
            }
        };
        // `page` is whole and at most PAGE_MAX, so the cast is exact; from 1 in the message, from 0 here.
        Ok(Self { id, page: page as usize - 1, rect })
    }
}

impl crate::PdfCraftApp {
    /// Carry out the host's `goto` requests on the active document, in order, and report each.
    pub(crate) fn host_goto_tick(&mut self) {
        let commands: Vec<GotoCommand> = self.goto_inbox.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        if commands.is_empty() {
            return;
        }
        for c in commands {
            let outcome = self.host_goto(&c);
            if let Some(report) = self.host_goto.as_ref() {
                report(&match outcome {
                    Ok(()) => GotoEvent::Done { id: c.id },
                    Err(reason) => GotoEvent::Failed { id: c.id, reason },
                });
            }
        }
    }

    fn host_goto(&mut self, c: &GotoCommand) -> Result<(), GotoFail> {
        // The host opens one document at a time (`replace`); its requests go to the active one.
        let index = self.active.filter(|i| *i < self.views.len()).ok_or(GotoFail::NotFound)?;
        let id = self.views.get(index).map(|v| v.id).ok_or(GotoFail::NotFound)?;
        let doc = self.session.get(id).ok_or(GotoFail::NotFound)?;
        if c.page >= doc.info.pages.len() {
            return Err(GotoFail::NotFound);
        }
        let view = self.views.get_mut(index).ok_or(GotoFail::Error)?;
        match c.rect {
            Some(r) => {
                // f64 → f32 only loses precision here; PDF coordinates are well inside f32's range.
                let rect = [r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32];
                view.go_to_rect(c.page, rect, &doc.info);
                // The same brief highlight a search hit or a Comments-panel click gets.
                view.flash = Some((c.page, rect, 0.0));
            }
            None => view.go_to_page(c.page),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_page_location_is_read_from_1_and_kept_from_0() {
        let c = GotoCommand::from_json(&json!({ "id": "g1", "location": { "kind": "pdf", "page": 9 } })).unwrap();
        assert_eq!(c, GotoCommand { id: "g1".into(), page: 8, rect: None });
    }

    #[test]
    fn a_rect_is_normalised_to_min_max_corners() {
        let c = GotoCommand::from_json(&json!({ "id": "g", "location": { "kind": "pdf", "page": 1, "rect": [900.0, 500.0, 30.0, 20.0] } })).unwrap();
        assert_eq!(c.rect, Some([30.0, 20.0, 900.0, 500.0]));
    }

    #[test]
    fn malformed_locations_are_refused_not_panicked_on() {
        for bad in [
            json!({ "id": "g", "location": { "kind": "docx", "page": 1 } }),
            json!({ "id": "g", "location": { "kind": "pdf", "page": 0 } }),
            json!({ "id": "g", "location": { "kind": "pdf", "page": 1.5 } }),
            json!({ "id": "g", "location": { "kind": "pdf", "page": 1e18 } }),
            json!({ "id": "g", "location": { "kind": "pdf", "page": 1, "rect": [1, 2, 3] } }),
            json!({ "id": "g", "location": { "kind": "pdf", "page": 1, "rect": [1, 2, 3, "x"] } }),
            json!({ "id": "", "location": { "kind": "pdf", "page": 1 } }),
            json!({ "location": { "kind": "pdf", "page": 1 } }),
            json!({ "id": "g" }),
            json!(null),
        ] {
            assert!(GotoCommand::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_messages_carry_the_id_and_the_reason() {
        assert_eq!(GotoEvent::Done { id: "a".into() }.to_message(), ("pdfcraft:goto-done", json!({ "id": "a" })));
        assert_eq!(
            GotoEvent::Failed { id: "a".into(), reason: GotoFail::NotFound }.to_message(),
            ("pdfcraft:goto-failed", json!({ "id": "a", "reason": "not-found" }))
        );
    }

    /// With no document open every request fails with `not-found`, and the inbox is left empty.
    #[test]
    fn without_a_document_the_request_is_refused_and_reported() {
        let mut app = crate::PdfCraftApp::new();
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = got.clone();
        app.host_goto = Some(Box::new(move |e| sink.lock().unwrap().push(e.clone())));
        app.goto_inbox.lock().unwrap().push(GotoCommand { id: "x".into(), page: 0, rect: None });
        app.host_goto_tick();
        assert_eq!(*got.lock().unwrap(), vec![GotoEvent::Failed { id: "x".into(), reason: GotoFail::NotFound }]);
        assert!(app.goto_inbox.lock().unwrap().is_empty());
    }
}
