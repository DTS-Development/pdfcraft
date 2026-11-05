//! Annotation sharing with the embedding page (web `annotSync`, `pdfcraft:annots-*`).
//!
//! The engine does the work (`Session::take_annot_changes`, `annots_reset`…); this module carries
//! the page's messages in, the user's changes and selection out, and keeps the canvas selection
//! pointing at the same annotation when a remote patch moves the indices.

use std::sync::Arc;

use pdfcraft_engine::{AnnotChanges, AnnotOp, DocId, RejectedAnnot, RemoteAnnot};

use crate::PdfCraftApp;

/// What the host asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum AnnotCommand {
    /// `seq`: the last `annots-changed` number the page had received when it fetched the list.
    Reset {
        items: Vec<RemoteAnnot>,
        seq: u64,
    },
    Apply {
        upserts: Vec<RemoteAnnot>,
        deletes: Vec<String>,
    },
    Reject {
        items: Vec<RejectedAnnot>,
    },
    Select {
        annot_id: String,
    },
}

/// What the host is told.
#[derive(Clone, Debug, PartialEq)]
pub enum AnnotEvent {
    /// One `annots-changed` message; `seq` counts them from 1 for each document the page opens.
    Changed {
        seq: u64,
        changes: AnnotChanges,
    },
    Selected(Option<String>),
}

/// Requests from the host, drained every frame once the shared document is open.
pub type AnnotInbox = Arc<std::sync::Mutex<Vec<AnnotCommand>>>;

/// Hands annotation changes and selections to the embedding page (web).
pub type HostAnnotFn = Box<dyn Fn(&AnnotEvent)>;

/// A message's `xfdf` is one annotation; this bounds what a page can make the app parse.
// A stamp's picture (at most 4 MB) travels inside it as base64, a third larger.
const MAX_ITEM_BYTES: usize = 8 << 20;

fn item(v: &serde_json::Value) -> Result<RemoteAnnot, String> {
    let text = |key: &str| v.get(key).and_then(|x| x.as_str()).ok_or_else(|| format!("{key} must be a string"));
    let xfdf = text("xfdf")?;
    if xfdf.len() > MAX_ITEM_BYTES {
        return Err("an annotation is too large".into());
    }
    let (annot_id, author_id) = (text("annotId")?, text("authorId")?);
    if annot_id.is_empty() || author_id.is_empty() {
        return Err("annotId and authorId can't be empty".into());
    }
    Ok(RemoteAnnot { annot_id: annot_id.into(), author_id: author_id.into(), xfdf: xfdf.into() })
}

fn items<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a [serde_json::Value], String> {
    v.get(key).and_then(|x| x.as_array()).map(Vec::as_slice).ok_or_else(|| format!("{key} must be an array"))
}

/// The items that read well; one that doesn't is left out and logged, so a single bad annotation
/// doesn't keep the rest of the server's list from showing.
fn good<T>(list: &[serde_json::Value], read: impl Fn(&serde_json::Value) -> Result<T, String>) -> Vec<T> {
    list.iter()
        .filter_map(|v| match read(v) {
            Ok(x) => Some(x),
            Err(e) => {
                log::warn!("an annotation from the host was left out: {e}");
                None
            }
        })
        .collect()
}

impl AnnotCommand {
    /// Read a `pdfcraft:annots-*` message (`kind` is the part after `annots-`) from its JSON. A
    /// malformed envelope is an error; a malformed item is skipped.
    pub fn from_json(kind: &str, v: &serde_json::Value) -> Result<Self, String> {
        match kind {
            "reset" => Ok(Self::Reset { items: good(items(v, "items")?, item), seq: v.get("seq").and_then(|x| x.as_u64()).unwrap_or(0) }),
            "apply" => {
                let deletes = good(items(v, "deletes")?, |d| d.as_str().map(str::to_string).ok_or_else(|| "a delete must be a string".to_string()));
                Ok(Self::Apply { upserts: good(items(v, "upserts")?, item), deletes })
            }
            "reject" => {
                let one = |r: &serde_json::Value| {
                    let annot_id = r.get("annotId").and_then(|x| x.as_str()).filter(|a| !a.is_empty()).ok_or("annotId must be a string")?;
                    let server = match r.get("server") {
                        None | Some(serde_json::Value::Null) => None,
                        Some(s) => Some(item(s)?),
                    };
                    Ok::<_, String>(RejectedAnnot { annot_id: annot_id.into(), server })
                };
                Ok(Self::Reject { items: good(items(v, "items")?, one) })
            }
            "select" => match v.get("annotId").and_then(|x| x.as_str()) {
                Some(annot_id) => Ok(Self::Select { annot_id: annot_id.into() }),
                None => Err("annotId must be a string".into()),
            },
            other => Err(format!("unknown annotation message {other}")),
        }
    }
}

/// The page takes at most this many upserts and this many deletes in one `annots-changed`.
const MAX_PER_MESSAGE: usize = 2000;

/// The longest id the page accepts, counted in UTF-16 units like its `.length`.
const MAX_ID_UNITS: usize = 200;

/// A change cut to what the page takes in one `annots-changed`: a delete id it can't take (it drops
/// the whole message for one) is left out, and the engine's order is kept, so a reply stays behind
/// the annotation it replies to.
pub(crate) fn split_changes(c: AnnotChanges) -> Vec<AnnotChanges> {
    let deletes: Vec<String> = c.deletes.into_iter().filter(|d| (1..=MAX_ID_UNITS).contains(&d.encode_utf16().count())).collect();
    let parts = c.upserts.len().div_ceil(MAX_PER_MESSAGE).max(deletes.len().div_ceil(MAX_PER_MESSAGE)).max(1);
    let mut upserts = c.upserts.into_iter();
    let mut deletes = deletes.into_iter();
    (0..parts)
        .map(|_| AnnotChanges {
            upserts: upserts.by_ref().take(MAX_PER_MESSAGE).collect(),
            deletes: deletes.by_ref().take(MAX_PER_MESSAGE).collect(),
        })
        .collect()
}

