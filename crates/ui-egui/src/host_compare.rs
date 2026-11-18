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

/// Which request: compare at once, open the two files without comparing (`compare-preview`), compare
/// the files a preview opened (`compare-run`; its `old` and `new` are empty), or move to one change of the
/// comparison (`compare-goto`; only `id` and `change` are read).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Compare,
    Preview,
    Run,
    Goto,
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
    /// `changes` of the message: tell the host the list of differences with the counts.
    pub changes: bool,
    /// `changeList` of the message (default true): false keeps the editor's own list of differences closed and out of the menu.
    pub change_list: bool,
    /// `names` of the message: what the legend and the list call the two files.
    pub names: crate::compare_ui::CompareNames,
    /// `change` of a `compare-goto`: the id (`c1`, `c2`…) of the change to move to.
    pub change: String,
}

/// The most changes (and so the most ids) the host is told; the editor's own list shows as many.
pub const MAX_CHANGES: usize = 2000;
/// The longest `text` of one side of a change, in characters (longer is cut to 199 and an ellipsis).
pub const MAX_CHANGE_TEXT: usize = 200;

/// One side of a change as the host is told: the page (from 0) and the box of the words on it, in the space of
/// `sign-box-selected` (`[x, y, width, height]` in points from the top-left of the page as shown).
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeSide {
    pub page: usize,
    pub rect: [f64; 4],
    pub text: String,
}

/// One difference. `previous` is the old file's side (`None` for an insertion), `current` the new file's
/// (`None` for a deletion).
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeItem {
    pub id: String,
    pub kind: &'static str,
    pub previous: Option<ChangeSide>,
    pub current: Option<ChangeSide>,
}

