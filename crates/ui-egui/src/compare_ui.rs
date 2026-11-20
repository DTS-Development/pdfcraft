//! Compare files: choose the older version, then the Compare panel lists the differences
//! (replaced blue, inserted green, deleted red), shades them on the newer document's pages, and
//! offers a report or the differences as comments.

use egui::{Align, Color32, Layout};
use pdfcraft_engine::DocId;
use pdfcraft_engine::compare::{Comparison, Kind};

use crate::theme::{self, Tokens};
use crate::{PdfCraftApp, RightPanel, widgets};

/// The last comparison.
pub struct CompareState {
    pub old: DocId,
    pub new: DocId,
    pub result: Comparison,
    /// Regions that look different (page, user-space box), page n against page n.
    pub visual: Vec<(usize, [f64; 4])>,
    pub selected: Option<usize>,
    /// Side by side: the old file on the left, the new on the right, scrolled and zoomed
    /// together.
    pub side_by_side: bool,
    /// Which half leads the other (the one the pointer was over last): 0 old, 1 new.
    pub leader: usize,
    /// Each half's scroll offset last frame, to tell which one moved.
    pub prev_scroll: [egui::Vec2; 2],
    /// The PPM (host-lock) build: the viewer's compare screen (see [`ppm`]).
    pub ppm: bool,
    /// PPM: the overlay is shown instead of the two halves.
    pub overlay_shown: bool,
    /// PPM: the overlay being built, one page pair a frame (dropping it cancels the build).
    pub overlay_job: Option<pdfcraft_engine::compare::OverlayJob>,
    /// PPM: the overlay document (a background tab) and the note about unequal page counts.
    pub overlay: Option<(DocId, Option<String>)>,
    /// PPM: the overlay's page view. It is no tab: the view is kept here and only stands in the
    /// app's views for the moment the overlay pane is drawn or rendered (see [`with_overlay_view`]).
    pub overlay_view: Option<crate::canvas::DocView>,
    /// PPM: the overlay couldn't be built.
    pub overlay_failed: bool,
    /// PPM: the host's overlay colours (the viewer's own when it names none).
    pub overlay_style: pdfcraft_engine::compare::OverlayStyle,
    /// PPM: the host fixed the view (`lockView`): the user can't switch between side by side and the overlay.
    pub lock_view: bool,
    /// PPM: the overlay is shown beside the old file (`overlay-side`) rather than alone; only meaningful with `overlay_shown`.
    pub overlay_beside: bool,
    /// PPM: the two files are open side by side and nothing is compared yet (`compare-preview`):
    /// no shading, no change list, no view switch.
    pub preview: bool,
    /// PPM: the shading colours of the old and the new file's words (the viewer's own unless the host named them).
    pub mark_colors: [Color32; 2],
    /// PPM: the host named a shading colour, so the strip's note shows colour dots rather than colour names.
    pub marks_custom: bool,
    /// The host kept the editor's own list of differences away (`changeList: false`).
    pub change_list: bool,
    /// PPM: the host's names of the two files for the legend, the colour note, the rows and the page note.
    pub names: CompareNames,
}

/// The names the host gave the two files of a PPM comparison (`names` of the message). None keeps the viewer's own wording.
/// `previous` is the chosen (old) file, `current` the active (new) file the viewer calls the base.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompareNames {
    pub previous: Option<String>,
    pub current: Option<String>,
}

impl CompareNames {
    /// The current file's name where a sentence needs one: the host's, else the viewer's word for it.
    fn current_or_default(&self) -> String {
        self.current.clone().unwrap_or_else(|| tl_ctx!(ppm::CTX, "the current file").to_string())
    }

    /// The chosen file's name, likewise.
    fn previous_or_default(&self) -> String {
        self.previous.clone().unwrap_or_else(|| tl_ctx!(ppm::CTX, "the chosen file").to_string())
    }

    fn is_empty(&self) -> bool {
        self.previous.is_none() && self.current.is_none()
    }
}

/// What a PPM comparison shows: the two halves, the overlay alone, or the old file beside the overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareView {
    Side,
    Overlay,
    OverlaySide,
}

impl CompareState {
    /// The two files open side by side and nothing compared yet (`compare-preview`).
    pub fn preview(old: DocId, new: DocId, ppm: bool) -> Self {
        Self {
            old,
            new,
            result: Comparison::default(),
            visual: Vec::new(),
            selected: None,
            side_by_side: true,
            leader: 1,
            prev_scroll: [egui::Vec2::ZERO; 2],
            ppm,
            overlay_shown: false,
            overlay_job: None,
            overlay: None,
            overlay_view: None,
            overlay_failed: false,
            overlay_style: Default::default(),
            lock_view: false,
            overlay_beside: false,
            preview: true,
            mark_colors: [ppm::TARGET, ppm::BASE],
            marks_custom: false,
            change_list: true,
            names: CompareNames::default(),
        }
    }

    pub fn view(&self) -> CompareView {
        match (self.overlay_shown, self.overlay_beside) {
            (false, _) => CompareView::Side,
            (true, false) => CompareView::Overlay,
            (true, true) => CompareView::OverlaySide,
        }
    }
}

/// The PPM build follows the viewer's compare screen: the current file on the left, the chosen
/// one on the right, words shaded red (only in the current file or changed) and green (only in
/// the chosen file or changed) like the viewer's semantic diff, no visual marks, and an overlay
/// of the pages (current red, chosen blue, same gray).
pub mod ppm {
    use egui::Color32;