impl AnnotEvent {
    /// The message the page gets (`type` is added by the bridge): `annots-changed` or
    /// `annots-selected`, as a list for the bridge. `page` counts from 1, as the server's.
    pub fn to_messages(&self) -> Vec<(&'static str, serde_json::Value)> {
        use serde_json::json;
        match self {
            Self::Changed { seq, changes: c } => {
                let upserts: Vec<_> =
                    c.upserts.iter().map(|u| json!({"annotId": u.annot_id, "page": u.page + 1, "type": u.subtype, "xfdf": u.xfdf})).collect();
                vec![("pdfcraft:annots-changed", json!({ "seq": seq, "upserts": upserts, "deletes": c.deletes }))]
            }
            Self::Selected(id) => vec![("pdfcraft:annots-selected", json!({ "annotId": id }))],
        }
    }
}

impl PdfCraftApp {
    /// The active document's host asked to hide this interface's comments panel.
    pub(crate) fn comments_panel_hidden(&self) -> bool {
        self.active_ids().and_then(|(_, id)| self.session.get(id)).is_some_and(|d| d.ownership().hide_comments_panel)
    }

    /// The active document is kept in the page's database (`annotSync`).
    pub(crate) fn active_shares_annotations(&self) -> bool {
        self.active_ids().and_then(|(_, id)| self.session.get(id)).is_some_and(|d| d.annot_sync())
    }

    /// A file the host opened is still on its way in: its `annots-*` messages belong to it.
    pub(crate) fn host_open_pending(&self) -> bool {
        self.startup_inbox.lock().map(|q| !q.is_empty()).unwrap_or(false)
    }

    /// A file opened on a worker thread has not come in yet.
    pub(crate) fn open_in_flight(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            !self.pending_opens.is_empty()
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }

    /// Once a frame, after the frame's edits: report what the user changed, then apply the host's
    /// annotation messages, and report which annotation is selected. The user's changes go first so
    /// the page sees them before it sees its own news acted on.
    pub(crate) fn host_annots_tick(&mut self) {
        // The messages wait for the document they belong to (the page sends them right behind
        // `pdfcraft:open`): not for an earlier one, nor while a password is still asked for.
        if self.annot_open_waiting {
            if self.password_prompt.is_some() || self.open_in_flight() {
                return;
            }
            self.adopt_annot_target();
            // Nothing left to wait for and no shared document came: the open did not produce one.
            if self.annot_open_waiting {
                self.annot_open_finished(false);
                return;
            }
        }
        let Some(doc) = self.annot_target.filter(|d| self.session.get(*d).is_some_and(|d| d.annot_sync())) else {
            self.annot_target = None;
            return;
        };
        if self.host_open_pending() {
            return;
        }
        self.post_annot_changes(doc);
        let commands: Vec<AnnotCommand> = self.annot_inbox.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        self.run_annot_commands(doc, commands);
        if self.host_annots.is_some() {
            self.report_annot_selection(doc);
        }
    }

    /// Hand the user's changes to the page, and tell the user of annotations that can't be kept there.
    fn post_annot_changes(&mut self, doc: DocId) {
        if self.host_annots.is_some()
            && let Some(changes) = self.session.take_annot_changes(doc)
            && !changes.is_empty()
        {
            for part in split_changes(changes) {
                self.annot_seq += 1;
                // A reset the page fetched before it had this message must not undo it.
                let names = part.upserts.iter().map(|u| u.annot_id.clone()).chain(part.deletes.iter().cloned()).collect();
                self.session.record_annot_sent(doc, self.annot_seq, names);
                if let Some(report) = self.host_annots.as_ref() {
                    report(&AnnotEvent::Changed { seq: self.annot_seq, changes: part });
                }
            }
        }
        let problems = self.session.take_annot_problems(doc);
        if !problems.is_empty() {
            self.notify_fmt("{n} annotations can't be saved to the server (too large or not supported)", &[("n", &problems.len().to_string())]);
        }
    }

    /// The newest shared document becomes the one the messages go to, if it is a new one.
    fn adopt_annot_target(&mut self) {
        let newest = self.session.annot_sync_doc();
        if newest.is_some() && newest != self.annot_target {
            self.annot_target = newest;
            self.annot_open_waiting = false;
            self.annot_selected_sent = None;
            self.annot_seq = 0;
        }
    }

    /// A file the host opened with `annotSync` went through the open: its messages follow it, or
    /// (open failed) are dropped, never applied to the document that was open before.
    pub(crate) fn annot_open_finished(&mut self, ok: bool) {
        self.annot_open_waiting = ok;
        if ok {
            self.adopt_annot_target();
        } else {
            self.annot_target = None;
            if let Ok(mut q) = self.annot_inbox.lock() {
                q.clear();
            }
        }
    }

    /// Apply the messages in order; a run of them between two selections is one patch with one refresh.
    fn run_annot_commands(&mut self, doc: DocId, commands: Vec<AnnotCommand>) {
        let mut ops: Vec<AnnotOp> = Vec::new();
        for command in commands {
            match command {
                AnnotCommand::Reset { items, seq } => ops.push(AnnotOp::Reset { items, seq }),
                AnnotCommand::Apply { upserts, deletes } => ops.push(AnnotOp::Apply { upserts, deletes }),
                AnnotCommand::Reject { items } => ops.push(AnnotOp::Reject(items)),
                AnnotCommand::Select { annot_id } => {
                    self.run_annot_ops(doc, std::mem::take(&mut ops));
                    self.select_annot(doc, &annot_id);
                }
            }
        }
        self.run_annot_ops(doc, ops);
    }

