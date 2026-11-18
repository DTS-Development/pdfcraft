//! A comparison the embedding page starts (web `pdfcraft:compare`).
//!
//! The host hands in both files: the old one opens in a background tab, the new one becomes the
//! active tab, and the same comparison as Compare Files runs. The host is told the counts.
//! A frame compares once; later requests are answered with "busy".

use std::sync::Arc;

use pdfcraft_engine::compare::Kind;

use crate::PdfCraftApp;
use crate::compare_ui::CompareView;

const MAX_ID_CHARS: usize = 128;
const MAX_ERROR_CHARS: usize = 200;

/// The most bytes either file may have; the bridge checks it before copying into memory.
pub const MAX_COMPARE_BYTES: usize = 200 << 20;

/// Which request: compare at once, open the two files without comparing (`compare-preview`), or compare
/// the files a preview opened (`compare-run`; its `old` and `new` are empty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Compare,
    Preview,
    Run,
}

/// What the host asks for: `(name, bytes)` of each file, and the rules both open under (its
/// `mode` and `policy`, read like an open's; the narrowest when it names none).
#[derive(Clone, Debug, PartialEq)]
pub struct CompareCommand {
    pub id: String,
    pub op: CompareOp,
    pub old: (String, Vec<u8>),
    pub new: (String, Vec<u8>),
    pub ownership: pdfcraft_engine::Ownership,
    /// How the overlay is inked (`overlay` of the message); the viewer's colours when none.
    pub overlay: Option<pdfcraft_engine::compare::OverlayStyle>,
    /// The shading colours of the side-by-side view (`side` of the message).
    pub side: SideColors,
    /// `view` of the message: the view the comparison opens on.
    pub view: CompareView,
    /// `lockView` of the message: the user can't switch the view.
    pub lock_view: bool,
}

/// Read the host's `overlay` object: `colorA` (the old file's ink), `colorB` (the new file's ink),
/// `opacity` (0 to 1, both files), `opacityA` / `opacityB` (one file's, which win over `opacity`). A value
/// of the wrong type or out of range is an error, never ignored.
pub fn overlay_style_from_json(v: &serde_json::Value) -> Result<pdfcraft_engine::compare::OverlayStyle, String> {
    let obj = v.as_object().ok_or("overlay must be an object")?;
    let mut style = pdfcraft_engine::compare::OverlayStyle::default();
    let rgb = |value: &serde_json::Value, name: &str| -> Result<[u8; 3], String> {
        let text = value.as_str().ok_or_else(|| format!("{name} must be a string"))?;
        Ok(crate::watermark::parse_rgb(text).map_err(|_| format!("{name} must be #rgb or #rrggbb"))?.map(|c| (c * 255.0).round() as u8))
    };
    let opacity = |value: &serde_json::Value, name: &str| -> Result<f32, String> {
        value.as_f64().filter(|n| (0.0..=1.0).contains(n)).map(|n| n as f32).ok_or_else(|| format!("{name} must be a number from 0 to 1"))
    };
    for (key, value) in obj {
        match key.as_str() {
            "colorA" => style.chosen = rgb(value, "colorA")?,
            "colorB" => style.current = rgb(value, "colorB")?,
            "opacity" | "opacityA" | "opacityB" => {
                opacity(value, key)?;
            }
            other => return Err(format!("unknown overlay field {other}")),
        }
    }
    // `opacity` first, then the one-file values over it, whatever order the keys came in.
    if let Some(v) = obj.get("opacity") {
        let n = opacity(v, "opacity")?;
        (style.strength_current, style.strength_chosen) = (n, n);
    }
    if let Some(v) = obj.get("opacityA") {
        style.strength_chosen = opacity(v, "opacityA")?;
    }
    if let Some(v) = obj.get("opacityB") {
        style.strength_current = opacity(v, "opacityB")?;
    }
    Ok(style)
}

/// Read the host's `side` object: `colorA` (the old file's shading), `colorB` (the new file's), each
/// `#rgb` or `#rrggbb`; a colour left out stays as the viewer has it. Nothing else is allowed.
pub fn side_colors_from_json(v: &serde_json::Value) -> Result<SideColors, String> {
    let obj = v.as_object().ok_or("side must be an object")?;
    let mut out = SideColors::default();
    for (key, value) in obj {
        let text = value.as_str().ok_or_else(|| format!("{key} must be a string"))?;
        let rgb = || crate::watermark::parse_rgb(text).map(|c| c.map(|v| (v * 255.0).round() as u8)).map_err(|_| format!("{key} must be #rgb or #rrggbb"));
        match key.as_str() {
            "colorA" => out.old = Some(rgb()?),
            "colorB" => out.new = Some(rgb()?),
            other => return Err(format!("unknown side field {other}")),
        }
    }
    Ok(out)
}

/// The host's shading colours of the side-by-side view: the old file's words and the new file's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SideColors {
    pub old: Option<[u8; 3]>,
    pub new: Option<[u8; 3]>,
}

/// What the host is told.
#[derive(Clone, Debug, PartialEq)]
pub enum CompareEvent {
    Done { id: String, replaced: usize, inserted: usize, deleted: usize, visual: usize },
    Failed { id: String, error: String },
    /// The two files of a `compare-preview` are open side by side.
    PreviewDone { id: String },
    PreviewFailed { id: String, error: String },
    /// PPM: which view the comparison shows (side by side, the overlay, or the old file beside the overlay)
    /// and whether the overlay is still being built; sent after `Done` and again whenever either changes,
    /// so the host can word its status line like the viewer's.
    View { id: String, view: CompareView, building: bool },
}

/// Requests from the host, drained every frame.
pub type CompareInbox = Arc<std::sync::Mutex<Vec<CompareCommand>>>;

/// Tells the embedding page how the comparison went (web).
pub type HostCompareFn = Box<dyn Fn(&CompareEvent)>;

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