/// The list of changes `changes: true` asks for; `truncated` when there were more than [`MAX_CHANGES`].
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeList {
    pub items: Vec<ChangeItem>,
    pub truncated: bool,
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

/// `names` of a compare message: `{ previous?, current? }`, each a string of 1 to 20 characters that is not blank.
/// `previous` names the chosen (old) file and `current` the active (new) one.
pub fn names_from_json(v: &serde_json::Value) -> Result<crate::compare_ui::CompareNames, String> {
    let obj = v.as_object().ok_or("names must be an object")?;
    let mut out = crate::compare_ui::CompareNames::default();
    for (key, value) in obj {
        let text = value.as_str().ok_or_else(|| format!("{key} must be a string"))?;
        if text.trim().is_empty() || text.chars().count() > 20 {
            return Err(format!("{key} must be 1 to 20 characters"));
        }
        // Control and bidirectional formatting characters can reorder or hide what the user reads.
        if text.chars().any(|c| c.is_control() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')) {
            return Err(format!("{key} must not hold control or direction characters"));
        }
        match key.as_str() {
            "previous" => out.previous = Some(text.to_string()),
            "current" => out.current = Some(text.to_string()),
            other => return Err(format!("unknown names field {other}")),
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

/// The counts of replaced, inserted, deleted and visual differences, and the list when the host asked for it.
type CompareDone = ((usize, usize, usize, usize), Option<ChangeList>);

/// What the host is told.
#[derive(Clone, Debug, PartialEq)]
pub enum CompareEvent {
    Done { id: String, replaced: usize, inserted: usize, deleted: usize, visual: usize, changes: Option<ChangeList> },
    /// A `compare-goto` moved both files (or the overlay) to the change.
    GotoDone { id: String },
    GotoFailed { id: String, error: String },
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
            } else if command.op == CompareOp::Goto {
                match self.goto_change(&command.change) {
                    Ok(()) => self.emit_compare(CompareEvent::GotoDone { id: cut(&id, MAX_ID_CHARS) }),
                    Err(error) => self.emit_compare(CompareEvent::GotoFailed { id: cut(&id, MAX_ID_CHARS), error: cut(&error, MAX_ERROR_CHARS) }),
                }
            } else if command.op == CompareOp::Run {
                // Only the files a preview opened, and once.
                if !self.compare.as_ref().is_some_and(|c| c.preview) {
                    fail(self, "no preview is open: send compare-preview first");
                    continue;
                }
                match self.run_host_compare(command.overlay, command.side, command.view, command.lock_view, command.changes, command.change_list) {
                    Ok(done) => {
                        self.set_compare_names(command.names);
                        self.report_compare_done(command.id, done)
                    }
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
                    (Ok(()), _) => match self.run_host_compare(command.overlay, command.side, command.view, command.lock_view, command.changes, command.change_list) {
                        Ok(done) => {
                            self.set_compare_names(command.names);
                            self.report_compare_done(command.id, done)
                        }
                        Err(error) => fail(self, &error),
                    },
                }
            }
        }
    }

    /// Tell the host the counts, and the view when it starts on one that builds (so a quick build can't hide it).
    fn report_compare_done(&mut self, id: String, ((replaced, inserted, deleted, visual), changes): CompareDone) {
        self.emit_compare(CompareEvent::Done { id: id.clone(), replaced, inserted, deleted, visual, changes });
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
        changes: bool,
        change_list: bool,
    ) -> Result<CompareDone, String> {
        // The new file is the active one (a preview may have been left on the other tab).
        if let Some(new) = self.compare.as_ref().map(|c| c.new) {
            self.active = self.views.iter().position(|v| v.id == new).or(self.active);
        }
        self.run_compare();
        // A comparison that couldn't run (the engine refused, a tab is gone) leaves the preview as it was: that is a failure, not zero differences.
        if !self.compare.as_ref().is_some_and(|c| !c.preview) {
            return Err("the comparison failed".into());
        }
        if let (Some(style), Some(c)) = (overlay, self.compare.as_mut()) {
            c.overlay_style = style;
        }
        self.apply_side_colors(side);
        if let Some(c) = self.compare.as_mut() {
            c.lock_view = lock_view;
            c.change_list = change_list;
        }
        if !change_list && self.right == Some(crate::RightPanel::Compare) {
            self.right = None;
        }
        if view != CompareView::Side {
            // The same path as the user's Overlay buttons: the build starts and `View` reports it.
            self.set_view(view);
        }
        let Some(s) = self.compare.as_ref() else { return Err("the comparison failed".into()) };
        let counts = (s.result.count(Kind::Replaced), s.result.count(Kind::Inserted), s.result.count(Kind::Deleted), s.visual.len());
        Ok((counts, changes.then(|| self.change_list_for_host())))
    }

    /// The comparison's differences as the host is told them: ids `c1`… in the order of the editor's list, the
    /// first [`MAX_CHANGES`] of them.
    fn change_list_for_host(&self) -> ChangeList {
        let Some(s) = self.compare.as_ref() else { return ChangeList { items: Vec::new(), truncated: false } };
        let (old_doc, new_doc) = (self.session.get(s.old), self.session.get(s.new));
        let side = |doc: Option<&pdfcraft_engine::Document>, side: &pdfcraft_engine::compare::Side| -> ChangeSide {
            // The box that holds the words of the side (a side with none sits by its neighbour).
            let mut boxes = side.rects.iter().copied();
            let first = boxes.next().or(side.near).unwrap_or_default();
            let user = boxes.fold(first, |a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]);
            let rect = doc
                .and_then(|d| d.info.pages.get(side.page))
                .map_or(user, |p| crate::host_annots::user_rect_to_view(p, user))
                .map(|v| (v * 100.0).round() / 100.0);
            let text = if side.text.chars().count() > MAX_CHANGE_TEXT {
                side.text.chars().take(MAX_CHANGE_TEXT - 1).chain(std::iter::once('\u{2026}')).collect()
            } else {
                side.text.clone()
            };
            ChangeSide { page: side.page, rect, text }
        };
        let items = s
            .result
            .changes
            .iter()
            .take(MAX_CHANGES)
            .enumerate()
            .map(|(i, ch)| ChangeItem {
                id: format!("c{}", i + 1),
                kind: match ch.kind {
                    Kind::Replaced => "replaced",
                    Kind::Inserted => "inserted",
                    Kind::Deleted => "deleted",
                },
                previous: (ch.kind != Kind::Inserted).then(|| side(old_doc, &ch.old)),
                current: (ch.kind != Kind::Deleted).then(|| side(new_doc, &ch.new)),
            })
            .collect();
        ChangeList { items, truncated: s.result.changes.len() > MAX_CHANGES }
    }

    /// `compare-goto`: select the change `id` like a click on its row of the editor's list (both files go to it and flash;
    /// the overlay goes to its page).
    fn goto_change(&mut self, id: &str) -> Result<(), String> {
        let Some(s) = self.compare.as_ref().filter(|c| !c.preview) else { return Err("no comparison is finished".into()) };
        let n = id.strip_prefix('c').and_then(|n| n.parse::<usize>().ok()).filter(|n| (1..=MAX_CHANGES).contains(n) && n.to_string() == id[1..]);
        let Some(i) = n.map(|n| n - 1).filter(|i| *i < s.result.changes.len()) else { return Err(format!("unknown change {id}")) };
        let Some(index) = self.views.iter().position(|v| v.id == s.new) else { return Err("the new file is not open".into()) };
        self.select_change(index, i);
        Ok(())
    }

    /// PPM: the host's names of the two files (`names`), shown wherever the comparison names a side.
    fn set_compare_names(&mut self, names: crate::compare_ui::CompareNames) {
        if let Some(c) = self.compare.as_mut().filter(|c| c.ppm) {
            c.names = names;
        }
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
            changes: false,
            change_list: true,
            names: Default::default(),
            change: String::new(),
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
        assert_eq!(*events.borrow(), vec![CompareEvent::Done { id: "c1".into(), replaced: 2, inserted: 0, deleted: 0, visual: events_visual(&h), changes: None }]);
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
    fn a_run_that_cannot_compare_fails_and_leaves_the_preview() {
        let (mut h, events) = ppm_with(preview("p1"));
        h.run_steps(2);
        // The old file's tab is gone from the window's point of view: the engine has nothing to compare.
        h.state_mut().compare_old = None;
        send(&mut h, run("r1"));
        let events = events.borrow();
        assert!(matches!(events.last(), Some(CompareEvent::Failed { id, .. }) if id == "r1"), "{events:?}");
        assert!(!events.iter().any(|e| matches!(e, CompareEvent::Done { .. })), "no compare-done with zero counts: {events:?}");
        assert!(h.state().compare.as_ref().is_some_and(|c| c.preview), "the preview is still open");
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

    fn done_of(events: &Rc<RefCell<Vec<CompareEvent>>>) -> CompareEvent {
        events.borrow().iter().rev().find(|e| matches!(e, CompareEvent::Done { .. })).cloned().expect("a compare-done")
    }

    fn list_of(e: &CompareEvent) -> ChangeList {
        match e {
            CompareEvent::Done { changes: Some(l), .. } => l.clone(),
            other => panic!("no list in {other:?}"),
        }
    }

    #[test]
    fn the_list_of_changes_comes_only_when_asked_and_has_ids_kinds_pages_and_boxes() {
        // Asked for: ids c1.., the three kinds, the side that has no words is None, boxes lie on the page.
        let (mut h, events) = ppm_with(CompareCommand {
            changes: true,
            old: ("old.pdf".into(), text_pdf("Delivery within five days. Old clause removed here. Returns accepted.")),
            new: ("new.pdf".into(), text_pdf("Delivery within three days. Returns accepted. A brand new clause was added.")),
            ..command("c1")
        });
        h.run_steps(2);
        let list = list_of(&done_of(&events));
        assert!(!list.truncated && !list.items.is_empty());
        for (n, c) in list.items.iter().enumerate() {
            assert_eq!(c.id, format!("c{}", n + 1));
            assert_eq!(c.previous.is_none(), c.kind == "inserted", "{c:?}");
            assert_eq!(c.current.is_none(), c.kind == "deleted", "{c:?}");
            for side in [&c.previous, &c.current].into_iter().flatten() {
                assert_eq!(side.page, 0);
                let [x, y, w, hh] = side.rect;
                assert!(x >= 0.0 && y >= 0.0 && w > 0.0 && hh > 0.0 && x + w <= 700.0 && y + hh <= 900.0, "{side:?}");
                assert!(!side.text.is_empty() && side.text.chars().count() <= MAX_CHANGE_TEXT);
            }
        }
        let kinds: Vec<_> = list.items.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&"replaced") && kinds.contains(&"inserted") && kinds.contains(&"deleted"), "{kinds:?}");
        // The counts stay the totals of the comparison.
        let CompareEvent::Done { replaced, inserted, deleted, .. } = done_of(&events) else { unreachable!() };
        assert_eq!(replaced + inserted + deleted, list.items.len());

        // Not asked for: the payload has no list.
        let (_, events) = ppm_with(command("c2"));
        assert!(matches!(done_of(&events), CompareEvent::Done { changes: None, .. }));
    }

    #[test]
    fn a_long_side_is_cut_to_two_hundred_characters_with_an_ellipsis() {
        let long = "word ".repeat(120);
        let (_, events) = ppm_with(CompareCommand {
            changes: true,
            old: ("old.pdf".into(), text_pdf("Start. End.")),
            new: ("new.pdf".into(), text_pdf(&format!("Start. {long}End."))),
            ..command("c1")
        });
        let list = list_of(&done_of(&events));
        let text = &list.items.iter().find_map(|c| c.current.as_ref()).expect("a current side").text;
        assert_eq!(text.chars().count(), MAX_CHANGE_TEXT);
        assert!(text.ends_with('\u{2026}'));
    }

    #[test]
    fn change_list_false_keeps_the_editors_list_closed_and_unselectable() {
        let (mut h, _) = ppm_with(CompareCommand { change_list: false, ..command("c1") });
        assert_eq!(h.state().right, None);
        h.state_mut().choose_right_panel(Some(RightPanel::Compare));
        assert_eq!(h.state().right, None, "the panel can't be chosen");
        assert!(!h.state().compare.as_ref().unwrap().change_list);
        // The default shows it.
        let (h2, _) = ppm_with(command("c2"));
        assert_eq!(h2.state().right, Some(RightPanel::Compare));
    }

    #[test]
    fn a_goto_selects_the_change_like_a_click_on_its_row() {
        let (mut h, events) = ppm_with(CompareCommand { changes: true, ..command("c1") });
        let goto = |id: &str, change: &str| CompareCommand { op: CompareOp::Goto, change: change.into(), old: Default::default(), new: Default::default(), ..command(id) };
        send(&mut h, goto("g1", "c2"));
        assert_eq!(h.state().compare.as_ref().unwrap().selected, Some(1));
        assert!(h.state().views.iter().any(|v| v.flash.is_some()), "a file flashes the change");
        // Unknown ids, ids that aren't ours and ids past the list fail.
        for bad in ["c0", "c99", "x1", "c01", "c", "c-1", ""] {
            send(&mut h, goto("g2", bad));
        }
        let events = events.borrow();
        assert!(matches!(&events[1], CompareEvent::GotoDone { id } if id == "g1"), "{events:?}");
        assert!(events[2..].iter().all(|e| matches!(e, CompareEvent::GotoFailed { .. })), "{events:?}");
        assert_eq!(events.len(), 2 + 7);
    }

    #[test]
    fn a_goto_in_the_overlay_goes_to_the_page_and_without_a_comparison_it_fails() {
        let (mut h, events) = ppm_with(CompareCommand { view: CompareView::Overlay, ..command("c1") });
        let goto = CompareCommand { op: CompareOp::Goto, change: "c1".into(), old: Default::default(), new: Default::default(), ..command("g1") };
        send(&mut h, goto.clone());
        assert!(matches!(events.borrow().last(), Some(CompareEvent::GotoDone { .. })), "{:?}", events.borrow());
        // No comparison at all, and a preview that isn't compared yet.
        let (mut h, events) = ppm_with(preview("p1"));
        send(&mut h, goto);
        assert!(matches!(events.borrow().last(), Some(CompareEvent::GotoFailed { error, .. }) if error.contains("no comparison")), "{:?}", events.borrow());
    }

    #[test]
    fn a_run_takes_changes_and_change_list_like_a_compare() {
        let (mut h, events) = ppm_with(preview("p1"));
        send(&mut h, CompareCommand { changes: true, change_list: false, ..run("r1") });
        let list = list_of(&done_of(&events));
        assert!(!list.items.is_empty());
        assert_eq!(h.state().right, None);
        let (_, events2) = ppm_with(CompareCommand { changes: true, change_list: false, ..command("c1") });
        assert_eq!(list_of(&done_of(&events2)), list, "the same list as a compare of the same files");
    }

    #[test]
    fn the_names_are_read_strictly() {
        let n = names_from_json(&serde_json::json!({ "previous": "비교", "current": "기준" })).unwrap();
        assert_eq!((n.previous.as_deref(), n.current.as_deref()), (Some("비교"), Some("기준")));
        assert_eq!(names_from_json(&serde_json::json!({})).unwrap(), Default::default());
        assert!(names_from_json(&serde_json::json!({ "current": "a".repeat(20) })).is_ok());
        for bad in [
            serde_json::json!({ "previous": "" }),
            serde_json::json!({ "previous": "   " }),
            serde_json::json!({ "current": "a".repeat(21) }),
            serde_json::json!({ "current": 3 }),
            serde_json::json!({ "old": "x" }),
            serde_json::json!([]),
            serde_json::json!({ "previous": "a\nb" }),
            serde_json::json!({ "current": "a\u{85}b" }),
            serde_json::json!({ "current": "a\u{202e}b" }),
            serde_json::json!({ "previous": "\u{2066}a" }),
            serde_json::json!({ "previous": "a\u{200f}" }),
        ] {
            assert!(names_from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_hosts_names_replace_the_viewers_words_for_the_two_files() {
        use egui_kittest::kittest::Queryable;
        let names = crate::compare_ui::CompareNames { previous: Some("Mine".into()), current: Some("Base".into()) };
        // Without names the viewer's own words stay.
        let mut h = ppm_with(command("c1")).0;
        h.run_steps(2);
        assert!(h.get_all_by_label_contains("Old -").count() >= 2 && h.query_all_by_label_contains("Base -").count() == 0);
        // With names the rows head their lines by them, and a run takes them like a compare.
        let (mut h, _) = ppm_with(preview("p1"));
        send(&mut h, CompareCommand { names: names.clone(), ..run("r1") });
        h.run_steps(2);
        assert_eq!(h.state().compare.as_ref().unwrap().names, names);
        assert!(h.get_all_by_label_contains("Base -").count() >= 2 && h.get_all_by_label_contains("Mine +").count() >= 1);
        assert_eq!(h.query_all_by_label_contains("Old -").count(), 0);
        let (mut h, _) = ppm_with(CompareCommand { names: names.clone(), ..command("c2") });
        h.run_steps(2);
        assert_eq!(h.state().compare.as_ref().unwrap().names, names);
    }

    #[test]
    fn the_legend_and_the_notes_use_the_names_and_a_missing_one_keeps_the_viewers_word() {
        use crate::compare_ui::CompareNames;
        let both = CompareNames { previous: Some("Mine".into()), current: Some("Base".into()) };
        assert_eq!(crate::compare_ui::lines_only(&both, true), "Lines only in Base");
        assert_eq!(crate::compare_ui::lines_only(&both, false), "Lines only in Mine");
        assert_eq!(crate::compare_ui::words_only(&both, false), "Words only in Mine or changed");
        let one = CompareNames { previous: None, current: Some("Base".into()) };
        assert_eq!(crate::compare_ui::lines_only(&one, false), "Lines only in the chosen file");
        assert_eq!(crate::compare_ui::lines_only(&one, true), "Lines only in Base");
    }
}