    fn run_annot_ops(&mut self, doc: DocId, ops: Vec<AnnotOp>) {
        if ops.is_empty() {
            return;
        }
        // Remote patches shift (page, index): the selection, the open inline edit, a drag and an
        // open composer follow their annotation by `/NM`, or go when it is gone.
        let name_of = |app: &Self, at: Option<(usize, usize)>| at.and_then(|(p, i)| app.annot_name(doc, p, i));
        let view = self.view_of(doc);
        let selected = name_of(self, view.and_then(|v| v.comments.selected));
        let editing = view.and_then(|v| v.comments.editing.as_ref().map(|(p, i, t)| (*p, *i, t.clone())));
        let editing_name = name_of(self, editing.as_ref().map(|(p, i, _)| (*p, *i)));
        let dragged = name_of(
            self,
            match view.and_then(|v| v.comments.gesture.as_ref()) {
                Some(crate::comments::Gesture::Move { page, index, .. } | crate::comments::Gesture::Resize { page, index, .. }) => {
                    Some((*page, *index))
                }
                _ => None,
            },
        );
        let composing = name_of(
            self,
            match view.and_then(|v| v.comments.composer.as_ref()) {
                Some(c) => match c.kind {
                    crate::comments::ComposerKind::Edit(i) => Some((c.page, i)),
                    _ => None,
                },
                None => None,
            },
        );
        // The pages to draw again; `None` is every page.
        let mut redraw: Option<std::collections::BTreeSet<usize>> = Some(Default::default());
        let generation = self.session.get(doc).map(|d| d.edit_generation());
        let changed = match self.session.annots_run(doc, ops) {
            Ok(reports) => {
                for report in &reports {
                    for (annot_id, why) in &report.failed {
                        log::warn!("annotation {annot_id} from the host was not applied: {why}");
                    }
                }
                for report in &reports {
                    match (&mut redraw, report.pages_unknown) {
                        (Some(pages), false) => pages.extend(&report.pages),
                        _ => redraw = None,
                    }
                }
                reports.iter().any(|r| r.applied > 0)
            }
            Err(e) => {
                log::warn!("annotation message refused: {e}");
                // A refusal that changed nothing (the generation is the same) draws nothing again;
                // one that came after some messages were applied does not say which pages.
                redraw = None;
                self.session.get(doc).map(|d| d.edit_generation()) != generation
            }
        };
        // A message that put in or took away nothing moved nothing.
        if !changed {
            return;
        }
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
        let position = |app: &Self, name: &Option<String>| name.as_deref().and_then(|n| app.session.get(doc)?.annot_position(n));
        let (selected_at, editing_at) = (position(self, &selected), position(self, &editing_name));
        let (dragged_at, composing_at) = (position(self, &dragged), position(self, &composing));
        if let Some(view) = self.views.iter_mut().find(|v| v.id == doc) {
            // What is on screen was drawn before the message: draw the pages again.
            view.annotations_changed_remotely(redraw.as_ref());
            view.comments.selected = selected_at;
            view.comments.editing = match (editing, editing_at) {
                (Some((_, _, text)), Some((p, i))) => Some((p, i, text)),
                _ => None,
            };
            if selected_at.is_none() {
                view.comments.reply.clear();
            }
            match (view.comments.gesture.as_mut(), dragged_at) {
                (Some(crate::comments::Gesture::Move { page, index, .. } | crate::comments::Gesture::Resize { page, index, .. }), Some((p, i))) => {
                    (*page, *index) = (p, i);
                }
                (Some(crate::comments::Gesture::Move { .. } | crate::comments::Gesture::Resize { .. }), None) => view.comments.gesture = None,
                _ => {}
            }
            if let Some(c) = view.comments.composer.as_mut()
                && matches!(c.kind, crate::comments::ComposerKind::Edit(_))
            {
                match composing_at {
                    Some((p, i)) => (c.page, c.kind) = (p, crate::comments::ComposerKind::Edit(i)),
                    None => view.comments.composer = None,
                }
            }
        }
    }

    fn view_of(&self, doc: DocId) -> Option<&crate::canvas::DocView> {
        self.views.iter().find(|v| v.id == doc)
    }

    fn annot_name(&self, doc: DocId, page: usize, index: usize) -> Option<String> {
        self.session.get(doc)?.annot_name_at(page, index)
    }

    /// `annots-select`: go to the annotation's page and select it. An id this document doesn't have
    /// reports "nothing selected", so the page's card doesn't stay lit.
    fn select_annot(&mut self, doc: DocId, annot_id: &str) {
        let at = self.session.get(doc).and_then(|d| d.annot_position(annot_id));
        let Some(view) = self.views.iter_mut().find(|v| v.id == doc) else { return };
        match at {
            Some((page, index)) => {
                view.go_to_page(page);
                view.comments.selected = Some((page, index));
                view.comments.reveal = true;
            }
            None => view.comments.selected = None,
        }
    }