impl PdfCraftApp {
    fn emit_compare(&self, event: CompareEvent) {
        if let Some(report) = &self.host_compare {
            report(&event);
        }
    }

    /// PPM: tell the host when the view or the overlay build changes.
    fn report_compare_view(&mut self) {
        // A locked view can't be fixed by the user's hand: an overlay that failed to build goes back to side by side.
        if self.compare.as_ref().is_some_and(|c| c.ppm && c.lock_view && c.overlay_failed && c.overlay_shown) {
            self.show_overlay(false);
        }
        let Some((id, sent)) = self.compare_view_sent.as_ref() else { return };
        let Some(c) = self.compare.as_ref().filter(|c| c.ppm) else { return };
        let now = (c.view(), c.overlay_job.is_some());
        if now == *sent {
            return;
        }
        let event = CompareEvent::View { id: id.clone(), view: now.0, building: now.1 };
        self.emit_compare(event);
        if let Some((_, sent)) = self.compare_view_sent.as_mut() {
            *sent = now;
        }
    }

    fn fail_compare(&self, id: &str, error: &str) {
        self.emit_compare(CompareEvent::Failed { id: cut(id, MAX_ID_CHARS), error: cut(error, MAX_ERROR_CHARS) });
    }

    /// Once a frame: take the host's requests and run them in order.
    pub(crate) fn host_compare_tick(&mut self) {
        self.report_compare_view();
        let commands: Vec<CompareCommand> = self.compare_inbox.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for command in commands {
            let id = command.id.clone();
            let too_long = id.is_empty() || id.chars().count() > MAX_ID_CHARS;
            let fail = |app: &Self, error: &str| match command.op {
                CompareOp::Preview => app.emit_compare(CompareEvent::PreviewFailed { id: cut(&id, MAX_ID_CHARS), error: cut(error, MAX_ERROR_CHARS) }),
                _ => app.fail_compare(&id, error),
            };
            if too_long {
                fail(self, "the request id is missing or too long");
            } else if command.op == CompareOp::Run {
                // Only the files a preview opened, and once.
                if !self.compare.as_ref().is_some_and(|c| c.preview) {
                    fail(self, "no preview is open: send compare-preview first");
                    continue;
                }
                match self.run_host_compare(command.overlay, command.side, command.view, command.lock_view) {
                    Ok(counts) => self.report_compare_done(command.id, counts),
                    Err(error) => fail(self, &error),
                }
            } else if self.compare_taken {
                fail(self, "busy: this window already compares");
            } else {
                self.compare_taken = true;
                let opened = self.open_compare_pair(command.old, command.new, command.ownership);
                match (opened, command.op) {
                    (Err(error), _) => fail(self, &error),
                    (Ok(()), CompareOp::Preview) => {
                        if let Some(old) = self.compare_old {
                            let ppm = self.host_lock;
                            self.end_compare();
                            if let Some(new) = self.active_ids().map(|(_, id)| id) {
                                self.compare = Some(crate::compare_ui::CompareState::preview(old, new, ppm));
                            }
                        }
                        self.emit_compare(CompareEvent::PreviewDone { id: cut(&id, MAX_ID_CHARS) });
                    }
                    (Ok(()), _) => match self.run_host_compare(command.overlay, command.side, command.view, command.lock_view) {
                        Ok(counts) => self.report_compare_done(command.id, counts),
                        Err(error) => fail(self, &error),
                    },
                }
            }
        }
    }

    /// Tell the host the counts, and the view when it starts on one that builds (so a quick build can't hide it).
    fn report_compare_done(&mut self, id: String, (replaced, inserted, deleted, visual): (usize, usize, usize, usize)) {
        self.emit_compare(CompareEvent::Done { id: id.clone(), replaced, inserted, deleted, visual });
        // The host starts from side by side, so only a change from that is told.
        let started = self.compare.as_ref().filter(|c| c.ppm && c.overlay_shown).map(|c| (c.view(), c.overlay_job.is_some()));
        if let Some((view, building)) = started.filter(|(_, building)| *building) {
            self.emit_compare(CompareEvent::View { id: id.clone(), view, building });
        }
        self.compare_view_sent = Some((id, started.filter(|(_, building)| *building).unwrap_or((CompareView::Side, false))));
    }

    /// Open both files (old in the background, new active). `compare_old` names the old one after.
    fn open_compare_pair(&mut self, old: (String, Vec<u8>), new: (String, Vec<u8>), ownership: pdfcraft_engine::Ownership) -> Result<(), String> {
        // A prompt or a host file still on its way in is not ours to take over.
        if self.password_prompt.is_some() || self.annot_open_waiting || self.host_open_pending() {
            return Err("a document is still being opened".into());
        }
        // A request supersedes whatever the host armed or queued.
        self.disarm_stamp();
        // The rules come from this request alone, never from an open that came before it.
        // Both files are only looked at: read-only, so nothing here is ever saved back to the parent.
        self.session.set_open_ownership(pdfcraft_engine::Ownership { read_only: true, annot_sync: false, hide_comments_panel: false, ..ownership });
        let before = self.views.len();
        let opened = [(old, false), (new, true)].into_iter().try_for_each(|((name, bytes), activate)| {
            let name = if name.trim().is_empty() { "document.pdf".to_string() } else { name };
            self.open_bytes_with_focus(&name, None, bytes, activate).map_err(|e| format!("couldn't open {name}: {e}"))?;
            // An encrypted file asks for a password the host can't give.
            if self.password_prompt.take().is_some() {
                return Err(format!("{name} needs a password"));
            }
            Ok(())
        });
        // Later opens start from the build's own default again.
        self.session.set_open_ownership(if self.host_lock { crate::host_lock::viewer_ownership() } else { Default::default() });
        opened?;
        if self.views.len() != before + 2 {
            return Err("couldn't open both files".into());
        }
        // The host's own viewer draws no replies in a comparison either.
        for view in &self.views[before..] {
            self.session.set_hide_replies(view.id, true);
        }
        self.end_compare();
        self.compare_old = Some(self.views[before].id);
        Ok(())
    }

