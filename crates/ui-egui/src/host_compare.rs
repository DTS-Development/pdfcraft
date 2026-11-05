//! A comparison the embedding page starts (web `pdfcraft:compare`).
//!
//! The host hands in both files: the old one opens in a background tab, the new one becomes the
//! active tab, and the same comparison as Compare Files runs. The host is told the counts.
//! A frame compares once; later requests are answered with "busy".

use std::sync::Arc;

use pdfcraft_engine::compare::Kind;

use crate::PdfCraftApp;

const MAX_ID_CHARS: usize = 128;
const MAX_ERROR_CHARS: usize = 200;

/// The most bytes either file may have; the bridge checks it before copying into memory.
pub const MAX_COMPARE_BYTES: usize = 200 << 20;

/// What the host asks for: `(name, bytes)` of each file, and the rules both open under (its
/// `mode` and `policy`, read like an open's; the narrowest when it names none).
#[derive(Clone, Debug, PartialEq)]
pub struct CompareCommand {
    pub id: String,
    pub old: (String, Vec<u8>),
    pub new: (String, Vec<u8>),
    pub ownership: pdfcraft_engine::Ownership,
}

/// What the host is told.
#[derive(Clone, Debug, PartialEq)]
pub enum CompareEvent {
    Done { id: String, replaced: usize, inserted: usize, deleted: usize, visual: usize },
    Failed { id: String, error: String },
    /// PPM: which view the comparison shows (side by side or the overlay) and whether the overlay
    /// is still being built; sent after `Done` and again whenever either changes, so the host can
    /// word its status line like the viewer's.
    View { id: String, overlay: bool, building: bool },
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
        let Some((id, sent)) = self.compare_view_sent.as_ref() else { return };
        let Some(c) = self.compare.as_ref().filter(|c| c.ppm) else { return };
        let now = (c.overlay_shown, c.overlay_job.is_some());
        if now == *sent {
            return;
        }
        let event = CompareEvent::View { id: id.clone(), overlay: now.0, building: now.1 };
        self.emit_compare(event);
        if let Some((_, sent)) = self.compare_view_sent.as_mut() {
            *sent = now;
        }
    }

    fn fail_compare(&self, id: &str, error: &str) {
        self.emit_compare(CompareEvent::Failed { id: cut(id, MAX_ID_CHARS), error: cut(error, MAX_ERROR_CHARS) });
    }

    /// Once a frame: take the host's requests and run the first one.
    pub(crate) fn host_compare_tick(&mut self) {
        self.report_compare_view();
        let commands: Vec<CompareCommand> = self.compare_inbox.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for command in commands {
            if command.id.is_empty() || command.id.chars().count() > MAX_ID_CHARS {
                self.fail_compare(&command.id, "the request id is missing or too long");
            } else if self.compare_taken {
                self.fail_compare(&command.id, "busy: this window already compares");
            } else {
                self.compare_taken = true;
                match self.start_host_compare(command.old, command.new, command.ownership) {
                    Ok((replaced, inserted, deleted, visual)) => {
                        self.emit_compare(CompareEvent::Done { id: command.id.clone(), replaced, inserted, deleted, visual });
                        // The host starts from side by side, so only a change from that is told.
                        self.compare_view_sent = Some((command.id, (false, false)));
                    }
                    Err(error) => self.fail_compare(&command.id, &error),
                }
            }
        }
    }

    /// Open both files (old in the background, new active) and compare them: the counts of
    /// replaced, inserted, deleted and visual differences.
    fn start_host_compare(
        &mut self,
        old: (String, Vec<u8>),
        new: (String, Vec<u8>),
        ownership: pdfcraft_engine::Ownership,
    ) -> Result<(usize, usize, usize, usize), String> {
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
        self.run_compare();
        let Some(s) = self.compare.as_ref() else { return Err("the comparison failed".into()) };
        Ok((s.result.count(Kind::Replaced), s.result.count(Kind::Inserted), s.result.count(Kind::Deleted), s.visual.len()))
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

    fn text_pdf(text: &str) -> Vec<u8> {
        Session::new().create_from_text("t", text).unwrap().to_vec()
    }

    fn command(id: &str) -> CompareCommand {
        CompareCommand {
            id: id.into(),
            old: ("old.pdf".into(), text_pdf("Delivery within five days. Returns accepted.")),
            new: ("new.pdf".into(), text_pdf("Delivery within three days. Returns accepted for a week.")),
            ownership: Default::default(),
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
        let rows: Vec<_> = h.get_all_by_label_contains("Change  ").collect();
        assert!(rows.len() >= 2, "two changes, two rows");
        rows[1].click();
        h.run_steps(3);
        assert_eq!(h.state().compare.as_ref().unwrap().selected, Some(1));
        let rows: Vec<_> = h.get_all_by_label_contains("Change  ").collect();
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
        let view = |overlay, building| CompareEvent::View { id: "c1".into(), overlay, building };
        h.state_mut().show_overlay(true);
        h.run_steps(6);
        h.state_mut().show_overlay(false);
        h.run_steps(2);
        let events = events.borrow();
        assert_eq!(events[1..], [view(true, true), view(true, false), view(false, false)], "{events:?}");
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
        h.state().stamp_inbox.lock().unwrap().push(StampCommand::Watermark { text: Some("CONFIDENTIAL".into()) });
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
}