    /// Tell the page when the canvas selection changed (once per change).
    fn report_annot_selection(&mut self, doc: DocId) {
        let selected = self.view_of(doc).and_then(|v| v.comments.selected).and_then(|(p, i)| self.annot_name(doc, p, i));
        if self.annot_selected_sent.as_ref() == Some(&selected) {
            return;
        }
        if let Some(report) = self.host_annots.as_ref() {
            report(&AnnotEvent::Selected(selected.clone()));
        }
        self.annot_selected_sent = Some(selected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_every_message_shape() {
        let it = json!({"annotId": "a", "authorId": "u", "xfdf": "<x/>"});
        let reset = AnnotCommand::from_json("reset", &json!({"items": [it]})).unwrap();
        assert_eq!(
            reset,
            AnnotCommand::Reset { seq: 0, items: vec![RemoteAnnot { annot_id: "a".into(), author_id: "u".into(), xfdf: "<x/>".into() }] }
        );
        let apply = AnnotCommand::from_json("apply", &json!({"upserts": [], "deletes": ["z"]})).unwrap();
        assert_eq!(apply, AnnotCommand::Apply { upserts: vec![], deletes: vec!["z".into()] });
        let reject =
            AnnotCommand::from_json("reject", &json!({"items": [{"annotId": "a", "server": null}, {"annotId": "b", "server": it}]})).unwrap();
        let AnnotCommand::Reject { items } = reject else { panic!("not a reject") };
        assert!(items[0].server.is_none() && items[1].server.is_some());
        assert_eq!(AnnotCommand::from_json("select", &json!({"annotId": "a"})).unwrap(), AnnotCommand::Select { annot_id: "a".into() });
    }

    #[test]
    fn refuses_wrong_shapes() {
        assert!(AnnotCommand::from_json("reset", &json!({})).is_err());
        assert!(AnnotCommand::from_json("reset", &json!({"items": "no"})).is_err());
        assert!(AnnotCommand::from_json("apply", &json!({"upserts": []})).is_err());
        assert!(AnnotCommand::from_json("select", &json!({"annotId": 3})).is_err());
        assert!(AnnotCommand::from_json("nope", &json!({})).is_err());
    }

    #[test]
    fn a_bad_item_is_left_out_and_the_rest_stay() {
        let good = json!({"annotId": "a", "authorId": "u", "xfdf": "<x/>"});
        let bad = json!({"annotId": "b", "xfdf": "<x/>"});
        let AnnotCommand::Reset { items, .. } = AnnotCommand::from_json("reset", &json!({"items": [bad, good, 3]})).unwrap() else {
            panic!("not a reset")
        };
        assert_eq!(items.iter().map(|i| i.annot_id.as_str()).collect::<Vec<_>>(), ["a"]);
        let AnnotCommand::Apply { upserts, deletes } =
            AnnotCommand::from_json("apply", &json!({"upserts": [bad, good], "deletes": [1, "z"]})).unwrap()
        else {
            panic!("not an apply")
        };
        assert_eq!((upserts.len(), deletes), (1, vec!["z".to_string()]));
        let AnnotCommand::Reject { items } =
            AnnotCommand::from_json("reject", &json!({"items": [{"server": null}, {"annotId": "c", "server": bad}, {"annotId": "d"}]})).unwrap()
        else {
            panic!("not a reject")
        };
        assert_eq!(items.iter().map(|i| i.annot_id.as_str()).collect::<Vec<_>>(), ["d"]);
    }

    #[test]
    fn events_become_messages() {
        assert_eq!(AnnotEvent::Selected(None).to_messages(), vec![("pdfcraft:annots-selected", json!({"annotId": null}))]);
        let changes = AnnotChanges {
            upserts: vec![pdfcraft_engine::AnnotUpsert { annot_id: "a".into(), page: 2, subtype: "Text".into(), xfdf: "<x/>".into() }],
            deletes: vec!["d".into()],
        };
        let messages = AnnotEvent::Changed { seq: 7, changes }.to_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "pdfcraft:annots-changed");
        // The page counts from 1, the engine from 0.
        assert_eq!(messages[0].1, json!({"seq": 7, "upserts": [{"annotId": "a", "page": 3, "type": "Text", "xfdf": "<x/>"}], "deletes": ["d"]}));
        let reset = AnnotCommand::from_json("reset", &json!({"items": [], "seq": 9})).unwrap();
        assert_eq!(reset, AnnotCommand::Reset { items: vec![], seq: 9 });
    }

    /// One blank page.
    fn pdf() -> Vec<u8> {
        pdf_pages(1)
    }

    fn pdf_pages(n: usize) -> Vec<u8> {
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 200 300] >>", kids.join(" ")),
        ];
        objs.extend((0..n).map(|_| "<< /Type /Page /Parent 2 0 R >>".to_string()));
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    fn square(id: &str) -> RemoteAnnot {
        square_on(id, 0)
    }

    fn square_on(id: &str, page: usize) -> RemoteAnnot {
        let xfdf = format!(
            r#"<?xml version="1.0" encoding="UTF-8" ?><xfdf xmlns="http://ns.adobe.com/xfdf/" xml:space="preserve"><annots><square page="{page}" name="{id}" rect="10,10,60,60" title="Kim" flags="print"/></annots></xfdf>"#
        );
        RemoteAnnot { annot_id: id.into(), author_id: "u-kim".into(), xfdf }
    }

    fn share() -> pdfcraft_engine::Ownership {
        pdfcraft_engine::Ownership { actor: Some("alice".into()), annot_sync: true, ..Default::default() }
    }

    /// The page's messages go to the document they were sent for: after a second open fails, the
    /// first document is not the target of its list.
    #[test]
    fn messages_of_a_failed_open_are_not_applied_to_the_document_before() {
        let app = PdfCraftApp::new();
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let inbox = app.annot_inbox.clone();
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        let (_, a) = h.state().active_ids().unwrap();
        inbox.lock().unwrap().push(AnnotCommand::Reset { seq: 0, items: vec![square("first")] });
        h.run_steps(2);
        let at = |h: &egui_kittest::Harness<'_, PdfCraftApp>, nm: &str| h.state().session.get(a).unwrap().annot_position(nm);
        assert!(at(&h, "first").is_some(), "the target receives its list");

        // The second open fails; its list arrives right behind it and must not land on `a`.
        h.state().ownership_inbox.lock().unwrap().push(share());
        h.state().startup_inbox.lock().unwrap().push(("b.pdf".into(), b"not a pdf".to_vec()));
        inbox.lock().unwrap().push(AnnotCommand::Reset { seq: 0, items: vec![square("second")] });
        h.run_steps(4);
        assert!(at(&h, "second").is_none(), "a failed open's list is dropped");
        assert!(at(&h, "first").is_some(), "and the document is as it was");
        assert!(inbox.lock().unwrap().is_empty());
    }