    /// Compare the two open files and apply the host's looks: the counts of replaced, inserted, deleted
    /// and visual differences.
    fn run_host_compare(
        &mut self,
        overlay: Option<pdfcraft_engine::compare::OverlayStyle>,
        side: SideColors,
        view: CompareView,
        lock_view: bool,
    ) -> Result<(usize, usize, usize, usize), String> {
        // The new file is the active one (a preview may have been left on the other tab).
        if let Some(new) = self.compare.as_ref().map(|c| c.new) {
            self.active = self.views.iter().position(|v| v.id == new).or(self.active);
        }
        self.run_compare();
        if let (Some(style), Some(c)) = (overlay, self.compare.as_mut()) {
            c.overlay_style = style;
        }
        self.apply_side_colors(side);
        if let Some(c) = self.compare.as_mut() {
            c.lock_view = lock_view;
        }
        if view != CompareView::Side {
            // The same path as the user's Overlay buttons: the build starts and `View` reports it.
            self.set_view(view);
        }
        let Some(s) = self.compare.as_ref() else { return Err("the comparison failed".into()) };
        Ok((s.result.count(Kind::Replaced), s.result.count(Kind::Inserted), s.result.count(Kind::Deleted), s.visual.len()))
    }

    /// PPM: shade the old and the new file's words in the host's colours (the alpha stays the viewer's).
    fn apply_side_colors(&mut self, side: SideColors) {
        let Some(c) = self.compare.as_mut().filter(|c| c.ppm) else { return };
        if side == SideColors::default() {
            return;
        }
        let rgb = |c: [u8; 3]| egui::Color32::from_rgb(c[0], c[1], c[2]);
        c.mark_colors = [side.old.map_or(c.mark_colors[0], rgb), side.new.map_or(c.mark_colors[1], rgb)];
        c.marks_custom = true;
        let (old, new, colors) = (c.old, c.new, c.mark_colors);
        for view in &mut self.views {
            let colour = if view.id == old { colors[0] } else if view.id == new { colors[1] } else { continue };
            for mark in &mut view.compare_marks {
                mark.2 = colour;
            }
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use pdfcraft_engine::Session;

    use super::*;
    use crate::RightPanel;

    #[test]
    fn the_overlay_style_is_read_strictly() {
        let style = overlay_style_from_json(&serde_json::json!({ "colorA": "#00f", "colorB": "#ff8000", "opacity": 0.5 })).unwrap();
        assert_eq!((style.chosen, style.current, (style.strength_current, style.strength_chosen)), ([0, 0, 255], [255, 128, 0], (0.5, 0.5)));
        assert_eq!(overlay_style_from_json(&serde_json::json!({})).unwrap(), Default::default());
        for bad in [
            serde_json::json!({ "colorA": "red" }),
            serde_json::json!({ "colorB": 3 }),
            serde_json::json!({ "opacity": 1.5 }),
            serde_json::json!({ "color": "#fff" }),
            serde_json::json!([]),
        ] {
            assert!(overlay_style_from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_hosts_overlay_style_reaches_the_overlay_build() {
        let (mut h, _) = app(true);
        let style = pdfcraft_engine::compare::OverlayStyle { current: [0, 200, 0], chosen: [200, 0, 200], strength_current: 0.5, strength_chosen: 0.5 };
        send(&mut h, CompareCommand { overlay: Some(style), ..command("c1") });
        assert_eq!(h.state().compare.as_ref().unwrap().overlay_style, style);
    }

    fn text_pdf(text: &str) -> Vec<u8> {
        Session::new().create_from_text("t", text).unwrap().to_vec()
    }

    fn command(id: &str) -> CompareCommand {
        CompareCommand {
            id: id.into(),
            old: ("old.pdf".into(), text_pdf("Delivery within five days. Returns accepted.")),
            new: ("new.pdf".into(), text_pdf("Delivery within three days. Returns accepted for a week.")),
            op: CompareOp::Compare,
            ownership: Default::default(),
            overlay: None,
            side: SideColors::default(),
            view: CompareView::Side,
            lock_view: false,
        }
    }

    fn app(hosted: bool) -> (Harness<'static, PdfCraftApp>, Rc<RefCell<Vec<CompareEvent>>>) {
        let events: Rc<RefCell<Vec<CompareEvent>>> = Rc::default();
        let sink = events.clone();
        let mut harness = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.host_compare = Some(Box::new(move |e| sink.borrow_mut().push(e.clone())));
            if hosted {
                app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
            }
            app
        });
        harness.run_steps(3);
        (harness, events)
    }

    fn send(h: &mut Harness<'static, PdfCraftApp>, c: CompareCommand) {
        h.state().compare_inbox.lock().unwrap().push(c);
        h.run_steps(4);
    }

    #[test]
    fn the_host_starts_a_comparison_and_hears_the_counts() {
        let (mut h, events) = app(true);
        send(&mut h, command("c1"));
        assert_eq!(*events.borrow(), vec![CompareEvent::Done { id: "c1".into(), replaced: 2, inserted: 0, deleted: 0, visual: events_visual(&h) }]);
        let state = h.state();
        assert_eq!(state.right, Some(RightPanel::Compare));
        let (_, active) = state.active_ids().unwrap();
        assert_eq!(state.session.get(active).unwrap().name, "new.pdf", "new is the active tab");
        let s = state.compare.as_ref().unwrap();
        assert_eq!(state.session.get(s.old).unwrap().name, "old.pdf");
        assert_eq!(s.new, active);
    }

    fn events_visual(h: &Harness<'static, PdfCraftApp>) -> usize {
        h.state().compare.as_ref().unwrap().visual.len()
    }

    #[test]
    fn a_second_request_is_busy() {
        let (mut h, events) = app(true);
        send(&mut h, command("c1"));
        send(&mut h, command("c2"));
        let events = events.borrow();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1], CompareEvent::Failed { id, error } if id == "c2" && error.starts_with("busy")), "{events:?}");
        assert_eq!(h.state().views.len(), 2, "no further tabs");
    }