    /// The viewer's semantic-diff colours (WebViewer's defaults) and how strongly they are shaded.
    pub const BASE: Color32 = Color32::from_rgb(255, 73, 73);
    pub const TARGET: Color32 = Color32::from_rgb(21, 205, 131);
    pub const MARK_ALPHA: f32 = 0.4;
    /// Height of the strip above the pages: the view buttons, the legend and a note.
    pub const STRIP_H: f32 = 50.0;
    /// Overlay pages are drawn at this dpi.
    pub const OVERLAY_DPI: f32 = 100.0;
    /// Context of the strings below in the catalogs.
    pub const CTX: &str = "compare viewer";

    pub fn rgb(c: [u8; 3]) -> Color32 {
        Color32::from_rgb(c[0], c[1], c[2])
    }
}

/// Visual differences are shaded orange.
pub const VISUAL: Color32 = Color32::from_rgb(0xF0, 0x8C, 0x1A);

/// What the panel asks for.
pub enum PanelAction {
    Select(usize),
    Mark,
    Report,
    Clear,
    SideBySide(bool),
}

pub fn colour(kind: Kind) -> Color32 {
    let [r, g, b] = pdfcraft_engine::compare::colour(kind);
    Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// The Compare Files dialog: pick the older document. Returns (compare, cancel).
pub(crate) fn body(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &Tokens) -> (bool, bool) {
    ui.label(egui::RichText::new(tl!("Compare Files")).font(theme::semibold(18.0)));
    ui.add_space(8.0);
    let Some((_, new)) = app.active_ids() else { return (false, true) };
    let others: Vec<(DocId, String)> =
        app.views.iter().filter(|v| v.id != new).filter_map(|v| app.session.get(v.id).map(|d| (d.id, d.name.clone()))).collect();
    if app.compare_old.is_none_or(|o| !others.iter().any(|(id, _)| *id == o)) {
        app.compare_old = others.first().map(|(id, _)| *id);
    }
    let new_name = app.session.get(new).map(|d| d.name.clone()).unwrap_or_default();
    egui::Grid::new("compare-files").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
        ui.label(tl!("Old file:"));
        let shown =
            others.iter().find(|(id, _)| Some(*id) == app.compare_old).map_or(tl!("Open the older version first").to_string(), |(_, n)| n.clone());
        egui::ComboBox::from_id_salt("compare-old").selected_text(shown).width(320.0).show_ui(ui, |ui| {
            for (id, n) in &others {
                ui.selectable_value(&mut app.compare_old, Some(*id), n);
            }
        });
        ui.end_row();
        ui.label(tl!("New file:"));
        ui.label(egui::RichText::new(new_name).strong());
        ui.end_row();
    });
    ui.add_space(4.0);
    ui.label(egui::RichText::new(tl!("Text is compared word by word; the differences are shown on the new file.")).small().color(t.text_muted));
    ui.add_space(10.0);
    let (mut go, mut cancel) = (false, false);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add_enabled_ui(app.compare_old.is_some(), |ui| widgets::pill_button(ui, tl!("Compare"), true)).inner.clicked() {
                go = true;
            }
            if widgets::pill_button(ui, tl!("Cancel"), false).clicked() {
                cancel = true;
            }
        })
    });
    (go, cancel)
}

/// The Compare panel.
pub(crate) fn panel(ui: &mut egui::Ui, t: &Tokens, state: &Option<CompareState>, id: DocId, hosted: bool) -> Option<PanelAction> {
    let Some(s) = state.as_ref().filter(|s| s.new == id) else {
        ui.label(egui::RichText::new(tl!("Compare this file with an older version: Compare files… in All tools.")).color(t.text_muted));
        return None;
    };
    if s.preview {
        ui.label(egui::RichText::new(tl!("Compare this file with an older version: Compare files… in All tools.")).color(t.text_muted));
        return None;
    }
    if s.ppm {
        return ppm_panel(ui, t, s);
    }
    let mut action = None;
    let c = &s.result;
    if c.identical() {
        ui.label(tl!("No differences in the text."));
    }
    ui.horizontal_wrapped(|ui| {
        for k in [Kind::Replaced, Kind::Inserted, Kind::Deleted] {
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().rect_filled(dot, egui::CornerRadius::same(2), colour(k));
            ui.label(crate::i18n::fmt(tl!("{n} {kind}"), &[("n", &c.count(k).to_string()), ("kind", tl!(k.label()))]));
        }
        let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().rect_filled(dot, egui::CornerRadius::same(2), VISUAL);
        ui.label(crate::i18n::fmt(tl!("{n} {kind}"), &[("n", &s.visual.len().to_string()), ("kind", tl!("Visual"))]));
    });
    ui.add_space(6.0);
    // Side by side, or the new file alone.
    ui.horizontal(|ui| {
        if widgets::pill_button(ui, tl!("Side by side"), s.side_by_side).clicked() {
            action = Some(PanelAction::SideBySide(true));
        }
        if widgets::pill_button(ui, tl!("New file only"), !s.side_by_side).clicked() {
            action = Some(PanelAction::SideBySide(false));
        }
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        // Marks would edit the host's document and a report is a new tab the host can't save.
        if !hosted {
            if ui.button(tl!("Mark as comments")).clicked() {
                action = Some(PanelAction::Mark);
            }
            if ui.button(tl!("Report…")).clicked() {
                action = Some(PanelAction::Report);
            }
        }
        if ui.button(tl!("Clear")).clicked() {
            action = Some(PanelAction::Clear);
        }
    });
    ui.separator();
    for (i, ch) in c.changes.iter().enumerate().take(2000) {
        let text = match ch.kind {
            Kind::Replaced => format!("\"{}\" → \"{}\"", ch.old.text, ch.new.text),
            Kind::Inserted => format!("+ \"{}\"", ch.new.text),
            Kind::Deleted => format!("− \"{}\"", ch.old.text),
        };
        let selected = s.selected == Some(i);
        let r = ui.push_id(i, |ui| {
            ui.horizontal(|ui| {
                let (bar, _) = ui.allocate_exact_size(egui::vec2(4.0, 34.0), egui::Sense::hover());
                ui.painter().rect_filled(bar, egui::CornerRadius::same(2), colour(ch.kind));
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(crate::i18n::fmt(
                            tl!("{kind} · page {p}"),
                            &[("kind", tl!(ch.kind.label())), ("p", &(ch.new.page + 1).to_string())],
                        ))
                        .small()
                        .color(t.text_muted),
                    );
                    ui.selectable_label(selected, text)
                })
                .inner
            })
            .inner
        });
        if r.inner.clicked() {
            action = Some(PanelAction::Select(i));
        }
    }
    action
}