    /// The page posts its list in the same frame as the open: the messages wait for the document
    /// and then apply in the order they came (the delete of the second message hits the first's item).
    #[test]
    fn messages_sent_with_the_open_wait_for_the_document_and_keep_their_order() {
        let app = PdfCraftApp::new();
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        {
            let mut q = app.annot_inbox.lock().unwrap();
            q.push(AnnotCommand::Reset { seq: 0, items: vec![square("one"), square("two")] });
            q.push(AnnotCommand::Apply { upserts: vec![square("three")], deletes: vec!["one".into()] });
            q.push(AnnotCommand::Select { annot_id: "three".into() });
        }
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        let (_, a) = h.state().active_ids().unwrap();
        let doc = h.state().session.get(a).unwrap();
        assert!(doc.annot_position("one").is_none(), "deleted by the second message");
        assert!(doc.annot_position("two").is_some() && doc.annot_position("three").is_some());
        let view = h.state().views.iter().find(|v| v.id == a).unwrap();
        assert_eq!(view.comments.selected, doc.annot_position("three"), "and the third selected it");
        assert!(h.state().annot_inbox.lock().unwrap().is_empty());
    }

    /// A shared document is not saved from here: no command, button or shortcut writes it. Another
    /// document (not shared) saves as before.
    #[test]
    fn a_shared_document_has_no_manual_save() {
        let mut app = PdfCraftApp::new();
        app.session.set_open_ownership(share());
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        assert!(!app.host_allows("file.save") && !app.host_allows("file.save_as"));
        assert!(!app.save_active(crate::SaveTarget::InPlace) && !app.save_active(crate::SaveTarget::As));
        app.session.set_open_ownership(Default::default());
        app.open_bytes("b.pdf", None, pdf()).unwrap();
        assert!(app.host_allows("file.save") && app.host_allows("file.save_as"), "a document that isn't shared saves as before");
    }

    /// The page's limits: a big change is split, a delete id it can't take is left out, replies
    /// stay behind their parent, and a page-0 annotation of the engine goes out as page 1.
    #[test]
    fn a_big_change_is_split_for_the_page() {
        let up = |i: usize| pdfcraft_engine::AnnotUpsert { annot_id: format!("a{i}"), page: 0, subtype: "Text".into(), xfdf: "<x/>".into() };
        let changes =
            AnnotChanges { upserts: (0..4500).map(up).collect(), deletes: vec!["ok".into(), String::new(), "x".repeat(201), "é".repeat(200)] };
        let parts = split_changes(changes);
        let sizes: Vec<usize> = parts.iter().map(|p| p.upserts.len()).collect();
        assert_eq!(sizes, [2000, 2000, 500]);
        assert_eq!(parts[0].deletes, ["ok", &"é".repeat(200)], "the empty and the 201-unit ids are left out");
        assert_eq!(parts[0].upserts[0].annot_id, "a0", "order is kept");
    }

    /// The reply order and the page number hold from the engine to the message.
    #[test]
    fn what_the_engine_hands_out_reaches_the_page_one_based_and_replies_last() {
        let mut app = PdfCraftApp::new();
        app.session.set_open_ownership(share());
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        let (_, id) = app.active_ids().unwrap();
        let add = |app: &mut PdfCraftApp, x: f64| {
            let shape = pdfcraft_engine::Shape::Rectangle { rect: [x, 20.0, x + 40.0, 60.0] };
            let style = pdfcraft_engine::Style::default_for(&shape);
            let new = pdfcraft_engine::NewAnnotation { page: 0, shape, style, contents: "c".into(), author: "alice".into() };
            app.session.apply(id, pdfcraft_engine::Edit::AddAnnotation(new)).unwrap();
        };
        add(&mut app, 10.0);
        app.session.apply(id, pdfcraft_engine::Edit::ReplyToAnnotation { page: 0, index: 0, text: "r".into(), author: "alice".into() }).unwrap();
        let changes = app.session.take_annot_changes(id).unwrap();
        let messages = AnnotEvent::Changed { seq: 1, changes }.to_messages();
        let ups = messages[0].1["upserts"].as_array().unwrap();
        assert!(ups.iter().all(|u| u["page"] == 1), "{ups:?}");
        assert!(!ups[0]["xfdf"].as_str().unwrap().contains("inreplyto") && ups[1]["xfdf"].as_str().unwrap().contains("inreplyto"));
    }

    /// Opening a shared file leaves nothing of the sharing behind: a file opened afterwards is a
    /// plain one, with its comments panel.
    #[test]
    fn the_sharing_does_not_carry_over_to_later_opens() {
        let app = PdfCraftApp::new();
        let own = pdfcraft_engine::Ownership { hide_comments_panel: true, ..share() };
        app.ownership_inbox.lock().unwrap().push(own);
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        let app = h.state_mut();
        let (_, shared) = app.active_ids().unwrap();
        assert!(app.session.get(shared).unwrap().annot_sync());
        app.open_bytes("b.pdf", None, pdf()).unwrap();
        let (_, plain) = app.active_ids().unwrap();
        assert_ne!(plain, shared);
        let d = app.session.get(plain).unwrap();
        assert!(!d.annot_sync() && !d.ownership().hide_comments_panel);
    }

    /// Waiting for a shared open that produced no shared document ends: its messages are dropped
    /// and the document that was open before is not touched.
    #[test]
    fn waiting_for_an_open_that_never_comes_ends() {
        let app = PdfCraftApp::new();
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let inbox = app.annot_inbox.clone();
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        let target = h.state().annot_target;
        assert!(target.is_some());
        h.state_mut().annot_open_waiting = true;
        // An open that adopts nothing new: the same document is still the newest shared one.
        inbox.lock().unwrap().push(AnnotCommand::Reset { seq: 0, items: vec![square("late")] });
        h.run_steps(2);
        assert!(inbox.lock().unwrap().is_empty());
        assert!(!h.state().annot_open_waiting);
        let (_, id) = h.state().active_ids().unwrap();
        assert!(h.state().session.get(id).unwrap().annot_position("late").is_none(), "dropped, not applied");
    }