    #[test]
    fn bad_requests_fail_and_a_bad_id_does_not_use_up_the_window() {
        let (mut h, events) = app(true);
        let mut bad = command("c1");
        bad.new.1 = b"not a pdf".to_vec();
        send(&mut h, bad);
        send(&mut h, command(""));
        send(&mut h, command(&"x".repeat(MAX_ID_CHARS + 1)));
        {
            let events = events.borrow();
            assert_eq!(events.len(), 3);
            assert!(events.iter().all(|e| matches!(e, CompareEvent::Failed { .. })), "{events:?}");
            assert!(matches!(&events[2], CompareEvent::Failed { id, .. } if id.chars().count() == MAX_ID_CHARS));
        }
        assert!(h.state().compare.is_none());
        // A request with a good id and a broken file used the window up.
        send(&mut h, command("c4"));
        assert!(matches!(events.borrow().last(), Some(CompareEvent::Failed { error, .. }) if error.starts_with("busy")));
    }

    #[test]
    fn a_bad_id_alone_leaves_the_window_free() {
        let (mut h, events) = app(true);
        send(&mut h, command(""));
        send(&mut h, command("ok"));
        assert!(matches!(events.borrow().as_slice(), [CompareEvent::Failed { .. }, CompareEvent::Done { id, .. }] if id == "ok"));
    }