/// One row of the PPM change list: page, kind and the two sides' words, one line each.
const PPM_ROW_H: f32 = 76.0;
/// The most changes the list shows.
const PPM_ROWS_MAX: usize = 2000;

/// The rows of equal height `row_h` that touch the clip rect between `clip_min` and `clip_max`,
/// for a list whose first row starts at `top`.
fn visible_rows(top: f32, clip_min: f32, clip_max: f32, row_h: f32, n: usize) -> std::ops::Range<usize> {
    let first = (((clip_min - top) / row_h).floor().max(0.0) as usize).min(n);
    let end = (((clip_max - top) / row_h).ceil().max(0.0) as usize).clamp(first, n);
    first..end
}

/// The PPM panel: the viewer's change list, grouped by page. What only the current file has
/// (the viewer's "base") is a deletion, what only the chosen file has an insertion.
fn ppm_panel(ui: &mut egui::Ui, t: &Tokens, s: &CompareState) -> Option<PanelAction> {
    let c = &s.result;
    let mut action = None;
    ui.label(crate::i18n::fmt(tl_ctx!(ppm::CTX, "{n} differences"), &[("n", &c.changes.len().to_string())]));
    if c.identical() {
        ui.label(tl!("No differences in the text."));
    }
    ui.separator();
    let words = |text: &str| text.split_whitespace().count().max(1);
    // Every row is as tall as the others (long words are cut to one line), so only the rows in
    // view are laid out: the rows above and below are plain space.
    let n = c.changes.len().min(PPM_ROWS_MAX);
    let width = ui.available_width();
    let clip = ui.clip_rect();
    let rows = visible_rows(ui.cursor().min.y, clip.min.y, clip.max.y, PPM_ROW_H, n);
    ui.add_space(rows.start as f32 * PPM_ROW_H);
    ui.spacing_mut().item_spacing.y = 0.0;
    for i in rows.clone() {
        let ch = &c.changes[i];
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, PPM_ROW_H), egui::Sense::click());
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(4.0)));
        if s.selected == Some(i) {
            ui.painter().rect_filled(rect, 4.0, t.selected);
        }
        let (kind, bar) = match ch.kind {
            Kind::Inserted => (tl_ctx!(ppm::CTX, "Delete"), s.mark_colors[1]),
            Kind::Deleted => (tl_ctx!(ppm::CTX, "Insert"), s.mark_colors[0]),
            Kind::Replaced => (tl_ctx!(ppm::CTX, "Edit"), t.border),
        };
        ui.painter().rect_filled(egui::Rect::from_min_size(rect.min + egui::vec2(4.0, 4.0), egui::vec2(4.0, PPM_ROW_H - 8.0)), 2.0, bar);
        row.horizontal(|ui| {
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                // The page heads its first change.
                let first = i == 0 || c.changes[i - 1].new.page != ch.new.page;
                let head = if first {
                    crate::i18n::fmt(tl_ctx!(ppm::CTX, "Page {n}"), &[("n", &(ch.new.page + 1).to_string())])
                } else {
                    String::new()
                };
                ui.add(egui::Label::new(egui::RichText::new(head).small().color(t.text_muted)).truncate().selectable(false));
                ui.add(egui::Label::new(egui::RichText::new(kind).strong()).truncate().selectable(false));
                // "Old" is the current file's side and "New" the chosen file's, as in the viewer.
                let side = |ui: &mut egui::Ui, show: bool, mark: &str, label: &str, text: &str| {
                    let line = if show { format!("{label} {mark}{}: {text}", words(text)) } else { String::new() };
                    ui.add(egui::Label::new(egui::RichText::new(line).small()).truncate().selectable(false));
                };
                let (current, previous) = (s.names.current.as_deref().unwrap_or(tl_ctx!(ppm::CTX, "Old")), s.names.previous.as_deref().unwrap_or(tl_ctx!(ppm::CTX, "New")));
                side(ui, ch.kind != Kind::Deleted, "-", current, &ch.new.text);
                side(ui, ch.kind != Kind::Inserted, "+", previous, &ch.old.text);
            });
        });
        if resp.clicked() {
            action = Some(PanelAction::Select(i));
        }
    }
    ui.add_space(n.saturating_sub(rows.end) as f32 * PPM_ROW_H);
    action
}