    /// A message that changes nothing leaves a drag and an open edit alone; one that moves things
    /// makes them follow their annotation.
    #[test]
    fn a_patch_that_changes_nothing_keeps_the_drag_and_the_composer() {
        let app = PdfCraftApp::new();
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let inbox = app.annot_inbox.clone();
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        let (_, id) = h.state().active_ids().unwrap();
        inbox.lock().unwrap().push(AnnotCommand::Reset { seq: 0, items: vec![square("one")] });
        h.run_steps(2);
        let gesture = crate::comments::Gesture::Move { page: 0, index: 0, from: egui::Pos2::ZERO };
        let composer =
            crate::comments::Composer { page: 0, at: [0.0; 2], kind: crate::comments::ComposerKind::Edit(0), text: "typing".into(), focus: false };
        let view = h.state_mut().views.iter_mut().find(|v| v.id == id).unwrap();
        view.comments.gesture = Some(gesture.clone());
        view.comments.composer = Some(composer.clone());
        // The same list again: nothing to put in.
        inbox.lock().unwrap().push(AnnotCommand::Reset { seq: 0, items: vec![square("one")] });
        h.run_steps(2);
        let view = h.state().views.iter().find(|v| v.id == id).unwrap();
        assert_eq!(view.comments.gesture, Some(gesture));
        assert_eq!(view.comments.composer, Some(composer));
        // The annotation goes away: so do they.
        inbox.lock().unwrap().push(AnnotCommand::Apply { upserts: vec![], deletes: vec!["one".into()] });
        h.run_steps(2);
        let view = h.state().views.iter().find(|v| v.id == id).unwrap();
        assert!(view.comments.gesture.is_none() && view.comments.composer.is_none());
    }