    /// Both files open under the request's own rules, whatever an earlier open left behind.
    #[test]
    fn both_files_open_under_the_requests_policy_and_not_an_earlier_opens_rules() {
        use pdfcraft_engine::{EditScope, Ownership};
        let (mut h, events) = app(true);
        h.state_mut().host_lock = true;
        // A builder's open came first in this window.
        h.state_mut().session.set_open_ownership(Ownership { scope: EditScope::All, ..Default::default() });
        let mut c = command("c1");
        c.ownership = crate::with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, vec![("page.rotate".into(), true)]);
        send(&mut h, c);
        assert!(matches!(events.borrow().as_slice(), [CompareEvent::Done { .. }]), "{:?}", events.borrow());
        let state = h.state();
        let s = state.compare.as_ref().unwrap();
        for id in [s.old, s.new] {
            let own = state.session.get(id).unwrap().ownership().clone();
            assert_eq!(own.scope, EditScope::Viewer);
            assert_eq!(own.overrides, vec![("page.rotate".to_string(), true)]);
            assert!(own.allow.has(pdfcraft_engine::EditClass::Page));
            assert!(own.read_only, "a compared file is only looked at, never saved back");
        }
    }

    /// The compared files are only looked at: read-only and never in the host's annotation database,
    /// with the request's actor, and later opens start from the build's own rules again.
    #[test]
    fn compare_open_keeps_the_actor_and_drops_sharing_and_the_open_rules_are_the_builds_again() {
        use pdfcraft_engine::Ownership;
        let (mut h, events) = app(true);
        h.state_mut().session.set_open_ownership(Ownership { actor: Some("earlier".into()), ..Default::default() });
        let mut c = command("c1");
        c.ownership = Ownership { actor: Some("alice".into()), annot_sync: true, hide_comments_panel: true, ..Default::default() };
        send(&mut h, c);
        assert!(matches!(events.borrow().as_slice(), [CompareEvent::Done { .. }]), "{:?}", events.borrow());
        let state = h.state();
        let s = state.compare.as_ref().unwrap();
        for id in [s.old, s.new] {
            let own = state.session.get(id).unwrap().ownership();
            assert_eq!(own.actor.as_deref(), Some("alice"));
            assert!(own.read_only && !own.annot_sync && !own.hide_comments_panel, "{own:?}");
        }
        assert_eq!(state.session.open_ownership().actor, None, "later opens start from the build's own rules again");
        for id in [s.old, s.new] {
            assert!(state.session.get(id).unwrap().replies_hidden(), "the host's comparison draws no replies");
        }
    }

    /// A document opened on the build's own (not for the host's comparison) draws what it always did.
    #[test]
    fn the_replies_of_a_document_opened_without_the_host_are_drawn() {
        let (mut h, _) = app(true);
        h.state_mut().open_bytes_with_focus("a.pdf", None, text_pdf("x"), true).unwrap();
        let id = h.state().views[0].id;
        assert!(!h.state().session.get(id).unwrap().replies_hidden());
    }

    /// A password prompt or a host file still on its way in is not taken over by a compare.
    #[test]
    fn a_compare_waits_while_a_host_file_is_still_coming_in() {
        let (mut h, events) = app(true);
        h.state_mut().annot_open_waiting = true;
        send(&mut h, command("c1"));
        assert!(matches!(events.borrow().as_slice(), [CompareEvent::Failed { .. }]), "{:?}", events.borrow());
        assert!(h.state().compare.is_none() && h.state().views.is_empty(), "nothing was opened");
    }

    #[test]
    fn a_compare_without_rules_is_a_viewers() {
        use pdfcraft_engine::EditScope;
        let (mut h, _) = app(true);
        h.state_mut().host_lock = true;
        let mut c = command("c1");
        c.ownership = crate::with_policy(pdfcraft_engine::Ownership { scope: crate::open_scope(None), ..Default::default() }, Vec::new());
        send(&mut h, c);
        let state = h.state();
        let s = state.compare.as_ref().unwrap();
        // With the lock built in, no mode is the narrowest; without it, nothing is limited.
        let want = if crate::HOST_LOCK { EditScope::Viewer } else { EditScope::All };
        assert_eq!(state.session.get(s.new).unwrap().ownership().scope, want);
    }

    #[test]
    fn hosted_compare_panel_hides_marks_and_report() {
        let (mut h, _) = app(true);
        send(&mut h, command("c1"));
        // The PPM panel is the viewer's change list; it has no Clear (the page closes the comparison).
        #[cfg(not(feature = "host-lock"))]
        h.get_by_label("Clear");
        assert!(h.query_by_label("Mark as comments").is_none());
        assert!(h.query_by_label("Report…").is_none());
    }

    /// Without an embedding page; a host-lock build has no such mode (it shows only the notice).
    #[cfg(not(feature = "host-lock"))]
    #[test]
    fn standalone_compare_panel_keeps_marks_and_report() {
        let (mut h, _) = app(false);
        send(&mut h, command("c1"));
        h.get_by_label("Mark as comments");
        h.get_by_label("Report…");
    }

    /// A PPM (host-lock) window of `width` points with the host's compare already run.
    fn ppm_compared(width: f32) -> Harness<'static, PdfCraftApp> {
        let mut h = Harness::builder().with_size(egui::vec2(width, 900.0)).build_eframe(move |_| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.host_lock = true;
            app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
            app
        });
        h.run_steps(3);
        send(&mut h, command("c1"));
        h
    }

    #[test]
    fn ppm_compare_shades_words_red_and_green_side_by_side_at_any_width() {
        use crate::compare_ui::ppm;
        let h = ppm_compared(700.0);
        let s = h.state().compare.as_ref().unwrap();
        assert!(s.ppm && s.side_by_side, "side by side even in a narrow window");
        assert!(s.visual.is_empty(), "no visual marks: the viewer shows words only");
        let (new_i, old_i) = (h.state().views.iter().position(|v| v.id == s.new).unwrap(), h.state().views.iter().position(|v| v.id == s.old).unwrap());
        let (new_v, old_v) = (&h.state().views[new_i], &h.state().views[old_i]);
        assert!(!new_v.compare_marks.is_empty() && new_v.compare_marks.iter().all(|m| m.2 == ppm::BASE), "the current file's words are red");
        assert!(!old_v.compare_marks.is_empty() && old_v.compare_marks.iter().all(|m| m.2 == ppm::TARGET), "the chosen file's words are green");
        assert_eq!((new_v.compare_alpha, old_v.compare_alpha), (ppm::MARK_ALPHA, ppm::MARK_ALPHA));
        h.get_by_label("Change list");
        h.get_by_label("2 differences");
        h.get_by_label("Overlay");
    }

    #[test]
    fn ppm_overlay_is_built_once_in_a_background_tab_and_the_halves_come_back() {
        let mut h = ppm_compared(1400.0);
        let rules = h.state().session.open_ownership().clone();
        h.state_mut().show_overlay(true);
        h.run_steps(5);
        let s = h.state().compare.as_ref().unwrap();
        assert!(s.overlay_shown && s.overlay_job.is_none() && !s.overlay_failed);
        let (doc, notice) = s.overlay.clone().expect("the overlay was built");
        assert_eq!(notice, None, "equal page counts need no note");
        assert_eq!(h.state().views.len(), 2, "old and new: the overlay is no tab");
        let d = h.state().session.get(doc).unwrap();
        assert_eq!((d.name.as_str(), d.info.pages.len()), ("compare.pdf", 1));
        assert!(d.ownership().read_only, "looked at only");
        assert_eq!(*h.state().session.open_ownership(), rules, "later opens keep the rules they had");
        h.get_by_label("Lines only in the current file");
        h.state_mut().show_overlay(false);
        h.run_steps(3);
        h.state_mut().show_overlay(true);
        h.run_steps(3);
        assert_eq!(h.state().views.len(), 2, "built once, still no tab");
        h.state_mut().show_overlay(false);
        h.run_steps(3);
        h.get_by_label("Side by side");
    }

    #[test]
    fn ppm_overlay_pane_renders_its_pages_while_the_new_file_is_the_active_tab() {
        let mut h = ppm_compared(1400.0);
        h.state_mut().show_overlay(true);
        h.run_steps(5);
        let (doc, _) = h.state().compare.as_ref().unwrap().overlay.clone().expect("the overlay was built");
        let active = h.state().active.unwrap();
        assert_eq!(h.state().views[active].id, h.state().compare.as_ref().unwrap().new, "the compare tab is the new file's");
        let pane_ready = |h: &Harness<'static, PdfCraftApp>| h.state().compare.as_ref().unwrap().overlay_view.as_ref().is_some_and(|v| v.id == doc && v.has_page_textures());
        for _ in 0..200 {
            if pane_ready(&h) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            h.run_steps(1);
        }
        assert!(pane_ready(&h), "the overlay pane got a rendered page, not \"Rendering...\" for ever");
    }

    /// The overlay is a document and a view only for its pane: no tab, no tab to switch to.
    #[test]
    fn ppm_overlay_is_no_tab_and_leaves_with_the_comparison() {
        let mut h = ppm_compared(1400.0);
        let (tabs, active) = (h.state().views.len(), h.state().active);
        h.state_mut().show_overlay(true);
        h.run_steps(6);
        let (doc, _) = h.state().compare.as_ref().unwrap().overlay.clone().expect("the overlay was built");
        assert!(h.state().compare.as_ref().unwrap().overlay_view.is_some());
        assert_eq!(h.state().views.len(), tabs, "no tab was added");
        assert!(h.query_by_label_contains("compare.pdf").is_none(), "not on the tab strip");
        assert!((0..h.state().views.len()).all(|i| crate::canvas::tab_label(h.state(), i).as_deref() != Some("compare.pdf")));
        for _ in 0..4 {
            h.state_mut().execute("view.next_tab");
            h.run_steps(2);
            let a = h.state().active.expect("a tab stays active");
            assert!(a < tabs && h.state().views[a].id != doc, "next tab skips the overlay");
        }
        h.state_mut().active = active;
        h.run_steps(2);
        h.state_mut().compare_action(active.unwrap(), crate::compare_ui::PanelAction::Clear);
        h.run_steps(2);
        assert_eq!((h.state().views.len(), h.state().active), (tabs, active), "tabs and the active one are as before");
        assert!(h.state().compare.is_none() && h.state().session.get(doc).is_none(), "the overlay document is closed");
    }

    #[test]
    fn ppm_closing_a_compared_tab_ends_the_comparison_and_its_shading_and_overlay() {
        let mut h = ppm_compared(1400.0);
        h.state_mut().show_overlay(true);
        h.run_steps(6);
        let (doc, _) = h.state().compare.as_ref().unwrap().overlay.clone().expect("the overlay was built");
        let (old, new) = { let c = h.state().compare.as_ref().unwrap(); (c.old, c.new) };
        assert!(h.state().views.iter().any(|v| v.id == new && !v.compare_marks.is_empty()), "the new file is shaded");
        let at = h.state().views.iter().position(|v| v.id == old).unwrap();
        h.state_mut().close_tab(at);
        h.run_steps(2);
        assert!(h.state().compare.is_none() && h.state().session.get(doc).is_none(), "the overlay document is closed");
        assert!(h.state().views.iter().all(|v| v.compare_marks.is_empty()), "no shading is left with no way to clear it");
        assert!(h.state().right.is_none());
    }

    #[test]
    fn ppm_comparing_again_closes_the_overlay_of_the_comparison_before() {
        let mut h = ppm_compared(1400.0);
        h.state_mut().show_overlay(true);
        h.run_steps(6);
        let (doc, _) = h.state().compare.as_ref().unwrap().overlay.clone().expect("the overlay was built");
        let tabs = h.state().views.len();
        h.state_mut().run_compare();
        h.run_steps(2);
        let c = h.state().compare.as_ref().expect("a new comparison");
        assert!(c.overlay.is_none() && c.overlay_view.is_none() && !c.overlay_shown, "a fresh state");
        assert!(h.state().session.get(doc).is_none(), "the old overlay document is closed");
        assert_eq!(h.state().views.len(), tabs);
    }

    #[test]
    fn ppm_clicking_on_the_words_of_a_change_row_selects_it() {
        use egui_kittest::kittest::Queryable;
        let mut h = ppm_compared(1400.0);
        h.run_steps(2);
        let rows: Vec<_> = h.get_all_by_label_contains("Old -").collect();
        assert!(rows.len() >= 2, "two changes, two rows");
        rows[1].click();
        h.run_steps(3);
        assert_eq!(h.state().compare.as_ref().unwrap().selected, Some(1));
        let rows: Vec<_> = h.get_all_by_label_contains("Old -").collect();
        rows[0].click();
        h.run_steps(3);
        assert_eq!(h.state().compare.as_ref().unwrap().selected, Some(0));
    }

    #[test]
    fn ppm_host_is_told_the_view_and_the_overlay_build_when_they_change() {
        let events: Rc<RefCell<Vec<CompareEvent>>> = Rc::default();
        let sink = events.clone();
        let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.host_lock = true;
            app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
            app.host_compare = Some(Box::new(move |e| sink.borrow_mut().push(e.clone())));
            app
        });
        h.run_steps(3);
        send(&mut h, command("c1"));
        h.run_steps(2);
        assert_eq!(events.borrow().len(), 1, "only Done: side by side is where the host starts");
        let view = |view, building| CompareEvent::View { id: "c1".into(), view, building };
        h.state_mut().show_overlay(true);
        h.run_steps(6);
        h.state_mut().show_overlay(false);
        h.run_steps(2);
        let events = events.borrow();
        assert_eq!(events[1..], [view(CompareView::Overlay, true), view(CompareView::Overlay, false), view(CompareView::Side, false)], "{events:?}");
    }

    /// A hosted ppm window whose events are collected, with the compare request `c` sent.
    fn ppm_with(c: CompareCommand) -> (Harness<'static, PdfCraftApp>, Rc<RefCell<Vec<CompareEvent>>>) {
        let events: Rc<RefCell<Vec<CompareEvent>>> = Rc::default();
        let sink = events.clone();
        let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.host_lock = true;
            app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
            app.host_compare = Some(Box::new(move |e| sink.borrow_mut().push(e.clone())));
            app
        });
        h.run_steps(3);
        send(&mut h, c);
        (h, events)
    }

    #[test]
    fn the_host_can_open_the_comparison_on_the_overlay_and_is_told_it_builds() {
        let (mut h, events) = ppm_with(CompareCommand { view: CompareView::Overlay, ..command("c1") });
        h.run_steps(8);
        assert!(h.state().compare.as_ref().unwrap().overlay_shown);
        let view = |view, building| CompareEvent::View { id: "c1".into(), view, building };
        let events = events.borrow();
        assert!(matches!(events[0], CompareEvent::Done { .. }));
        assert_eq!(events[1..], [view(CompareView::Overlay, true), view(CompareView::Overlay, false)], "{events:?}");
    }

    #[test]
    fn a_locked_view_ignores_the_switch_buttons_and_an_open_one_does_not() {
        let (mut h, _) = ppm_with(CompareCommand { lock_view: true, ..command("c1") });
        h.run_steps(2);
        h.get_by_label("Overlay").click();
        h.run_steps(4);
        assert!(!h.state().compare.as_ref().unwrap().overlay_shown, "locked on side by side");
        let (mut h, _) = ppm_with(command("c2"));
        h.run_steps(2);
        h.get_by_label("Overlay").click();
        h.run_steps(4);
        assert!(h.state().compare.as_ref().unwrap().overlay_shown, "unlocked: the button switches");
        h.get_by_label("Side by side").click();
        h.run_steps(4);
        assert!(!h.state().compare.as_ref().unwrap().overlay_shown, "and the other button switches back");
    }

    #[test]
    fn a_locked_overlay_that_fails_to_build_goes_back_to_side_by_side_and_the_host_hears_it() {
        let (mut h, events) = ppm_with(CompareCommand { view: CompareView::Overlay, lock_view: true, ..command("c1") });
        h.run_steps(1);
        // The build fails as it would on a broken page pair.
        let c = h.state_mut().compare.as_mut().unwrap();
        c.overlay_job = None;
        c.overlay_failed = true;
        h.run_steps(4);
        let c = h.state().compare.as_ref().unwrap();
        assert!(!c.overlay_shown && c.overlay_failed);
        let events = events.borrow();
        assert_eq!(events.last(), Some(&CompareEvent::View { id: "c1".into(), view: CompareView::Side, building: false }), "{events:?}");
    }

    #[test]
    fn ppm_overlay_builds_a_page_a_frame_and_going_back_cancels_it() {
        let mut h = ppm_compared(1400.0);
        h.state_mut().show_overlay(true);
        let job = h.state().compare.as_ref().unwrap().overlay_job.as_ref().map(|j| j.progress());
        assert_eq!(job, Some((0, 1)), "started, nothing composed yet");
        h.state_mut().show_overlay(false);
        h.run_steps(4);
        let s = h.state().compare.as_ref().unwrap();
        assert!(s.overlay_job.is_none() && s.overlay.is_none() && !s.overlay_shown, "cancelled");
        assert_eq!(h.state().views.len(), 2, "no overlay tab");
    }

    #[test]
    fn the_screen_watermark_covers_both_compared_files_and_the_overlay_but_no_file() {
        use crate::host_stamp::StampCommand;
        let mut h = ppm_compared(1400.0);
        let saved = |h: &Harness<'static, PdfCraftApp>| -> Vec<std::sync::Arc<Vec<u8>>> {
            h.state().views.iter().map(|v| h.state().session.save_bytes(v.id).unwrap()).collect()
        };
        let before = saved(&h);
        h.state().stamp_inbox.lock().unwrap().push(StampCommand::Watermark { text: Some("CONFIDENTIAL".into()), style: None });
        h.state_mut().show_overlay(true);
        h.run_steps(6);
        let state = h.state();
        assert_eq!(state.views.len(), 2, "old and new; the overlay is no tab");
        let (overlay, _) = state.compare.as_ref().unwrap().overlay.clone().unwrap();
        for id in state.views.iter().map(|v| v.id).chain([overlay]) {
            assert_eq!(state.session.watermark_key(id), Some("CONFIDENTIAL"), "{}", state.session.get(id).unwrap().name);
        }
        assert_eq!(saved(&h)[..2], before[..2], "no file takes it");
    }

    #[test]
    fn opacity_a_and_b_win_over_opacity_whatever_the_key_order() {
        let read = |v: serde_json::Value| overlay_style_from_json(&v).map(|s| (s.strength_chosen, s.strength_current));
        // A is the old file (chosen), B the new (current).
        assert_eq!(read(serde_json::json!({ "opacityA": 0.0, "opacityB": 0.4 })), Ok((0.0, 0.4)));
        assert_eq!(read(serde_json::json!({ "opacityB": 0.4, "opacity": 0.8 })), Ok((0.8, 0.4)));
        assert_eq!(read(serde_json::json!({ "opacityA": 0.1, "opacity": 0.8 })), Ok((0.1, 0.8)));
        assert_eq!(read(serde_json::json!({ "opacity": 0.5 })), Ok((0.5, 0.5)));
        assert_eq!(read(serde_json::json!({})), Ok((1.0, 1.0)));
        for bad in [serde_json::json!({ "opacityA": 2 }), serde_json::json!({ "opacityB": "1" }), serde_json::json!({ "opacityC": 1 })] {
            assert!(read(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_side_colours_are_read_strictly() {
        let c = side_colors_from_json(&serde_json::json!({ "colorA": "#00f", "colorB": "#ff8000" })).unwrap();
        assert_eq!((c.old, c.new), (Some([0, 0, 255]), Some([255, 128, 0])));
        assert_eq!(side_colors_from_json(&serde_json::json!({})).unwrap(), SideColors::default());
        assert_eq!(side_colors_from_json(&serde_json::json!({ "colorB": "#fff" })).unwrap().old, None);
        for bad in [serde_json::json!({ "colorA": "red" }), serde_json::json!({ "colorA": 3 }), serde_json::json!({ "opacity": 0.5 }), serde_json::json!([])] {
            assert!(side_colors_from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_hosts_side_colours_shade_both_files_and_the_viewers_alpha_stays() {
        use crate::compare_ui::ppm;
        let side = SideColors { old: Some([39, 50, 245]), new: None };
        let (mut h, _) = ppm_with(CompareCommand { side, ..command("c1") });
        h.run_steps(2);
        let s = h.state().compare.as_ref().unwrap();
        let (new_v, old_v) = (h.state().views.iter().find(|v| v.id == s.new).unwrap(), h.state().views.iter().find(|v| v.id == s.old).unwrap());
        let blue = egui::Color32::from_rgb(39, 50, 245);
        assert!(!old_v.compare_marks.is_empty() && old_v.compare_marks.iter().all(|m| m.2 == blue), "the old file's words take the host's colour");
        assert!(!new_v.compare_marks.is_empty() && new_v.compare_marks.iter().all(|m| m.2 == ppm::BASE), "the new file keeps the viewer's colour when none is named");
        assert_eq!((new_v.compare_alpha, old_v.compare_alpha), (ppm::MARK_ALPHA, ppm::MARK_ALPHA));
        assert_eq!(s.mark_colors, [blue, ppm::BASE]);
        // No colour named: nothing changes and the strip keeps its colour-name note.
        let (mut h, _) = ppm_with(command("c2"));
        h.run_steps(2);
        let s = h.state().compare.as_ref().unwrap();
        assert!(!s.marks_custom && s.mark_colors == [ppm::TARGET, ppm::BASE]);
    }

    #[test]
    fn the_host_can_open_on_the_old_file_beside_the_overlay_and_the_user_can_switch() {
        let (mut h, events) = ppm_with(CompareCommand { view: CompareView::OverlaySide, ..command("c1") });
        h.run_steps(8);
        let s = h.state().compare.as_ref().unwrap();
        assert_eq!(s.view(), CompareView::OverlaySide);
        assert!(s.overlay_view.is_some(), "the overlay was built");
        let view = |view, building| CompareEvent::View { id: "c1".into(), view, building };
        assert_eq!(events.borrow()[1..], [view(CompareView::OverlaySide, true), view(CompareView::OverlaySide, false)], "{:?}", events.borrow());
        // The old file's shading is only off while the halves are drawn: its marks are still there.
        let old = h.state().compare.as_ref().unwrap().old;
        assert!(!h.state().views.iter().find(|v| v.id == old).unwrap().compare_marks.is_empty());
        h.get_by_label("Overlay").click();
        h.run_steps(4);
        assert_eq!(h.state().compare.as_ref().unwrap().view(), CompareView::Overlay);
        h.get_by_label("Previous + overlay").click();
        h.run_steps(4);
        assert_eq!(h.state().compare.as_ref().unwrap().view(), CompareView::OverlaySide);
        assert_eq!(events.borrow().last(), Some(&view(CompareView::OverlaySide, false)));
        h.get_by_label("Side by side").click();
        h.run_steps(4);
        let s = h.state().compare.as_ref().unwrap();
        assert!(!s.overlay_shown && !s.overlay_beside);
    }

    #[test]
    fn a_locked_view_keeps_the_overlay_beside_button_from_answering() {
        let (mut h, _) = ppm_with(CompareCommand { lock_view: true, ..command("c1") });
        h.run_steps(2);
        h.get_by_label("Previous + overlay").click();
        h.run_steps(4);
        assert_eq!(h.state().compare.as_ref().unwrap().view(), CompareView::Side);
    }

    fn preview(id: &str) -> CompareCommand {
        CompareCommand { op: CompareOp::Preview, ..command(id) }
    }

    fn run(id: &str) -> CompareCommand {
        CompareCommand { op: CompareOp::Run, old: Default::default(), new: Default::default(), ..command(id) }
    }

    #[test]
    fn a_preview_opens_both_files_without_comparing_and_a_run_then_compares_them() {
        let (mut h, events) = ppm_with(preview("p1"));
        h.run_steps(2);
        assert_eq!(*events.borrow(), vec![CompareEvent::PreviewDone { id: "p1".into() }]);
        let s = h.state().compare.as_ref().unwrap();
        assert!(s.preview && s.side_by_side && s.result.changes.is_empty());
        assert_eq!(h.state().right, None, "no change list");
        assert!(h.state().views.iter().all(|v| v.compare_marks.is_empty()), "no shading");
        assert_eq!(h.state().views.len(), 2);
        assert!(h.query_by_label("Overlay").is_none() && h.query_by_label("Side by side").is_none(), "no view switch");
        // The run compares the same two files (no bytes again).
        h.state().compare_inbox.lock().unwrap().push(CompareCommand { view: CompareView::Side, ..run("r1") });
        h.run_steps(4);
        let events = events.borrow();
        assert!(matches!(events[1], CompareEvent::Done { ref id, replaced: 2, .. } if id == "r1"), "{events:?}");
        let s = h.state().compare.as_ref().unwrap();
        assert!(!s.preview && !s.result.changes.is_empty());
        assert_eq!(h.state().views.len(), 2, "the files were not opened again");
        assert!(h.state().views.iter().all(|v| !v.compare_marks.is_empty()));
    }

    #[test]
    fn a_run_without_a_preview_fails_and_so_does_a_second_run_or_a_compare_during_a_preview() {
        let (mut h, events) = ppm_with(run("r0"));
        h.run_steps(2);
        assert!(matches!(&events.borrow()[0], CompareEvent::Failed { id, .. } if id == "r0"), "{:?}", events.borrow());
        assert!(h.state().compare.is_none());
        let (mut h, events) = ppm_with(preview("p1"));
        h.state().compare_inbox.lock().unwrap().push(command("c1"));
        h.run_steps(4);
        assert!(matches!(&events.borrow()[1], CompareEvent::Failed { id, error } if id == "c1" && error.starts_with("busy")), "{:?}", events.borrow());
        assert!(h.state().compare.as_ref().unwrap().preview, "the preview is still there");
        h.state().compare_inbox.lock().unwrap().push(run("r1"));
        h.state().compare_inbox.lock().unwrap().push(run("r2"));
        h.run_steps(4);
        let events = events.borrow();
        assert!(matches!(events[2], CompareEvent::Done { .. }), "{events:?}");
        assert!(matches!(&events[3], CompareEvent::Failed { id, .. } if id == "r2"), "{events:?}");
    }

    #[test]
    fn a_preview_that_cannot_open_is_told_as_a_preview_failure() {
        let (mut h, events) = ppm_with(CompareCommand { old: ("old.pdf".into(), b"not a pdf".to_vec()), ..preview("p1") });
        h.run_steps(2);
        assert!(matches!(&events.borrow()[0], CompareEvent::PreviewFailed { id, .. } if id == "p1"), "{:?}", events.borrow());
        assert!(h.state().compare.is_none());
    }
}