impl PdfCraftApp {
    /// Compare the chosen older document with the active one and show the differences.
    pub fn run_compare(&mut self) {
        let Some((i, new)) = self.active_ids() else { return };
        let Some(old) = self.compare_old else { return };
        match self.session.compare(old, new) {
            Ok(result) => {
                let ppm = self.host_lock;
                // The PPM build shows words only, like the viewer's semantic diff.
                let visual = if ppm { Vec::new() } else { self.session.compare_visual(old, new, 72.0).unwrap_or_default() };
                self.views[i].compare_marks = result
                    .changes
                    .iter()
                    .flat_map(|ch| ch.new.rects.iter().map(move |r| (ch.new.page, r.map(|v| v as f32), if ppm { ppm::BASE } else { colour(ch.kind) })))
                    .collect();
                if ppm {
                    self.views[i].compare_alpha = ppm::MARK_ALPHA;
                }
                // Visual differences not already covered by a text change.
                let text_marks = self.views[i].compare_marks.clone();
                for (p, r) in &visual {
                    let r = r.map(|v| v as f32);
                    let covered = text_marks.iter().any(|(mp, m, _)| *mp == *p && m[0] <= r[2] && r[0] <= m[2] && m[1] <= r[3] && r[1] <= m[3]);
                    if !covered {
                        self.views[i].compare_marks.push((*p, r, VISUAL));
                    }
                }
                // The old file's pages too: what was deleted or replaced, and the visual
                // differences, so both versions read with their own shading.
                if let Some(oi) = self.views.iter().position(|v| v.id == old) {
                    let mut marks: Vec<(usize, [f32; 4], Color32)> = result
                        .changes
                        .iter()
                        .filter(|ch| ch.kind != Kind::Inserted)
                        .flat_map(|ch| ch.old.rects.iter().map(move |r| (ch.old.page, r.map(|v| v as f32), if ppm { ppm::TARGET } else { colour(ch.kind) })))
                        .collect();
                    for (p, r) in &visual {
                        let r = r.map(|v| v as f32);
                        let covered = marks.iter().any(|(mp, m, _)| *mp == *p && m[0] <= r[2] && r[0] <= m[2] && m[1] <= r[3] && r[1] <= m[3]);
                        if !covered {
                            marks.push((*p, r, VISUAL));
                        }
                    }
                    self.views[oi].compare_marks = marks;
                    if ppm {
                        self.views[oi].compare_alpha = ppm::MARK_ALPHA;
                    }
                }
                let n = result.changes.len();
                // Side by side when the old version is open in a tab and the window has the room.
                // The PPM build always does, whatever the width (the viewer has no such limit).
                let side_by_side = self.views.iter().any(|v| v.id == old) && (ppm || self.ctx.as_ref().is_none_or(|c| c.content_rect().width() >= 1000.0));
                self.end_compare();
                self.compare = Some(crate::compare_ui::CompareState {
                    old,
                    new,
                    result,
                    visual,
                    selected: None,
                    side_by_side,
                    leader: 1,
                    prev_scroll: [egui::Vec2::ZERO; 2],
                    ppm,
                    overlay_shown: false,
                    overlay_job: None,
                    overlay: None,
                    overlay_view: None,
                    overlay_failed: false,
                    overlay_style: Default::default(),
                    lock_view: false,
                    overlay_beside: false,
                    preview: false,
                    mark_colors: [ppm::TARGET, ppm::BASE],
                    marks_custom: false,
                    change_list: true,
                    names: CompareNames::default(),
                });
                self.right = Some(RightPanel::Compare);
                self.notify(if n == 0 {
                    tl!("The text is the same").to_string()
                } else if n == 1 {
                    tl!("1 difference found").to_string()
                } else {
                    crate::i18n::fmt(tl!("{n} differences found"), &[("n", &n.to_string())])
                });
            }
            Err(e) => self.notify_error(e),
        }
    }

    /// Select change `i`: both files go to it and flash (the panel's row click and the host's `compare-goto`).
    pub(crate) fn select_change(&mut self, index: usize, i: usize) {
        let Some(s) = self.compare.as_mut() else { return };
        let old = s.old;
        s.selected = Some(i);
        let Some(ch) = s.result.changes.get(i) else { return };
        let rect = ch.new.rects.first().copied().or(ch.new.near).unwrap_or_default();
        let page = ch.new.page;
        let old_target = (ch.old.page, ch.old.rects.first().copied().or(ch.old.near).unwrap_or_default());
        // The overlay has the pages only, so it goes to the page.
        if s.overlay_shown
            && let Some(v) = s.overlay_view.as_mut()
        {
            v.go_to_page(page);
        }
        let v = &mut self.views[index];
        v.go_to_page(page);
        v.flash = Some((page, rect.map(|x| x as f32), 0.0));
        // The old file's half jumps to its side of the change too.
        if let Some(oi) = self.views.iter().position(|v| v.id == old) {
            let v = &mut self.views[oi];
            v.go_to_page(old_target.0);
            v.flash = Some((old_target.0, old_target.1.map(|x| x as f32), 0.0));
        }
    }