    /// A host file behind a password keeps its rules: the prompt holds up the open, and the rules
    /// of the host must still be there when the password lets it in.
    #[test]
    fn a_password_protected_host_file_keeps_its_rules() {
        let mut doc = pdfcraft_cos::Document::open(std::sync::Arc::new(pdf())).unwrap();
        doc.set_encryption(&pdfcraft_cos::NewEncryption {
            algorithm: pdfcraft_cos::Algorithm::Aes256,
            user_password: "pw",
            owner_password: "owner",
            permissions: -1,
            encrypt_metadata: true,
            seed: [4; 32],
        })
        .unwrap();
        let locked = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        let app = PdfCraftApp::new();
        app.ownership_inbox.lock().unwrap().push(pdfcraft_engine::Ownership { read_only: true, ..share() });
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), locked));
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        for _ in 0..50 {
            h.run_steps(2);
            if h.state().password_prompt.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(h.state().password_prompt.is_some());
        h.run_steps(3);
        h.state_mut().submit_password(Some("pw".into()));
        for _ in 0..100 {
            h.run_steps(2);
            if h.state().active_ids().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (_, id) = h.state().active_ids().expect("the file opened");
        let d = h.state().session.get(id).unwrap();
        assert_eq!((d.ownership().actor.as_deref(), d.ownership().read_only, d.annot_sync()), (Some("alice"), true, true));
    }

    /// `pdf()` behind the user password "pw".
    fn locked_pdf() -> Vec<u8> {
        let mut doc = pdfcraft_cos::Document::open(std::sync::Arc::new(pdf())).unwrap();
        doc.set_encryption(&pdfcraft_cos::NewEncryption {
            algorithm: pdfcraft_cos::Algorithm::Aes256,
            user_password: "pw",
            owner_password: "owner",
            permissions: -1,
            encrypt_metadata: true,
            seed: [4; 32],
        })
        .unwrap();
        pdfcraft_cos::write_full(&doc, &Default::default()).unwrap()
    }

    fn rules_of(h: &egui_kittest::Harness<'_, PdfCraftApp>, name: &str) -> (Option<String>, bool, bool) {
        let state = h.state();
        let d = state.views.iter().filter_map(|v| state.session.get(v.id)).find(|d| d.name == name).unwrap_or_else(|| {
            panic!("{name} is not open: {:?}", state.views.iter().filter_map(|v| state.session.get(v.id)).map(|d| d.name.clone()).collect::<Vec<_>>())
        });
        (d.ownership().actor.clone(), d.ownership().read_only, d.annot_sync())
    }

    /// The rules the answer to a password prompt makes the document with are the ones the file came
    /// with, not whatever the session holds by the time the password is in.
    #[test]
    fn a_password_prompt_carries_the_rules_of_its_file_whatever_the_session_holds_then() {
        let app = PdfCraftApp::new();
        app.ownership_inbox.lock().unwrap().push(pdfcraft_engine::Ownership { read_only: true, ..share() });
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), locked_pdf()));
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        for _ in 0..50 {
            h.run_steps(2);
            if h.state().password_prompt.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(h.state().password_prompt.is_some());
        h.state_mut().session.set_open_ownership(Default::default());
        h.state_mut().submit_password(Some("pw".into()));
        h.state_mut().session.set_open_ownership(pdfcraft_engine::Ownership { actor: Some("mallory".into()), ..Default::default() });
        for _ in 0..100 {
            h.run_steps(2);
            if h.state().active_ids().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(rules_of(&h, "a.pdf"), (Some("alice".into()), true, true));
        assert_eq!(h.state().session.open_ownership().actor.as_deref(), Some("mallory"), "the rules in force are put back");
    }

    /// Files opened in one frame each get the rules sent with them, and one sent without gets the
    /// build's own, not the one before it's.
    #[test]
    fn files_opened_in_one_frame_each_get_their_own_rules() {
        for host_lock in [false, true] {
            let mut app = PdfCraftApp::new();
            app.host_lock = host_lock;
            app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
            {
                let mut o = app.ownership_inbox.lock().unwrap();
                o.push(pdfcraft_engine::Ownership { read_only: true, ..share() });
                o.push(pdfcraft_engine::Ownership { actor: Some("bob".into()), ..Default::default() });
                let mut q = app.startup_inbox.lock().unwrap();
                for name in ["a.pdf", "b.pdf", "c.pdf"] {
                    q.push((name.into(), pdf()));
                }
            }
            let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
            h.run_steps(4);
            assert_eq!(rules_of(&h, "a.pdf"), (Some("alice".into()), true, true));
            assert_eq!(rules_of(&h, "b.pdf"), (Some("bob".into()), false, false));
            assert_eq!(rules_of(&h, "c.pdf"), (None, false, false), "host lock {host_lock}: no rules came with it");
            let c = h.state().views.iter().filter_map(|v| h.state().session.get(v.id)).find(|d| d.name == "c.pdf").unwrap();
            let want = if host_lock { crate::host_lock::viewer_ownership().scope } else { Default::default() };
            assert_eq!(c.ownership().scope, want);
        }
    }

    /// A file that asks for a password holds the ones after it back, rules and all, and its prompt
    /// is not dropped by the next file's open.
    #[test]
    fn a_file_behind_a_prompt_keeps_the_prompt_and_the_files_after_it_wait_with_their_rules() {
        let app = PdfCraftApp::new();
        {
            let mut o = app.ownership_inbox.lock().unwrap();
            o.push(pdfcraft_engine::Ownership { read_only: true, ..share() });
            o.push(pdfcraft_engine::Ownership { actor: Some("bob".into()), annot_sync: true, ..Default::default() });
            let mut q = app.startup_inbox.lock().unwrap();
            q.push(("locked.pdf".into(), locked_pdf()));
            q.push(("plain.pdf".into(), pdf()));
        }
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        for _ in 0..50 {
            h.run_steps(2);
            if h.state().password_prompt.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        h.run_steps(3);
        assert_eq!(h.state().password_prompt.as_ref().map(|p| p.name.as_str()), Some("locked.pdf"));
        assert_eq!((h.state().startup_inbox.lock().unwrap().len(), h.state().ownership_inbox.lock().unwrap().len()), (1, 1), "the second file waits");
        assert!(h.state().views.is_empty());
        h.state_mut().submit_password(Some("pw".into()));
        for _ in 0..100 {
            h.run_steps(2);
            if h.state().views.len() == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(rules_of(&h, "locked.pdf"), (Some("alice".into()), true, true));
        assert_eq!(rules_of(&h, "plain.pdf"), (Some("bob".into()), false, true));
        assert!(h.state().password_prompt.is_none());
    }

    type Heard = std::rc::Rc<std::cell::RefCell<Vec<(u64, Vec<String>)>>>;

    /// What the page hears: the messages with their numbers, in the order they went out.
    fn listen(app: &mut PdfCraftApp) -> Heard {
        let heard = Heard::default();
        let sink = heard.clone();
        app.host_annots = Some(Box::new(move |e| {
            if let AnnotEvent::Changed { seq, changes } = e {
                sink.borrow_mut().push((*seq, changes.upserts.iter().map(|u| u.annot_id.clone()).collect()));
            }
        }));
        heard
    }

    /// An annotation by the user on the document the page's messages belong to.
    fn add_square(app: &mut PdfCraftApp, x: f64) {
        let id = app.annot_target.unwrap();
        let shape = pdfcraft_engine::Shape::Rectangle { rect: [x, 20.0, x + 4.0, 24.0] };
        let style = pdfcraft_engine::Style::default_for(&shape);
        let new = pdfcraft_engine::NewAnnotation { page: 0, shape, style, contents: "c".into(), author: "alice".into() };
        app.session.apply(id, pdfcraft_engine::Edit::AddAnnotation(new)).unwrap();
    }

    /// Seq rule 1: the first message of each document opened starts at 1 (never 0, never missing).
    #[test]
    fn the_first_message_of_each_opened_document_is_number_one() {
        let mut app = PdfCraftApp::new();
        let heard = listen(&mut app);
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        add_square(h.state_mut(), 10.0);
        h.run_steps(2);
        add_square(h.state_mut(), 30.0);
        h.run_steps(2);
        assert_eq!(heard.borrow().iter().map(|m| m.0).collect::<Vec<_>>(), [1, 2], "and one more for each message after it");
        // The next document starts over.
        heard.borrow_mut().clear();
        h.state().ownership_inbox.lock().unwrap().push(share());
        h.state().startup_inbox.lock().unwrap().push(("b.pdf".into(), pdf()));
        h.run_steps(4);
        add_square(h.state_mut(), 10.0);
        h.run_steps(2);
        assert_eq!(heard.borrow().iter().map(|m| m.0).collect::<Vec<_>>(), [1]);
    }

    /// Seq rule 2: every message has the next number, the pieces of a split change included.
    #[test]
    fn a_change_split_in_pieces_numbers_each_piece() {
        let mut app = PdfCraftApp::new();
        let heard = listen(&mut app);
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        for i in 0..(MAX_PER_MESSAGE + 5) {
            add_square(h.state_mut(), (i % 150) as f64);
        }
        h.run_steps(2);
        add_square(h.state_mut(), 5.0);
        h.run_steps(2);
        let heard = heard.borrow();
        assert_eq!(heard.iter().map(|m| m.0).collect::<Vec<_>>(), [1, 2, 3], "{:?}", heard.iter().map(|m| (m.0, m.1.len())).collect::<Vec<_>>());
        assert_eq!((heard[0].1.len(), heard[1].1.len(), heard[2].1.len()), (MAX_PER_MESSAGE, 5, 1));
    }

    /// Seq rule 3: a reset numbered N leaves alone what the messages after N carry, and treats what
    /// the earlier messages carried as the server has it (here: gone from the list).
    #[test]
    fn a_reset_leaves_the_annotations_of_messages_after_its_number_alone() {
        let mut app = PdfCraftApp::new();
        let heard = listen(&mut app);
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let inbox = app.annot_inbox.clone();
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        add_square(h.state_mut(), 10.0);
        h.run_steps(2);
        add_square(h.state_mut(), 40.0);
        h.run_steps(2);
        let names: Vec<String> = heard.borrow().iter().map(|m| m.1[0].clone()).collect();
        assert_eq!(heard.borrow().iter().map(|m| m.0).collect::<Vec<_>>(), [1, 2]);
        // The page fetched an empty list after message 1: the second annotation is not in it yet.
        inbox.lock().unwrap().push(AnnotCommand::Reset { seq: 1, items: vec![] });
        h.run_steps(2);
        let (_, id) = h.state().active_ids().unwrap();
        let d = h.state().session.get(id).unwrap();
        assert!(d.annot_position(&names[0]).is_none(), "the list is the truth for what it had seen");
        assert!(d.annot_position(&names[1]).is_some(), "message 2 came after the list was fetched");
    }
    /// A change from the page makes the pages it touched be drawn again: what is on screen was
    /// rendered before it.
    #[test]
    fn a_remote_change_makes_the_pages_be_drawn_again() {
        let app = PdfCraftApp::new();
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let inbox = app.annot_inbox.clone();
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        for _ in 0..40 {
            h.run_steps(2);
            let m = h.state().views.first().map(|v| v.raster_memory());
            if m.is_some_and(|m| m.page_count + m.tile_count + m.thumbnail_count > 0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(h.state().views[0].raster_memory().page_count + h.state().views[0].raster_memory().tile_count > 0, "the page was drawn");
        assert!(!h.state().views[0].page_is_stale(0));
        inbox.lock().unwrap().push(AnnotCommand::Reset { seq: 0, items: vec![square("one")] });
        // Stop before the next render replaces the old raster.
        h.step();
        assert!(h.state().views[0].page_is_stale(0), "the old raster waits for its replacement");
    }

    /// Only the pages the message touched are drawn again, not every page of a large document.
    #[test]
    fn a_remote_change_draws_only_its_own_pages_again() {
        let app = PdfCraftApp::new();
        app.ownership_inbox.lock().unwrap().push(share());
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf_pages(3)));
        let inbox = app.annot_inbox.clone();
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        for _ in 0..80 {
            h.run_steps(2);
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let view = &h.state().views[0];
        assert!(view.has_raster(1), "page 1 was drawn");
        assert!([0, 1, 2].iter().all(|p| !view.page_is_stale(*p)));
        inbox.lock().unwrap().push(AnnotCommand::Apply { upserts: vec![square_on("one", 1)], deletes: vec![] });
        h.step();
        let view = &h.state().views[0];
        assert!(view.page_is_stale(1), "the page with the new annotation is drawn again");
        assert!(!view.page_is_stale(0) && !view.page_is_stale(2), "the other pages are left alone");
        // Taking it away draws the page it was on again.
        assert!(h.state().session.get(h.state().views[0].id).unwrap().annot_position("one").is_some(), "the annotation is there to be taken away");
        for _ in 0..80 {
            h.run_steps(2);
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        inbox.lock().unwrap().push(AnnotCommand::Apply { upserts: vec![], deletes: vec!["one".into()] });
        h.step();
        let view = &h.state().views[0];
        assert!(view.page_is_stale(1) && !view.page_is_stale(0) && !view.page_is_stale(2));
    }

    fn open_read_only(language: Option<&str>) -> (egui_kittest::Harness<'static, PdfCraftApp>, std::sync::Arc<std::sync::Mutex<Vec<AnnotCommand>>>) {
        let mut app = PdfCraftApp::new();
        if let Some(l) = language {
            app.set_option("language", l).unwrap();
        }
        app.ownership_inbox.lock().unwrap().push(pdfcraft_engine::Ownership { read_only: true, ..share() });
        app.startup_inbox.lock().unwrap().push(("a.pdf".into(), pdf()));
        let inbox = app.annot_inbox.clone();
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        for _ in 0..20 {
            h.run_steps(2);
        }
        assert!(h.state().views.first().is_some_and(|v| h.state().session.get(v.id).is_some()), "the document is open");
        (h, inbox)
    }

    /// A read-only (viewer-only) shared document shows other people's annotations as they come.
    #[test]
    fn a_read_only_shared_document_takes_the_remote_annotations() {
        let (mut h, inbox) = open_read_only(None);
        let doc = h.state().views[0].id;
        assert!(h.state().session.get(doc).unwrap().ownership().read_only);
        inbox.lock().unwrap().push(AnnotCommand::Reset { seq: 0, items: vec![square("one")] });
        h.step();
        assert!(h.state().session.get(doc).unwrap().annot_position("one").is_some(), "reset");
        inbox.lock().unwrap().push(AnnotCommand::Apply { upserts: vec![square("two")], deletes: vec!["one".into()] });
        h.step();
        let d = h.state().session.get(doc).unwrap();
        assert!(d.annot_position("two").is_some() && d.annot_position("one").is_none(), "apply");
    }

    /// Undo and redo refused by the sharing rules say so in the UI language.
    #[test]
    fn a_refused_undo_is_said_in_the_ui_language() {
        let (mut h, _) = open_read_only(Some("ko"));
        h.state_mut().undo();
        assert_eq!(h.state().toast.as_ref().map(|t| t.0.as_str()), Some("문서가 읽기 전용입니다"));
        h.state_mut().toast = None;
        h.state_mut().redo();
        assert_eq!(h.state().toast.as_ref().map(|t| t.0.as_str()), Some("문서가 읽기 전용입니다"));
    }
}