    pub(crate) fn compare_action(&mut self, index: usize, a: PanelAction) {
        let Some(s) = self.compare.as_mut() else { return };
        let (old, new) = (s.old, s.new);
        match a {
            PanelAction::Select(i) => self.select_change(index, i),
            PanelAction::SideBySide(on) => {
                s.side_by_side = on;
                s.leader = 1;
            }
            PanelAction::Mark => match self.session.mark_differences(old, new) {
                Ok(n) => {
                    if let Some(info) = self.session.get(new).map(|d| d.info.clone()) {
                        self.views[index].document_changed(&info);
                    }
                    self.notify(if n == 1 {
                        tl!("Added 1 comment").to_string()
                    } else {
                        crate::i18n::fmt(tl!("Added {n} comments"), &[("n", &n.to_string())])
                    });
                }
                Err(e) => self.notify_error(e),
            },
            PanelAction::Report => match self.session.compare_report(old, new) {
                Ok(bytes) => {
                    // Only the top bar's look carries over from the compared file; the rules stay as they are.
                    let previous = self.session.open_ownership().clone();
                    if let Some(own) = self.session.get(new).map(|d| d.ownership().clone()) {
                        self.session.set_open_ownership(pdfcraft_engine::Ownership {
                            hide_menu_button: own.hide_menu_button,
                            show_zoom_controls: own.show_zoom_controls,
                            ..previous.clone()
                        });
                    }
                    let opened = self.open_bytes("Compare Report.pdf", None, bytes.to_vec());
                    self.session.set_open_ownership(previous);
                    if let Err(e) = opened {
                        self.notify_error(e);
                    }
                }
                Err(e) => self.notify_error(e),
            },
            PanelAction::Clear => {
                self.end_compare();
                self.views[index].compare_marks.clear();
                if let Some(oi) = self.views.iter().position(|v| v.id == old) {
                    self.views[oi].compare_marks.clear();
                }
                self.right = None;
            }
        }
    }
}

impl PdfCraftApp {
    /// PPM: show the overlay (its build starts the first time) or the two halves again, which
    /// also cancels a build that is still going.
    pub(crate) fn show_overlay(&mut self, on: bool) {
        let Some(c) = self.compare.as_mut() else { return };
        if c.preview {
            return;
        }
        c.overlay_shown = on;
        if !on {
            c.overlay_job = None;
            c.overlay_beside = false;
            return;
        }
        if c.overlay.is_some() || c.overlay_failed || c.overlay_job.is_some() {
            return;
        }
        match self.session.compare_overlay_job(c.new, c.old, ppm::OVERLAY_DPI) {
            Ok(mut job) => {
                job.set_style(c.overlay_style);
                c.overlay_job = Some(job);
            }
            Err(e) => {
                c.overlay_failed = true;
                self.notify_error(e);
            }
        }
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }

    /// PPM: show `view` (the host's `view`, or the strip's buttons); the overlay is built once and kept.
    pub(crate) fn set_view(&mut self, view: CompareView) {
        let Some(c) = self.compare.as_mut() else { return };
        if c.preview {
            return;
        }
        c.overlay_beside = view == CompareView::OverlaySide;
        self.show_overlay(view != CompareView::Side);
    }

    /// PPM: one more page pair of the overlay a frame, so the window keeps drawing and the
    /// build can be cancelled. The last step makes the PDF and opens it.
    fn step_overlay(&mut self) {
        let Some(c) = self.compare.as_mut() else { return };
        let Some(job) = c.overlay_job.as_mut() else { return };
        if let Err(e) = job.step() {
            c.overlay_job = None;
            c.overlay_failed = true;
            self.notify_error(e);
            return;
        }
        if !job.is_done() {
            if let Some(ctx) = &self.ctx {
                ctx.request_repaint();
            }
            return;
        }
        if let Some(job) = c.overlay_job.take() {
            self.finish_overlay(job);
        }
    }

    /// PPM: the pages of both files laid over each other (the current file red, the chosen blue,
    /// the same gray) as a PDF in a background tab, built once. The current file is the viewer's
    /// base, so it is the red one.
    fn finish_overlay(&mut self, job: pdfcraft_engine::compare::OverlayJob) {
        let Some(c) = self.compare.as_ref() else { return };
        let new = c.new;
        let names = c.names.clone();
        let built = job.finish(&self.session).map_err(|e| e.to_string());
        let outcome = built.and_then(|o| {
            // Looked at only: read-only and never synced, like the files it was made from.
            // What later opens use, read before it is narrowed for this one.
            let previous = self.session.open_ownership().clone();
            let own = self.session.get(new).map(|d| d.ownership().clone());
            if let Some(own) = own {
                self.session.set_open_ownership(pdfcraft_engine::Ownership { read_only: true, annot_sync: false, hide_comments_panel: false, ..own });
            }
            let before = self.views.len();
            // The PDF is the only holder of its bytes here, so they move into the tab without a copy.
            let bytes = std::sync::Arc::try_unwrap(o.pdf).unwrap_or_else(|shared| (*shared).clone());
            let opened = self.open_bytes_with_focus("compare.pdf", None, bytes, false);
            self.session.set_open_ownership(previous);
            opened?;
            if self.views.len() != before + 1 {
                return Err("couldn't open the overlay".to_string());
            }
            // The overlay is no tab of the window: its view leaves the tab list at once.
            let Some(view) = self.views.pop() else { return Err("couldn't open the overlay".to_string()) };
            let id = view.id;
            self.session.set_hide_replies(id, true);
            let notice = (o.current_pages != o.chosen_pages).then(|| {
                let (n, a, b) = (o.current_pages.min(o.chosen_pages).to_string(), o.current_pages.to_string(), o.chosen_pages.to_string());
                if names.is_empty() {
                    crate::i18n::fmt(tl_ctx!(ppm::CTX, "Only the first {n} pages were compared (current {a}, chosen {b})."), &[("n", &n), ("a", &a), ("b", &b)])
                } else {
                    let (an, bn) = (names.current_or_default(), names.previous_or_default());
                    crate::i18n::fmt(tl_ctx!(ppm::CTX, "Only the first {n} pages were compared ({an} {a}, {bn} {b})."), &[("n", &n), ("an", &an), ("a", &a), ("bn", &bn), ("b", &b)])
                }
            });
            Ok((id, notice, view))
        });
        if let Some(c) = self.compare.as_mut() {
            match outcome {
                Ok((id, notice, view)) => {
                    c.overlay = Some((id, notice));
                    c.overlay_view = Some(view);
                }
                Err(e) => {
                    c.overlay_failed = true;
                    self.notify_error(e);
                }
            }
        }
    }
}

/// The legend's text for the lines only one file has: the host's name for it, else the viewer's own wording.
pub(crate) fn lines_only(names: &CompareNames, current: bool) -> String {
    match (if current { &names.current } else { &names.previous }, current) {
        (Some(n), _) => crate::i18n::fmt(tl_ctx!(ppm::CTX, "Lines only in {name}"), &[("name", n)]),
        (None, true) => tl_ctx!(ppm::CTX, "Lines only in the current file").to_string(),
        (None, false) => tl_ctx!(ppm::CTX, "Lines only in the chosen file").to_string(),
    }
}

/// The note's text for the words only one file has (or that changed), likewise.
pub(crate) fn words_only(names: &CompareNames, current: bool) -> String {
    match (if current { &names.current } else { &names.previous }, current) {
        (Some(n), _) => crate::i18n::fmt(tl_ctx!(ppm::CTX, "Words only in {name} or changed"), &[("name", n)]),
        (None, true) => tl_ctx!(ppm::CTX, "Words only in the current file or changed").to_string(),
        (None, false) => tl_ctx!(ppm::CTX, "Words only in the chosen file or changed").to_string(),
    }
}

/// The legend's dots: the colours the overlay is drawn in (the host's, or the viewer's own), then the same-place grey.
fn legend_colours(style: &pdfcraft_engine::compare::OverlayStyle) -> [[u8; 3]; 3] {
    [style.current, style.chosen, pdfcraft_engine::compare::OVERLAY_SAME]
}

/// PPM: the strip above the pages, like the viewer's header row and legend. Returns the view the
/// user asked for, when it is not the one shown.
fn ppm_strip(ui: &mut egui::Ui, rect: egui::Rect, t: &Tokens, c: &CompareState) -> Option<CompareView> {
    let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(8.0, 4.0))));
    let mut ask = None;
    let building = c.overlay_job.is_some();
    ui.horizontal_wrapped(|ui| {
        // A host that fixed the view (`lockView`) shows the buttons, but they don't answer.
        ui.add_enabled_ui(!c.lock_view, |ui| {
            let shown = c.view();
            for (view, label) in [
                (CompareView::Side, tl!("Side by side").to_string()),
                (CompareView::Overlay, tl_ctx!(ppm::CTX, "Overlay").to_string()),
                (CompareView::OverlaySide, c.names.previous.as_ref().map_or_else(|| tl_ctx!(ppm::CTX, "Previous + overlay").to_string(), |n| crate::i18n::fmt(tl_ctx!(ppm::CTX, "{name} + overlay"), &[("name", n)]))),
            ] {
                if widgets::pill_button(ui, &label, shown == view).clicked() && shown != view {
                    ask = Some(view);
                }
            }
        });
        // While building, the host's status line says so (like the viewer's header); the strip stays quiet.
        if c.overlay_shown && !building {
            for (colour, label) in [
                (ppm::rgb(legend_colours(&c.overlay_style)[0]), lines_only(&c.names, true)),
                (ppm::rgb(legend_colours(&c.overlay_style)[1]), lines_only(&c.names, false)),
                (ppm::rgb(legend_colours(&c.overlay_style)[2]), tl_ctx!(ppm::CTX, "Same place").to_string()),
            ] {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().rect_filled(dot, egui::CornerRadius::same(2), colour);
                ui.label(egui::RichText::new(label).small());
            }
        } else if !c.overlay_shown && c.marks_custom {
            // The words are shaded in the host's colours, so the note names them with dots, not colour names.
            for (colour, label) in [
                (c.mark_colors[1], words_only(&c.names, true)),
                (c.mark_colors[0], words_only(&c.names, false)),
            ] {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().rect_filled(dot, egui::CornerRadius::same(2), colour);
                ui.label(egui::RichText::new(label).small());
            }
        } else if !c.overlay_shown {
            let note = if c.names.is_empty() {
                tl_ctx!(ppm::CTX, "Red: words only in the current file or changed / Green: words only in the chosen file or changed").to_string()
            } else {
                crate::i18n::fmt(
                    tl_ctx!(ppm::CTX, "Red: words only in {a} or changed / Green: words only in {b} or changed"),
                    &[("a", &c.names.current_or_default()), ("b", &c.names.previous_or_default())],
                )
            };
            ui.label(egui::RichText::new(note).small().color(t.text_muted));
        }
    });
    let note = if c.overlay_shown {
        c.overlay.as_ref().and_then(|(_, n)| n.clone())
    } else {
        Some(tl_ctx!(ppm::CTX, "Only words are shown. For pictures and drawings, use Overlay.").to_string())
    };
    if let Some(note) = note {
        ui.label(egui::RichText::new(note).small().color(t.text_muted));
    }
    ask
}

/// The two halves of a side-by-side comparison, as view indexes (old, new), when the active
/// document is the new file of a comparison shown side by side and the old file is open too.
pub(crate) fn side_by_side(app: &PdfCraftApp, index: usize) -> Option<(usize, usize)> {
    let c = app.compare.as_ref().filter(|c| c.side_by_side)?;
    let new = app.views.get(index).filter(|v| v.id == c.new)?;
    let old = app.views.iter().position(|v| v.id == c.old)?;
    (old != index).then_some((old, app.views.iter().position(|v| v.id == new.id)?))
}

/// PPM: run `f` with the overlay's view standing in the app's views (last, so no tab index
/// moves) and put it back after. The overlay is no tab: this is the only time its view is in the
/// list, and `f` gets its index. None when there is no overlay view to show.
pub(crate) fn with_overlay_view<R>(app: &mut PdfCraftApp, f: impl FnOnce(&mut PdfCraftApp, usize) -> R) -> Option<R> {
    let view = app.compare.as_mut()?.overlay_view.take()?;
    let id = view.id;
    app.views.push(view);
    let out = f(app, app.views.len() - 1);
    // `f` leaves the views as it found them; look for the view anyway rather than trust that.
    if let Some(at) = app.views.iter().rposition(|v| v.id == id) {
        let view = app.views.remove(at);
        if let Some(c) = app.compare.as_mut() {
            c.overlay_view = Some(view);
        }
    } else if let Some(c) = app.compare.as_mut() {
        // `f` closed the overlay's document: the pane says it couldn't be built, not stay empty.
        c.overlay = None;
        c.overlay_failed = true;
    }
    Some(out)
}

/// PPM: whether the overlay is what the active tab (the compared file's) shows now.
pub(crate) fn overlay_shown_over(app: &PdfCraftApp, index: usize) -> bool {
    app.compare.as_ref().is_some_and(|c| c.ppm && c.overlay_shown && app.views.get(index).is_some_and(|v| v.id == c.new))
}

impl PdfCraftApp {
    /// Drop the comparison. The PPM overlay document (kept only for its pane) is closed with it.
    pub(crate) fn end_compare(&mut self) {
        if let Some(c) = self.compare.take()
            && let Some((id, _)) = c.overlay
        {
            drop(c.overlay_view);
            self.forget_recovery(id);
            self.session.close(id);
        }
    }
}

/// Side by side: the old file on the left, the new on the right, each a page view of its own
/// with its shading, scrolled and zoomed together. The half the pointer is over leads; the
/// other follows its zoom, layout and scroll offset.
pub(crate) fn compare_area(app: &mut PdfCraftApp, old: usize, new: usize, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let mut rect = ui.available_rect_before_wrap();
    let ppm_on = app.compare.as_ref().is_some_and(|c| c.ppm);
    if ppm_on {
        // A page pair of the overlay a frame; the status below shows how far it got.
        app.step_overlay();
        // Before the comparison runs (`compare-preview`) there is nothing to switch or to read: the halves only.
        let previewing = app.compare.as_ref().is_some_and(|c| c.preview);
        if !previewing {
            let strip = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), ppm::STRIP_H));
            ui.painter().rect_filled(strip, 0.0, t.panel);
            if let Some(ask) =
                app.compare.as_ref().and_then(|c| ppm_strip(ui, strip, &t, c)).filter(|_| !app.compare.as_ref().is_some_and(|c| c.lock_view))
            {
                app.set_view(ask);
            }
            rect.min.y += ppm::STRIP_H;
        }
        if let Some(c) = app.compare.as_ref().filter(|c| c.overlay_shown && c.overlay_beside) {
            // The old file on the left, the overlay on the right; the old file's word shading is off here.
            let failed = c.overlay_failed;
            let old_marks = std::mem::take(&mut app.views[old].compare_marks);
            let drawn = with_overlay_view(app, |app, oi| draw_halves(app, ui, rect, (old, oi), false, Some(tl_ctx!(ppm::CTX, "Overlay").to_string())));
            if drawn.is_none() {
                // Still building (or failed): the old file alone, and the right half says so.
                let left = egui::Rect::from_min_size(rect.min, egui::vec2(((rect.width() - 1.0) / 2.0).max(1.0), rect.height()));
                let name = app.session.get(app.views[old].id).map(|d| d.name.clone()).unwrap_or_default();
                draw_half(app, ui, &t, left, old, &name, 0);
                if failed {
                    let right = egui::Rect::from_min_max(egui::pos2(left.max.x + 1.0, rect.min.y), rect.max);
                    ui.put(right, egui::Label::new(egui::RichText::new(tl_ctx!(ppm::CTX, "Couldn't build the overlay.")).color(t.text_muted)));
                }
            }
            app.views[old].compare_marks = old_marks;
            return;
        }
        if let Some(c) = app.compare.as_ref().filter(|c| c.overlay_shown) {
            let body = rect;
            let failed = c.overlay_failed;
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body).id_salt("compare-overlay"));
            child.set_clip_rect(body);
            if with_overlay_view(app, |app, index| crate::canvas::document_area(app, index, &mut child)).is_none() && failed {
                ui.put(body, egui::Label::new(egui::RichText::new(tl_ctx!(ppm::CTX, "Couldn't build the overlay.")).color(t.text_muted)));
            }
            return;
        }
    }
    draw_halves(app, ui, rect, (old, new), ppm_on, None);
}

/// One half of a side-by-side view: its caption (a name) over the document in `r`.
fn draw_half(app: &mut PdfCraftApp, ui: &mut egui::Ui, t: &Tokens, r: egui::Rect, index: usize, label: &str, k: usize) {
    let caption_h = 22.0;
    let caption = egui::Rect::from_min_size(r.min, egui::vec2(r.width(), caption_h));
    ui.painter().rect_filled(caption, 0.0, t.panel);
    ui.painter().text(egui::pos2(caption.min.x + 10.0, caption.center().y), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(12.0), t.text_muted);
    let body = egui::Rect::from_min_max(egui::pos2(r.min.x, r.min.y + caption_h), r.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body).id_salt(("compare-half", k)));
    child.set_clip_rect(body);
    crate::canvas::document_area(app, index, &mut child);
}

/// Two views in the two halves of `rect`, scrolled and zoomed together: `(old, new)` are the views in the old
/// and the new role (the leader, 0 old and 1 new, follows whichever half the pointer is over), `new_left` puts
/// the new role on the left. `new_label` names the new role's half when it is not a file (the overlay).
fn draw_halves(app: &mut PdfCraftApp, ui: &mut egui::Ui, rect: egui::Rect, (old, new): (usize, usize), new_left: bool, new_label: Option<String>) {
    let t = Tokens::get(ui.ctx());
    let gap = 1.0;
    let half_w = ((rect.width() - gap) / 2.0).max(1.0);
    let left = egui::Rect::from_min_size(rect.min, egui::vec2(half_w, rect.height()));
    let right = egui::Rect::from_min_size(egui::pos2(rect.min.x + half_w + gap, rect.min.y), egui::vec2(half_w, rect.height()));
    // The PPM build puts the current (new) file on the left like the viewer's base; the leader
    // (0 old, 1 new) follows whichever file a half shows.
    let (left_is, right_is) = if new_left { (1, 0) } else { (0, 1) };
    // Who leads: the half under the pointer (remembered), the new file at first.
    if let Some(p) = ui.input(|i| i.pointer.hover_pos())
        && let Some(c) = app.compare.as_mut()
    {
        if left.contains(p) {
            c.leader = left_is;
        } else if right.contains(p) {
            c.leader = right_is;
        }
    }
    let leader = app.compare.as_ref().map_or(1, |c| c.leader);
    let (lead, follow) = if leader == 0 { (old, new) } else { (new, old) };
    // The follower takes the leader's zoom and layout before either is drawn.
    let (zoom, fit, layout, continuous, rotation) = {
        let v = &app.views[lead];
        (v.zoom, v.fit, v.layout, v.continuous, v.rotation)
    };
    {
        let v = &mut app.views[follow];
        if v.zoom != zoom || v.fit != fit || v.layout != layout || v.continuous != continuous || v.rotation != rotation {
            v.zoom = zoom;
            v.fit = fit;
            v.layout = layout;
            v.continuous = continuous;
            v.rotation = rotation;
        }
    }
    let shown = if new_left { [new, old] } else { [old, new] };
    let names = shown.map(|i| app.session.get(app.views[i].id).map(|d| d.name.clone()).unwrap_or_default());
    ui.painter().vline(rect.min.x + half_w, rect.y_range(), egui::Stroke::new(gap, t.border));
    let halves = if new_left { [(left, new), (right, old)] } else { [(left, old), (right, new)] };
    for (k, (r, index)) in halves.into_iter().enumerate() {
        // The viewer's halves are headed by the file name alone.
        let label = match (&new_label, index == new) {
            (Some(l), true) => l.clone(),
            _ if new_left || new_label.is_some() => names[k].clone(),
            _ => crate::i18n::fmt(if k == 0 { tl!("Old file: {name}") } else { tl!("New file: {name}") }, &[("name", &names[k])]),
        };
        draw_half(app, ui, &t, r, index, &label, k);
    }
    // After both drew: the leader's scroll, when it moved, is the follower's next offset.
    let offsets = [app.views[old].last_scroll, app.views[new].last_scroll];
    if let Some(c) = app.compare.as_mut() {
        let lead_k = if leader == 0 { 0 } else { 1 };
        if (offsets[lead_k] - c.prev_scroll[lead_k]).length() > 0.5 && (offsets[1 - lead_k] - offsets[lead_k]).length() > 0.5 {
            app.views[follow].sync_scroll = Some(offsets[lead_k]);
            ui.ctx().request_repaint();
        }
        c.prev_scroll = offsets;
    }
}


#[cfg(test)]
mod tests {
    use super::{legend_colours, visible_rows};

    #[test]
    fn the_overlay_legend_follows_the_overlay_colours() {
        use pdfcraft_engine::compare::{OverlayStyle, OVERLAY_CHOSEN, OVERLAY_CURRENT, OVERLAY_SAME};
        assert_eq!(legend_colours(&OverlayStyle::default()), [OVERLAY_CURRENT, OVERLAY_CHOSEN, OVERLAY_SAME]);
        let style = OverlayStyle { current: [0, 170, 0], chosen: [255, 0, 255], strength_current: 0.8, strength_chosen: 0.8 };
        assert_eq!(legend_colours(&style), [[0, 170, 0], [255, 0, 255], OVERLAY_SAME]);
    }

    #[test]
    fn only_the_rows_in_view_are_laid_out() {
        // 2000 rows of 10 starting at y = 100; a clip of 300..360 shows rows 20..26.
        assert_eq!(visible_rows(100.0, 300.0, 360.0, 10.0, 2000), 20..26);
        // The list scrolled so its top is above the clip.
        assert_eq!(visible_rows(-500.0, 0.0, 40.0, 10.0, 2000), 50..54);
        // Before the first row, and past the last.
        assert_eq!(visible_rows(100.0, 0.0, 120.0, 10.0, 2000), 0..2);
        assert_eq!(visible_rows(0.0, 100_000.0, 100_100.0, 10.0, 5), 5..5);
        assert_eq!(visible_rows(0.0, 0.0, 100.0, 10.0, 0), 0..0);
    }
}
