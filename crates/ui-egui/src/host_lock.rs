//! The host lock ("메뉴 막기"): a build made for an embedding page (cargo feature `host-lock`,
//! turned on by `ppm/build-web.sh`) opens only what that page sends, and does nothing that
//! reaches the user's own files, printers or the web.
//!
//! The lock is a compile-time value ([`HOST_LOCK`]); nothing at run time lifts it. It is held
//! where the work happens (opening, picking, dropping, downloading, opening links), so a
//! shortcut, a right-click or a drag is stopped the same as a menu item. The command list below
//! decides what the menus show and is checked for drift by a test: a command that is in none of
//! the lists fails it, and is denied until someone classifies it.

use pdfcraft_engine::commands::COMMANDS;
use pdfcraft_engine::{EditClass, EditClasses, EditScope, Ownership};

/// Whether this build is made for an embedding page.
pub const HOST_LOCK: bool = cfg!(feature = "host-lock");

/// `pdfcraft:open` `mode`: `"builder"` lets everything not locked for every host through, and
/// anything else, including no mode or a value of another type, is the narrowest, `"viewer"`.
pub fn parse_mode(mode: Option<&str>) -> EditScope {
    match mode {
        Some("builder") => EditScope::All,
        _ => EditScope::Viewer,
    }
}

/// The edit scope to open a document with, given the `mode` the host sent. A build without the
/// lock has no modes: it edits as it always did.
pub fn open_scope(mode: Option<&str>) -> EditScope {
    if HOST_LOCK { parse_mode(mode) } else { EditScope::All }
}

/// `policy` of a `pdfcraft:open` or `pdfcraft:compare` message, as JSON: `{ overrides: { id: bool } }`.
/// Anything in the wrong shape (no policy, a policy or `overrides` that is not an object) is no
/// overrides, and so is a value that is not a boolean; the rest of the map still counts.
pub fn policy_overrides(policy: &serde_json::Value) -> Vec<(String, bool)> {
    let Some(overrides) = policy.get("overrides").and_then(|o| o.as_object()) else { return Vec::new() };
    overrides.iter().filter_map(|(id, on)| Some((id.clone(), on.as_bool()?))).collect()
}

/// Commands a viewer may run (and a builder too): comments, filling in, looking around.
pub(crate) const VIEWER: &[&str] = &[
    // Settings that only change how the app looks and behaves; the ones that would bring back
    // what is locked are left out of the dialog.
    "app.preferences",
    "ui.dock.reset",
    "file.save",
    "file.word_count",
    "edit.undo",
    "edit.redo",
    "edit.find",
    "edit.advanced_search",
    "edit.copy",
    "view.palette",
    "view.layout.single",
    "view.layout.two_up",
    "view.continuous",
    "view.layout.cover",
    "view.fit_width_scrolling",
    "view.fit_one_page",
    "view.fit_visible",
    "view.marquee_zoom",
    "view.full_screen",
    "view.read_mode",
    "view.next_tab",
    "view.previous_tab",
    "view.previous_view",
    "view.next_view",
    "view.focus_page_input",
    "view.theme",
    "view.theme.system",
    "view.theme.light",
    "view.theme.dark",
    "comment.list",
    "comment.note",
    "comment.freetext",
    "comment.highlight",
    "comment.underline",
    "comment.strikeout",
    "comment.squiggly",
    "comment.ink",
    "comment.line",
    "comment.arrow",
    "comment.square",
    "comment.circle",
    "comment.polygon",
    "comment.polyline",
    "comment.cloud",
    "comment.callout",
    "comment.caret",
    "comment.replace",
    "comment.eraser",
    "comment.hide_all",
    "form.fields",
    "form.clear",
    "sign.panel",
    "sign.validate",
    "help.shortcuts",
    "help.about",
];

/// Commands only a builder may run that a viewer's policy can turn on: editing the document
/// itself, in the kinds the engine can open for a viewer (see [`EDIT_CMDS`]), plus Fill & Sign and
/// what only reads or marks. Anything else is [`BUILDER_ONLY`].
pub(crate) const BUILDER: &[&str] = &[
    // Putting a file into the document is an edit, not a way out of it.
    "comment.attach",
    "page.combine",
    "page.insert",
    "page.replace",
    "edit.image",
    "form.add.image",
    "doc.compare",
    "sign.fill.text",
    "sign.fill.check",
    "sign.fill.cross",
    "sign.fill.dot",
    "sign.fill.line",
    "sign.fill.date",
    "sign.fill.signature",
    "sign.fill.initials",
    "measure.distance",
    "measure.perimeter",
    "measure.area",
    "measure.scale",
    "measure.info",
    "measure.snap",
    "page.copy",
    "page.cut",
    "page.paste",
    "file.properties",
    "edit.column_select",
    "edit.snapshot",
    "comment.stamp",
    "comment.summarize",
    "edit.text",
    "edit.edit_text",
    "edit.link",
    "edit.links_from_urls",
    "edit.remove_links",
    "redact.mark",
    "redact.pages",
    "redact.search",
    "redact.properties",
    "sign.fill.signature.change",
    "sign.fill.signature.remove",
    "sign.fill.initials.remove",
    "sign.fill.initials.change",
    "a11y.check",
    "a11y.find_untagged",
    "a11y.report",
    "a11y.reading_options",
    "a11y.tags",
    "page.organize",
    "page.rotate",
    "page.rotate_ccw",
    "page.delete",
    "page.insert_blank",
    "page.rotate_dialog",
    "page.duplicate",
    "page.reverse",
    "page.crop",
    "page.boxes",
    "page.extract",
    "page.number",
];

/// Commands only a builder may run, and no policy gives a viewer: the engine refuses their edits
/// to a viewer (form building, redaction, protection, watermarks, bookmarks, digital signing,
/// OCR, importing data), so turning one on would show a command that only ever fails.
pub(crate) const BUILDER_ONLY: &[&str] = &[
    "comment.import",
    "form.import_data",
    "form.merge_data",
    "comment.flatten",
    "form.flatten",
    "form.detect",
    "form.prepare",
    "form.tab_order.row",
    "form.tab_order.column",
    "form.tab_order.structure",
    "form.field.properties",
    "form.add.text",
    "form.add.checkbox",
    "form.add.radio",
    "form.add.combo",
    "form.add.list",
    "form.add.button",
    "form.add.date",
    "form.add.signature",
    "standards.pdfa",
    "edit.bates",
    "redact.apply",
    "redact.clear",
    "protect.remove_hidden",
    "redact.sanitize",
    "optimize.reduce",
    "optimize.advanced",
    "ocr.recognize",
    "a11y.autotag",
    "a11y.alt_text",
    "a11y.autotag_fields",
    "edit.header_footer",
    "edit.header_footer.update",
    "edit.header_footer.remove",
    "edit.watermark",
    "edit.watermark.update",
    "edit.watermark.remove",
    "edit.background",
    "edit.background.update",
    "edit.background.remove",
    "bookmark.add",
    "bookmark.from_structure",
];

/// Commands no policy can turn on: they open the user's files, start a new document, close or
/// revert the one the host gave (the parent's file could be overwritten with an empty or old
/// one), keep a document in the browser, run scripts, or change its security or signatures.
// Read by the drift test only: a command in no list is locked anyway.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const LOCKED: &[&str] = &[
    "file.open",
    "file.open_recent",
    "file.clear_recent",
    "file.pin_folder",
    "file.close",
    "file.close_all",
    "file.revert",
    "create.blank",
    "create.file",
    "create.multiple",
    "create.multiple_separate",
    "create.images",
    "create.clipboard",
    "tools.js_console",
    "tools.document_js",
    // A document's security and digital signatures are not the builder's to change either:
    // PPM keeps the file as the author made it. Fill & Sign stays on.
    "protect.password",
    "protect.remove",
    "protect.unlock",
    "protect.properties",
    "sign.digital",
    "sign.certify",
    "sign.certify_invisible",
];

/// The commands behind a digital signature (a signing rectangle, a signature field, an
/// invisible signature). Signing is no edit, so the work itself checks them.
pub(crate) const SIGN_CMDS: &[&str] = &["sign.digital", "sign.certify", "sign.certify_invisible"];

/// Commands that are off in every host build until the host's policy turns them on: they save
/// or export to the user's disk, print, or reach the web.
// Read by the drift test only.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const OFF: &[&str] = &[
    "file.save_as",
    "measure.export",
    "print.dialog",
    "comment.export",
    "form.export_data",
    "actions.wizard",
    "actions.distribution",
    "actions.optimize_scans",
    "export.image",
    "export.text",
    "export.docx",
    "export.pptx",
    "export.html",
    "export.rtf",
    "export.all_images",
    "ocr.enhance_camera",
    "ocr.recognize_batch",
    "page.split",
    "help.discord",
    "help.app_page",
    "help.github",
    "help.website",
    "help.check_updates",
];

/// The commands that reach the user's disk (save, export, download, a folder to write to).
/// Turning any of them on lets the platform functions behind them work.
// Read by the native file pickers only.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) const EXPORT_CMDS: &[&str] = &[
    "file.save_as",
    "measure.export",
    "comment.export",
    "form.export_data",
    "actions.wizard",
    "actions.distribution",
    "actions.optimize_scans",
    "export.image",
    "export.text",
    "export.docx",
    "export.pptx",
    "export.html",
    "export.rtf",
    "export.all_images",
    "ocr.enhance_camera",
    "ocr.recognize_batch",
    "page.split",
];

/// The Help menu's commands that open a web page (a document's links are [`DOCUMENT_LINKS`]).
#[cfg(test)]
pub(crate) const LINK_CMDS: &[&str] = &["help.discord", "help.app_page", "help.github", "help.website", "help.check_updates"];

/// The viewer-off commands each kind of edit comes from. A viewer's policy turning one of them on
/// opens its kind in the engine (see [`EditClass`]).
pub(crate) const EDIT_CMDS: &[(EditClass, &[&str])] = &[
    (
        EditClass::Page,
        &[
            "page.copy",
            "page.cut",
            "page.paste",
            "page.organize",
            "page.rotate",
            "page.rotate_ccw",
            "page.delete",
            "page.insert_blank",
            "page.rotate_dialog",
            "page.duplicate",
            "page.reverse",
            "page.crop",
            "page.boxes",
            "page.extract",
            "page.number",
            "page.combine",
            "page.insert",
            "page.replace",
        ],
    ),
    (EditClass::Content, &["edit.text", "edit.edit_text", "edit.link", "edit.links_from_urls", "edit.remove_links", "edit.image", "form.add.image"]),
    (EditClass::Measure, &["measure.distance", "measure.perimeter", "measure.area", "measure.scale", "measure.info", "measure.snap"]),
];

/// The commands an interface edit comes from. A locked build applies a page or content edit only
/// when one of them is on, so a screen that offers more than its command (the page grid's delete,
/// move and duplicate, a drag, a key) cannot do what the administrator turned off. `None`: the
/// edit isn't one of these kinds, or has no single owner, and the engine's own limit decides.
pub(crate) fn edit_commands(edit: &pdfcraft_engine::Edit) -> Option<&'static [&'static str]> {
    use pdfcraft_engine::Edit as E;
    Some(match edit {
        E::RotatePages { .. } => &["page.rotate", "page.rotate_ccw", "page.rotate_dialog"],
        E::DeletePages { .. } => &["page.delete", "page.cut"],
        E::MovePages { .. } => &["page.organize"],
        E::ReversePages { .. } => &["page.reverse"],
        E::InsertBlankPage { .. } => &["page.insert_blank"],
        E::DuplicatePages { .. } => &["page.duplicate", "page.paste"],
        E::ReplacePages { .. } => &["page.replace"],
        E::SetPageBox { .. } => &["page.crop", "page.boxes"],
        E::InsertPagesFrom { .. } => &["page.insert", "page.paste", "page.combine"],
        E::NumberPages { .. } => &["page.number"],
        E::Protect(_) => &["protect.password"],
        E::RemoveProtection => &["protect.remove"],
        // Text, pictures and links each have their own commands: turning one on opens its own edits only.
        E::EditTextLine { .. } | E::EditTextBlock { .. } | E::AddText { .. } | E::RegisterTextFont { .. } => &["edit.text", "edit.edit_text"],
        E::EditPageImage { .. } => &["edit.image"],
        E::AddImage { .. } | E::ReplaceImage { .. } => &["edit.image", "form.add.image"],
        E::AddLink { .. } | E::SetLink { .. } | E::AddLinks { .. } => &["edit.link", "edit.links_from_urls"],
        E::DeleteLink { .. } | E::RemoveLinks { .. } => &["edit.link", "edit.remove_links"],
        // MoveObjects, UpdateContent and DeleteContent name an object, so the kind is found from
        // the document: see `PdfCraftApp::host_edit_allowed`.
        _ => return None,
    })
}

/// A setting the administrator can turn on that is no menu command: it has an id, a label and a
/// default, and the interface asks [`PdfCraftApp::host_allows`] about it where the thing happens.
pub(crate) struct PolicyOnly {
    pub id: &'static str,
    pub label: &'static str,
    pub label_ko: &'static str,
}

/// Off for viewers and builders alike until the policy turns them on.
pub(crate) const POLICY_ONLY: &[PolicyOnly] =
    &[PolicyOnly { id: DOCUMENT_LINKS, label: "Open document links", label_ko: "문서 안 링크 열기" }];

/// Links inside a document (a link annotation, a button's address, a script's `app.launchURL`).
/// Not tied to the Help menu's links.
pub(crate) const DOCUMENT_LINKS: &str = "link.document";

/// How a command stands by default in a host build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Grade {
    /// No policy turns it on.
    Locked,
    /// Off until the policy turns it on.
    Off,
    /// On until the policy turns it off.
    On,
}

impl Grade {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Grade::Locked => "locked",
            Grade::Off => "off",
            Grade::On => "on",
        }
    }
}

/// The grade of `id` for documents of `scope`. A command in no list is locked.
pub(crate) fn grade(id: &str, scope: EditScope) -> Grade {
    if VIEWER.contains(&id) {
        Grade::On
    } else if BUILDER.contains(&id) {
        if scope == EditScope::All { Grade::On } else { Grade::Off }
    } else if BUILDER_ONLY.contains(&id) {
        if scope == EditScope::All { Grade::On } else { Grade::Locked }
    } else if OFF.contains(&id) || POLICY_ONLY.iter().any(|p| p.id == id) {
        Grade::Off
    } else {
        Grade::Locked
    }
}

/// Whether the host's policy `overrides` leave `id` on, for a document of `scope`.
pub(crate) fn enabled(id: &str, scope: EditScope, overrides: &[(String, bool)]) -> bool {
    match grade(id, scope) {
        Grade::Locked => false,
        g => overrides.binary_search_by(|(k, _)| k.as_str().cmp(id)).map_or(g == Grade::On, |i| overrides[i].1),
    }
}

/// The commands whose files may leave the page (a download, a save to disk); a download is told
/// which of them it is for ([`downloads_allow`]).
pub(crate) const DOWNLOAD_CMDS: &[&str] = &[
    "file.save_as",
    "print.dialog",
    "page.split",
    "measure.export",
    "comment.export",
    "form.export_data",
    "actions.wizard",
    "actions.distribution",
    "actions.optimize_scans",
    "export.image",
    "export.text",
    "export.docx",
    "export.pptx",
    "export.html",
    "export.rtf",
    "export.all_images",
    "ocr.enhance_camera",
    "ocr.recognize_batch",
];

/// Which of [`DOWNLOAD_CMDS`] the document being shown has on. Platform code that cannot reach
/// the app reads it.
static DOWNLOADS_ALLOWED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn set_downloads_allowed(allowed: Vec<&'static str>) {
    if let Ok(mut now) = DOWNLOADS_ALLOWED.lock() {
        *now = allowed;
    }
}

/// Whether a file for one of the commands `cmds` may leave: any of them is on.
// Read by the web build's download only.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn downloads_allow(cmds: &[&str]) -> bool {
    DOWNLOADS_ALLOWED.lock().is_ok_and(|now| any_listed(&now, cmds))
}

fn any_listed(on: &[&str], cmds: &[&str]) -> bool {
    cmds.iter().any(|c| on.contains(c))
}

/// Whether work of `class` may be done on a document with `own`: a builder's always, a viewer's
/// when its policy opened that kind of edit.
pub(crate) fn scope_allows(own: &Ownership, class: EditClass) -> bool {
    own.scope == EditScope::All || own.allow.has(class)
}

/// `own` with the host's `policy.overrides` applied: the commands it names (unknown ids and
/// anything not true or false are dropped by the caller or here), and for a viewer the edit
/// kinds its turned-on commands open. A missing or malformed policy is no overrides.
pub fn with_policy(mut own: Ownership, overrides: Vec<(String, bool)>) -> Ownership {
    let mut known: Vec<(String, bool)> = Vec::new();
    for (id, on) in overrides {
        if COMMANDS.iter().any(|c| c.id == id) || POLICY_ONLY.iter().any(|p| p.id == id) {
            known.retain(|(k, _)| *k != id);
            known.push((id, on));
        }
    }
    known.sort();
    own.allow = EditClasses::default();
    if own.scope == EditScope::Viewer {
        for (class, ids) in EDIT_CMDS {
            if ids.iter().any(|id| grade(id, EditScope::Viewer) == Grade::Off && enabled(id, EditScope::Viewer, &known)) {
                own.allow = own.allow.with(*class);
            }
        }
    }
    own.overrides = known;
    own
}

/// The list of commands the admin screen shows (`ppm/commands.json`): id, the label the app
/// shows (English: the app has no Korean catalog), the top-level menu if any, and the grade for
/// a viewer's and a builder's documents. Built from the engine's command table, and a test holds
/// the checked-in file to it.
pub fn commands_manifest() -> String {
    // The Korean catalogue is the one source of every label the admin screen shows in Korean.
    let ko = crate::i18n::Lang::from_code("ko").unwrap_or(crate::i18n::Lang::EN);
    let commands: Vec<serde_json::Value> = COMMANDS
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "label": c.label,
                "labelKo": crate::i18n::tr(ko, c.label),
                "location": c.menu.map(|m| vec![m]).unwrap_or_default(),
                "grades": {
                    "viewer": grade(c.id, EditScope::Viewer).name(),
                    "builder": grade(c.id, EditScope::All).name(),
                },
            })
        })
        .chain(POLICY_ONLY.iter().map(|p| {
            serde_json::json!({
                "id": p.id,
                "label": p.label,
                "labelKo": p.label_ko,
                "location": Vec::<&str>::new(),
                "grades": { "viewer": grade(p.id, EditScope::Viewer).name(), "builder": grade(p.id, EditScope::All).name() },
            })
        }))
        .collect();
    let mut out = serde_json::to_string_pretty(&serde_json::json!({ "app": "pdf", "schema": 1, "commands": commands })).unwrap_or_default();
    out.push('\n');
    out
}

/// The rules of a document the host opened without naming any: a viewer's, the narrowest.
pub(crate) fn viewer_ownership() -> pdfcraft_engine::Ownership {
    pdfcraft_engine::Ownership { scope: EditScope::Viewer, ..Default::default() }
}

/// The page every host build shows, and nothing else, when no embedding page is connected.
pub(crate) fn open_in_ppm_screen(ui: &mut egui::Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new("PPM에서 열어 주세요").size(22.0));
        });
    });
}

use crate::PdfCraftApp;

/// What a refused action says. Not a `tl!` literal, so the 23 other catalogs need no entry for a
/// text only a host build shows; a host build starts in Korean, so the Korean text is kept here.
pub(crate) const HOST_OFF: &str = "The administrator has turned this off.";
const HOST_OFF_KO: &str = "관리자가 이 기능을 꺼 두었습니다.";

/// [`HOST_OFF`] in the current language: Korean, or English in any other.
pub(crate) fn host_off_text() -> &'static str {
    if crate::i18n::current().code() == "ko" { HOST_OFF_KO } else { HOST_OFF }
}

impl PdfCraftApp {
    pub(crate) fn notify_host_off(&mut self) {
        self.notify(host_off_text().to_string());
    }

    /// This build is made for an embedding page (see [`HOST_LOCK`]).
    pub fn host_locked(&self) -> bool {
        self.host_lock
    }

    /// The scope of the active document; a viewer's when there is none.
    fn host_scope(&self) -> EditScope {
        self.active_ids().and_then(|(_, id)| self.session.get(id)).map_or(EditScope::Viewer, |d| d.ownership().scope)
    }

    /// Whether every open document's policy has `id` on (the active document's, with none open).
    /// For what can't tell which document asked: a download, a link from a script.
    pub(crate) fn host_allows_in_all_documents(&self, id: &str) -> bool {
        if !self.host_lock {
            return true;
        }
        let mut docs = self.views.iter().filter_map(|v| self.session.get(v.id)).map(|d| d.ownership()).peekable();
        if docs.peek().is_none() {
            return enabled(id, EditScope::Viewer, &[]);
        }
        docs.all(|o| enabled(id, o.scope, &o.overrides))
    }

    /// The downloads every open document's policy allows. A file may be asked for by a task that
    /// finishes after the user went to another tab, and a download can't tell which document it
    /// came from, so one document's policy never opens what another's keeps shut.
    pub(crate) fn host_downloads_allowed(&self) -> Vec<&'static str> {
        DOWNLOAD_CMDS.iter().copied().filter(|id| self.host_allows_in_all_documents(id)).collect()
    }

    /// Whether the commands behind `edit` (and each edit inside a `Batch`) are on. An edit that
    /// names an object (move, change, delete) is judged by the object's kind, text or picture; one
    /// whose kind can't be found is refused.
    pub(crate) fn host_edit_allowed(&self, edit: &pdfcraft_engine::Edit) -> bool {
        use pdfcraft_engine::{Edit, ObjectKind};
        const TEXT: &[&str] = &["edit.text", "edit.edit_text"];
        const PICTURE: &[&str] = &["edit.image"];
        let added_kind = |page: usize, index: usize| {
            let doc = self.active_ids().and_then(|(_, id)| self.session.get(id))?;
            match &doc.added().iter().filter(|a| a.page == page).nth(index)?.content {
                pdfcraft_engine::AddedContent::Text(_) => Some(TEXT),
                pdfcraft_engine::AddedContent::Image(_) => Some(PICTURE),
            }
        };
        let judged = |cmds: Option<&[&str]>| cmds.is_some_and(|c| self.host_any(c));
        match edit {
            Edit::Batch { edits, .. } => edits.iter().all(|e| self.host_edit_allowed(e)),
            Edit::MoveObjects { page, objects, .. } => objects.iter().all(|o| {
                judged(match o.kind {
                    ObjectKind::Text => Some(TEXT),
                    ObjectKind::Image => Some(PICTURE),
                    ObjectKind::Added => added_kind(*page, o.index),
                })
            }),
            Edit::UpdateContent { content, .. } => judged(Some(match content {
                pdfcraft_engine::AddedContent::Text(_) => TEXT,
                pdfcraft_engine::AddedContent::Image(_) => PICTURE,
            })),
            Edit::DeleteContent { page, index } => judged(added_kind(*page, *index)),
            e => crate::host_lock::edit_commands(e).is_none_or(|cmds| self.host_any(cmds)),
        }
    }

    /// The active document's scope and policy; a viewer's with no policy when there is none.
    fn host_rules(&self) -> (EditScope, &[(String, bool)]) {
        match self.active_ids().and_then(|(_, id)| self.session.get(id)) {
            Some(d) => (d.ownership().scope, &d.ownership().overrides),
            None => (EditScope::Viewer, &[]),
        }
    }

    /// A locked build showing a viewer's document (or none): the interface offers comments and
    /// looking around, and hides the tools that edit the document itself. A policy that opens
    /// pages, content or measuring to the viewer brings the tool panels back.
    pub(crate) fn host_viewer(&self) -> bool {
        self.host_lock && self.host_scope() == EditScope::Viewer && !self.host_tools_open()
    }

    /// Whether any command a viewer starts without (those of [`BUILDER`] and [`BUILDER_ONLY`]) is
    /// on: only then do the Tools panel and the workspace tabs come back. Always true without the lock.
    pub(crate) fn host_tools_open(&self) -> bool {
        !self.host_lock || BUILDER.iter().chain(BUILDER_ONLY).any(|id| self.host_allows(id))
    }

    /// Whether a tool group has a command that may run, so it is worth a row in the Tools panel.
    pub(crate) fn host_group_shown(&self, group: &pdfcraft_engine::catalog::ToolGroup) -> bool {
        group.sections.iter().flat_map(|s| s.items).any(|i| self.host_allows(i.command))
    }

    /// Whether the workspace tab of `mode` is offered: the tabs that open the Tools panel need
    /// some tool on, Edit, Convert and E-Sign need a command of their own tool group.
    pub(crate) fn host_mode_shown(&self, mode: crate::Mode) -> bool {
        use crate::Mode;
        let group = |id: &str| pdfcraft_engine::catalog::TOOL_GROUPS.iter().find(|g| g.id == id).is_some_and(|g| self.host_group_shown(g));
        if !self.host_lock {
            return true;
        }
        match mode {
            Mode::AllTools | Mode::Read => self.host_tools_open(),
            Mode::Edit => group("edit"),
            Mode::Convert => group("export"),
            Mode::Sign => group("fill_sign"),
        }
    }

    /// Whether the host lock and the host's policy let command `id` run now, and show where it
    /// has a button. Always true without the lock.
    pub(crate) fn host_allows(&self, id: &str) -> bool {
        // The page keeps the annotations itself (`annotSync`): the file isn't saved from here.
        if matches!(id, "file.save" | "file.save_as") && self.active_shares_annotations() {
            return false;
        }
        if !self.host_lock {
            return true;
        }
        let (scope, overrides) = self.host_rules();
        enabled(id, scope, overrides)
    }

    /// Whether any of `ids` is on: the platform functions behind a whole kind of command (writing
    /// files, opening links) work when one of its commands is.
    pub(crate) fn host_any(&self, ids: &[&str]) -> bool {
        !self.host_lock || ids.iter().any(|id| self.host_allows(id))
    }

    /// A locked build with no embedding page connected: it opens nothing and shows only
    /// [`open_in_ppm_screen`]. Fails closed, so a page opened on its own, without `?host=parent`
    /// or a same-origin parent, never gets a working app.
    pub(crate) fn host_missing(&self) -> bool {
        self.host_lock && self.host_save.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LinkOrigin, pickers::PickFor};
    use pdfcraft_engine::Ownership;
    use pdfcraft_engine::commands::COMMANDS;

    /// One blank page.
    fn pdf() -> Vec<u8> {
        build(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".into(),
            "<< /Type /Page /Parent 2 0 R >>".into(),
        ])
    }

    /// A one-page file with another PDF attached.
    fn pdf_with_attachment() -> Vec<u8> {
        let inner = String::from_utf8(pdf()).unwrap();
        build(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles << /Names [(in.pdf) 4 0 R] >> >> >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".into(),
            "<< /Type /Page /Parent 2 0 R >>".into(),
            "<< /Type /Filespec /F (in.pdf) /UF (in.pdf) /EF << /F 5 0 R /UF 5 0 R >> >>".into(),
            format!("<< /Type /EmbeddedFile /Subtype /application#2Fpdf /Length {} >>\nstream\n{inner}\nendstream", inner.len()),
        ])
    }

    fn build(objs: &[String]) -> Vec<u8> {
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

    fn locked() -> PdfCraftApp {
        let mut app = PdfCraftApp::new();
        app.host_lock = true;
        app
    }

    /// Every command is classified once: a new one is in no list and fails here until someone
    /// decides what a host build does with it (until then it is denied).
    #[test]
    fn every_command_is_classified_exactly_once() {
        let mut seen = std::collections::HashMap::new();
        for (list, ids) in [("VIEWER", VIEWER), ("BUILDER", BUILDER), ("BUILDER_ONLY", BUILDER_ONLY), ("OFF", OFF), ("LOCKED", LOCKED)] {
            for id in ids {
                assert!(COMMANDS.iter().any(|c| c.id == *id), "{id} in {list} is not a command (renamed or removed?)");
                if let Some(other) = seen.insert(*id, list) {
                    panic!("{id} is in both {other} and {list}");
                }
            }
        }
        let missing: Vec<&str> = COMMANDS.iter().map(|c| c.id).filter(|id| !seen.contains_key(id)).collect();
        assert!(
            missing.is_empty(),
            "commands with no host classification (add them to VIEWER, BUILDER, BUILDER_ONLY, OFF or LOCKED in host_lock.rs): {missing:?}"
        );
    }

    /// What the contract names as shut in every host build is not on, in either mode.
    #[test]
    fn what_reaches_the_users_files_or_the_web_is_off_or_locked() {
        for id in ["file.open", "file.open_recent", "create.blank", "create.file", "file.close", "file.revert", "tools.js_console"] {
            assert_eq!(grade(id, EditScope::All), Grade::Locked, "{id}");
        }
        for id in
            ["file.save_as", "print.dialog", "export.image", "export.docx", "comment.export", "help.discord", "help.github", "help.check_updates"]
        {
            assert_eq!((grade(id, EditScope::All), grade(id, EditScope::Viewer)), (Grade::Off, Grade::Off), "{id}");
        }
        assert_eq!(grade("help.about", EditScope::Viewer), Grade::On);
        // Saving to the host page and the look of the app are on for both; only saving elsewhere is off.
        for scope in [EditScope::Viewer, EditScope::All] {
            assert_eq!(
                (grade("file.save", scope), grade("app.preferences", scope), grade("file.save_as", scope)),
                (Grade::On, Grade::On, Grade::Off)
            );
        }
        assert_eq!((grade("page.rotate", EditScope::Viewer), grade("page.rotate", EditScope::All)), (Grade::Off, Grade::On));
        assert_eq!(grade("not.a.command", EditScope::All), Grade::Locked, "what is unknown is locked");
    }

    /// Every command a kind of edit comes from is a viewer-off command; the lists name real ones.
    #[test]
    fn the_edit_kind_lists_name_viewer_off_commands() {
        let mut seen = std::collections::HashSet::new();
        for (class, ids) in EDIT_CMDS {
            for id in *ids {
                assert!(seen.insert(*id), "{id} is in two kinds");
                assert!(COMMANDS.iter().any(|c| c.id == *id), "{id} ({class:?}) is not a command");
                assert_eq!(grade(id, EditScope::Viewer), Grade::Off, "{id}");
            }
        }
        for id in EXPORT_CMDS.iter().chain(LINK_CMDS) {
            assert_eq!(grade(id, EditScope::Viewer), Grade::Off, "{id}");
            assert!(OFF.contains(id), "{id}");
        }
    }

    /// A command a viewer's policy can turn on has to work once on: its edit kind is one the policy
    /// opens in the engine, or it needs no edit kind (Fill & Sign and marks are annotations, the
    /// rest only look). Whatever the engine refuses a viewer is `BUILDER_ONLY`, locked for it.
    #[test]
    fn a_viewer_off_edit_command_is_one_the_engine_can_open() {
        // Annotation-based or read-only: runs for a viewer without any edit kind opened.
        const NEEDS_NO_KIND: &[&str] = &[
            "comment.attach",
            "comment.stamp",
            "comment.summarize",
            "doc.compare",
            "file.properties",
            "edit.column_select",
            "edit.snapshot",
            "redact.mark",
            "redact.pages",
            "redact.search",
            "redact.properties",
            "a11y.check",
            "a11y.find_untagged",
            "a11y.report",
            "a11y.reading_options",
            "a11y.tags",
        ];
        for id in BUILDER {
            let kind = EDIT_CMDS.iter().any(|(_, ids)| ids.contains(id));
            let fill = id.starts_with("sign.fill.");
            assert!(
                kind || fill || NEEDS_NO_KIND.contains(id),
                "{id} is viewer-off but no edit kind of EDIT_CMDS opens it: lock it for a viewer in BUILDER_ONLY, or list it as needing none"
            );
            assert!(!BUILDER_ONLY.contains(id), "{id}");
        }
        for id in ["edit.image", "form.add.image"] {
            assert!(EDIT_CMDS.iter().any(|(c, ids)| *c == EditClass::Content && ids.contains(&id)), "{id}");
        }
        // The locked-for-a-viewer ones stay shut whatever the policy says, and are on for a builder.
        let own =
            with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, BUILDER_ONLY.iter().map(|id| (id.to_string(), true)).collect());
        assert!(own.allow.is_empty(), "no edit kind opens for a command the engine refuses");
        for id in BUILDER_ONLY {
            assert!(!enabled(id, EditScope::Viewer, &own.overrides) && grade(id, EditScope::All) == Grade::On, "{id}");
        }
    }

    /// `policy.overrides`: true turns an off command on, never a locked one; false turns an on one off.
    #[test]
    fn overrides_turn_commands_on_and_off_but_never_a_locked_one() {
        let viewer = |o: &[(&str, bool)]| {
            with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, o.iter().map(|(k, v)| (k.to_string(), *v)).collect())
        };
        let own = viewer(&[("print.dialog", true), ("file.open", true), ("comment.note", false), ("no.such", true), ("page.rotate", true)]);
        let ok = |id: &str| enabled(id, EditScope::Viewer, &own.overrides);
        assert!(ok("print.dialog") && ok("page.rotate") && !ok("comment.note") && ok("comment.ink"));
        assert!(!ok("file.open"), "locked stays locked");
        assert!(!ok("file.save_as") && !ok("sign.fill.text"), "what is not named keeps its default");
        assert!(own.overrides.iter().all(|(k, _)| k != "no.such"), "unknown ids are dropped");
        assert!(own.allow.has(EditClass::Page) && !own.allow.has(EditClass::Content), "only the kind of the turned-on command opens in the engine");
        assert!(viewer(&[]).allow.is_empty() && viewer(&[("page.rotate", false)]).allow.is_empty());
        // The last of two settings for one command wins.
        assert!(enabled("print.dialog", EditScope::Viewer, &viewer(&[("print.dialog", false), ("print.dialog", true)]).overrides));
        // A builder has no edit kinds to open.
        let b = with_policy(Ownership::default(), vec![("page.rotate".into(), false)]);
        assert!(b.allow.is_empty() && !enabled("page.rotate", EditScope::All, &b.overrides));
    }

    /// The shape checks of `policy`: wrong shapes change nothing, and one bad value spares the rest.
    #[test]
    fn a_policy_of_the_wrong_shape_or_type_is_ignored() {
        use serde_json::json;
        let get = |v: serde_json::Value| {
            let mut o = policy_overrides(&v);
            o.sort();
            o
        };
        let on = |id: &str, v: bool| (id.to_string(), v);
        assert_eq!(
            get(json!({ "overrides": { "print.dialog": true, "page.rotate": false } })),
            vec![on("page.rotate", false), on("print.dialog", true)]
        );
        assert_eq!(
            get(json!({ "overrides": { "print.dialog": "true", "a": 1, "b": null, "c": [true], "page.rotate": true } })),
            vec![on("page.rotate", true)]
        );
        for bad in [
            json!(null),
            json!("x"),
            json!(3),
            json!([]),
            json!({}),
            json!({ "overrides": null }),
            json!({ "overrides": [true] }),
            json!({ "overrides": "print.dialog" }),
            json!({ "override": { "print.dialog": true } }),
        ] {
            assert!(get(bad.clone()).is_empty(), "{bad}");
        }
    }

    /// A viewer gets what its policy names, and nothing else.
    #[test]
    fn a_policy_widens_and_narrows_a_document_by_command() {
        let mut app = locked();
        let viewer = |o: &[(&str, bool)]| {
            with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, o.iter().map(|(k, v)| (k.to_string(), *v)).collect())
        };
        app.session.set_open_ownership(viewer(&[("print.dialog", true), ("page.rotate", true), ("sign.fill.text", true), ("comment.ink", false)]));
        app.open_bytes("p.pdf", None, pdf()).unwrap();
        assert!(app.host_allows("print.dialog") && app.host_allows("page.rotate") && app.host_allows("sign.fill.text"));
        assert!(!app.host_allows("comment.ink") && app.host_allows("comment.note"));
        assert!(!app.host_allows("file.save_as") && !app.host_allows("measure.distance") && !app.host_allows("edit.text"));
        assert!(!app.host_allows("file.open") && !app.host_allows("redact.apply") && !app.host_allows("help.github"));
        assert!(!app.host_viewer(), "page tools come back with page.rotate");
        assert!(app.host_any(&["file.save_as", "print.dialog"]) && !app.host_any(EXPORT_CMDS) && !app.host_any(LINK_CMDS));
        // The engine follows the same policy.
        let (_, id) = app.active_ids().unwrap();
        assert!(app.session.apply(id, pdfcraft_engine::Edit::RotatePages { pages: vec![0], degrees: 90 }).is_ok());
        app.session.set_open_ownership(viewer(&[("sign.fill.text", true)]));
        app.open_bytes("q.pdf", None, pdf()).unwrap();
        assert!(app.host_allows("sign.fill.text") && !app.host_allows("page.rotate"));
    }

    /// One tool's file may leave only when a command of its own is on.
    #[test]
    fn a_download_needs_one_of_its_own_commands() {
        let on = ["page.split"];
        assert!(any_listed(&on, &["page.split"]));
        assert!(!any_listed(&on, &["file.save_as"]));
        assert!(!any_listed(&[], &["file.save_as", "print.dialog"]));
        assert!(DOWNLOAD_CMDS.iter().all(|id| grade(id, EditScope::Viewer) == Grade::Off), "a viewer has none on by default");
    }

    /// Closing, reverting and the tab's own menu all go through the command's policy.
    #[test]
    fn closing_and_reverting_follow_their_commands() {
        let mut app = locked();
        app.session.set_open_ownership(with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, Vec::new()));
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        app.open_bytes("b.pdf", None, pdf()).unwrap();
        let n = app.views.len();
        app.request_close_tab(0);
        app.close_all();
        app.revert_active();
        app.tab_action(crate::tab_context::Action::Close(app.views[0].id));
        assert_eq!(app.views.len(), n, "locked: no tab closes");
    }

    /// Turning on one page tool brings back that tool and no other row of the Tools panel.
    #[test]
    fn one_tool_on_shows_only_its_own_group() {
        let mut app = locked();
        app.session.set_open_ownership(with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, vec![("page.rotate".into(), true)]));
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        assert!(app.host_mode_shown(crate::Mode::AllTools));
        let shown = |id: &str| pdfcraft_engine::catalog::TOOL_GROUPS.iter().find(|g| g.id == id).is_some_and(|g| app.host_group_shown(g));
        assert!(shown("organize"));
        assert!(!shown("redact") && !shown("protect") && !shown("stamp"));
        assert!(!app.host_allows("form.add.text") && !app.host_allows("edit.watermark"), "form building and watermarks stay off");
        assert!(!app.host_mode_shown(crate::Mode::Convert) && !app.host_mode_shown(crate::Mode::Sign));
    }

    /// Every command id a download or file write is told about is a real command and one the
    /// download gate knows, so a rename can't leave a button blocked for good.
    #[test]
    fn download_call_sites_name_known_commands() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut ids = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") || path.ends_with("host_lock.rs") {
                continue;
            }
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                if !(line.contains("download(&[") || line.contains("write_files(") || line.contains("write_files_then(")) || line.contains("fn ") {
                    continue;
                }
                for part in line.split('"').skip(1).step_by(2).filter(|p| p.contains('.') && !p.contains(' ') && !p.contains('/') && !p.contains('{'))
                {
                    if part.ends_with(".csv") || part.ends_with(".pdf") || part.ends_with(".xfdf") {
                        continue;
                    }
                    assert!(DOWNLOAD_CMDS.contains(&part) && COMMANDS.iter().any(|c| c.id == part), "{part} in {}", path.display());
                    ids += 1;
                }
            }
        }
        assert!(ids > 15, "the scan found the call sites ({ids})");
        assert!(DOWNLOAD_CMDS.iter().all(|id| COMMANDS.iter().any(|c| c.id == *id)));
    }

    /// A document's links have an id of their own that no Help link turns on, and it can be turned on.
    #[test]
    fn document_links_follow_their_own_setting() {
        assert!(!COMMANDS.iter().any(|c| c.id == DOCUMENT_LINKS) && !LINK_CMDS.contains(&DOCUMENT_LINKS));
        for scope in [EditScope::Viewer, EditScope::All] {
            assert_eq!(grade(DOCUMENT_LINKS, scope), Grade::Off);
        }
        let mut app = locked();
        let own = |o: &[(&str, bool)]| {
            with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, o.iter().map(|(k, v)| (k.to_string(), *v)).collect())
        };
        app.session.set_open_ownership(own(&[("help.github", true), ("help.website", true)]));
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        app.request_document_url("https://example.com", crate::LinkOrigin::Link);
        assert!(app.pending_link.is_none(), "Help links on, document links still off");
        let mut app = locked();
        app.session.set_open_ownership(own(&[(DOCUMENT_LINKS, true)]));
        app.open_bytes("b.pdf", None, pdf()).unwrap();
        app.request_document_url("https://example.com", crate::LinkOrigin::Link);
        assert!(app.pending_link.is_some());
        // A second document that keeps them off closes them for a request that can't say whose it is.
        app.session.set_open_ownership(own(&[]));
        app.open_bytes("c.pdf", None, pdf()).unwrap();
        app.pending_link = None;
        app.request_document_url("https://example.com", crate::LinkOrigin::Script);
        assert!(app.pending_link.is_none());
        let json = commands_manifest();
        assert!(json.contains("\"link.document\"") && json.contains("문서 안 링크 열기"));
    }

    /// A help link opens only for its own command.
    #[test]
    fn a_link_opens_for_its_own_command_only() {
        let mut app = locked();
        app.session.set_open_ownership(with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, vec![("help.github".into(), true)]));
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        app.open_url("https://example.com/w", &["help.website"]);
        assert_eq!(app.last_opened_url, None);
        app.open_url("https://example.com/g", &["help.github"]);
        assert_eq!(app.last_opened_url.as_deref(), Some("https://example.com/g"));
    }

    /// Every command an edit is tied to exists, and every page or content command opens its kind.
    #[test]
    fn edit_commands_are_real_commands() {
        let known: std::collections::HashSet<_> = COMMANDS.iter().map(|c| c.id).collect();
        let all = EDIT_CMDS.iter().flat_map(|(_, ids)| ids.iter().copied()).collect::<std::collections::HashSet<_>>();
        let sample = [
            pdfcraft_engine::Edit::RotatePages { pages: vec![], degrees: 90 },
            pdfcraft_engine::Edit::DeletePages { pages: vec![] },
            pdfcraft_engine::Edit::MovePages { pages: vec![], to: 0 },
            pdfcraft_engine::Edit::ReversePages { pages: vec![] },
            pdfcraft_engine::Edit::DuplicatePages { pages: vec![], at: None },
        ];
        for e in sample {
            for id in edit_commands(&e).expect("mapped") {
                assert!(known.contains(id) && all.contains(id), "{id}");
            }
        }
    }

    /// Text, picture and link edits each need a command of their own kind.
    #[test]
    fn text_picture_and_link_edits_are_told_apart() {
        use pdfcraft_engine::{Edit, ImageEdit, LinkAction, LinkStyle};
        let text = Edit::EditTextLine { page: 0, line: 0, text: "x".into() };
        let picture = Edit::EditPageImage { page: 0, index: 0, change: ImageEdit::Rotate(1) };
        let add_link = Edit::AddLink { page: 0, rect: [0.0; 4], action: LinkAction::Page(0), style: LinkStyle::default() };
        let remove = Edit::RemoveLinks { pages: None };
        let with = |id: &str| {
            let mut app = locked();
            app.session.set_open_ownership(with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, vec![(id.into(), true)]));
            app.open_bytes("a.pdf", None, pdf()).unwrap();
            app
        };
        let app = with("edit.text");
        assert!(
            app.host_edit_allowed(&text) && !app.host_edit_allowed(&picture) && !app.host_edit_allowed(&add_link) && !app.host_edit_allowed(&remove)
        );
        let app = with("edit.image");
        assert!(app.host_edit_allowed(&picture) && !app.host_edit_allowed(&text) && !app.host_edit_allowed(&add_link));
        let app = with("edit.remove_links");
        assert!(app.host_edit_allowed(&remove) && !app.host_edit_allowed(&add_link) && !app.host_edit_allowed(&text));
        let app = with("edit.links_from_urls");
        assert!(app.host_edit_allowed(&add_link) && !app.host_edit_allowed(&remove));
        // A batch is only as allowed as each edit in it.
        let app = with("edit.text");
        assert!(!app.host_edit_allowed(&Edit::Batch { label: "b".into(), edits: vec![text.clone(), picture] }));
        assert!(app.host_edit_allowed(&Edit::Batch { label: "b".into(), edits: vec![text] }));
    }

    /// Moving, changing and deleting an object is judged by what the object is: text or picture.
    #[test]
    fn objects_are_judged_by_their_kind() {
        use pdfcraft_engine::{AddedContent, AddedText, Edit, ObjectKind, ObjectTarget};
        let mut app = locked();
        // The file is a builder's until its two added items are in; then its rules are the viewer's.
        app.session.set_open_ownership(Ownership::default());
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        let (_, id) = app.active_ids().unwrap();
        let text = AddedText { rect: [20.0, 250.0, 180.0, 280.0], text: "note".into(), size: 12.0, ..AddedText::default() };
        app.session.apply(id, Edit::AddText { page: 0, text: text.clone() }).unwrap();
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 20, 10);
            enc.set_color(png::ColorType::Rgb);
            enc.write_header().unwrap().write_image_data(&[0u8; 600]).unwrap();
        }
        app.session.apply(id, Edit::AddImage { page: 0, rect: None, name: "dot.png".into(), bytes: std::sync::Arc::new(png) }).unwrap();
        let (text_at, image_at) = (0, 1);
        let target = |kind, index| ObjectTarget { kind, index };
        let mv = |objects: Vec<ObjectTarget>| Edit::MoveObjects { page: 0, objects, offset: [1.0, 1.0] };
        let rules = |own: &str| with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, vec![(own.into(), true)]);

        app.session.set_ownership(rules("edit.text"));
        let all = |app: &PdfCraftApp, edits: &[&Edit]| edits.iter().all(|e| app.host_edit_allowed(e));
        let none = |app: &PdfCraftApp, edits: &[&Edit]| edits.iter().all(|e| !app.host_edit_allowed(e));
        let text_move = mv(vec![target(ObjectKind::Text, 0)]);
        let added_text_move = mv(vec![target(ObjectKind::Added, text_at)]);
        let picture_move = mv(vec![target(ObjectKind::Image, 0)]);
        let added_picture_move = mv(vec![target(ObjectKind::Added, image_at)]);
        let mixed = mv(vec![target(ObjectKind::Text, 0), target(ObjectKind::Image, 0)]);
        let missing = mv(vec![target(ObjectKind::Added, 9)]);
        let change_text = Edit::UpdateContent { page: 0, index: text_at, content: AddedContent::Text(text.clone()) };
        let delete_text = Edit::DeleteContent { page: 0, index: text_at };
        let delete_picture = Edit::DeleteContent { page: 0, index: image_at };
        let change_picture =
            Edit::UpdateContent { page: 0, index: image_at, content: app.session.get(id).unwrap().added()[image_at].content.clone() };
        assert!(all(&app, &[&text_move, &added_text_move, &change_text, &delete_text]));
        assert!(none(&app, &[&picture_move, &added_picture_move, &mixed, &missing, &change_picture, &delete_picture]));

        app.session.set_ownership(rules("edit.image"));
        assert!(all(&app, &[&picture_move, &added_picture_move, &change_picture, &delete_picture]));
        assert!(none(&app, &[&text_move, &added_text_move, &mixed, &missing, &change_text, &delete_text]));
    }

    /// An attachment opened from a locked document is read-only, so saving it can't reach the host.
    #[test]
    fn an_attachment_opens_read_only_under_the_lock() {
        let mut app = locked();
        app.session.set_open_ownership(with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, Vec::new()));
        app.open_bytes("a.pdf", None, pdf_with_attachment()).unwrap();
        let (_, id) = app.active_ids().unwrap();
        assert_eq!(app.session.get(id).unwrap().info.attachments.len(), 1);
        let before = app.views.len();
        app.attachment_action(id, 0, true);
        assert_eq!(app.views.len(), before + 1, "the attachment opened");
        let (_, new) = app.active_ids().unwrap();
        assert!(app.session.get(new).unwrap().ownership().read_only);
        assert!(!app.session.get(id).unwrap().ownership().read_only);
    }

    /// `pdf()` behind the user password "pw".
    fn encrypted_pdf() -> Vec<u8> {
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

    /// A one-page file with a password-protected PDF attached (hex in the stream: the file is binary).
    fn pdf_with_locked_attachment() -> Vec<u8> {
        let hex: String = encrypted_pdf().iter().map(|b| format!("{b:02x}")).collect::<String>() + ">";
        build(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles << /Names [(in.pdf) 4 0 R] >> >> >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".into(),
            "<< /Type /Page /Parent 2 0 R >>".into(),
            "<< /Type /Filespec /F (in.pdf) /UF (in.pdf) /EF << /F 5 0 R /UF 5 0 R >> >>".into(),
            format!("<< /Type /EmbeddedFile /Subtype /application#2Fpdf /Filter /ASCIIHexDecode /Length {} >>\nstream\n{hex}\nendstream", hex.len()),
        ])
    }

    /// The host's rules for the parent: an actor, shared annotations, a read-write viewer.
    fn host_rules() -> Ownership {
        Ownership { scope: EditScope::Viewer, actor: Some("alice".into()), annot_sync: true, hide_comments_panel: true, ..Default::default() }
    }

    fn only_looked_at(own: &Ownership) {
        assert!(own.read_only && !own.annot_sync && !own.hide_comments_panel, "{own:?}");
        assert_eq!(own.actor.as_deref(), Some("alice"));
    }

    /// A password-protected attachment is made after its prompt, when the session's rules are the
    /// host's again: it still has to be read-only and out of the annotation database.
    #[test]
    fn a_password_protected_attachment_opens_read_only_under_the_lock() {
        let mut app = locked();
        app.session.set_open_ownership(host_rules());
        app.open_bytes("a.pdf", None, pdf_with_locked_attachment()).unwrap();
        let (_, parent) = app.active_ids().unwrap();
        app.attachment_action(parent, 0, true);
        assert!(app.password_prompt.is_some(), "the attachment asks for its password");
        assert!(!app.session.open_ownership().read_only, "the session's rules went back to the host's");
        app.submit_password(Some("pw".into()));
        let (_, new) = app.active_ids().unwrap();
        assert_ne!(new, parent);
        only_looked_at(app.session.get(new).unwrap().ownership());
        let p = app.session.get(parent).unwrap().ownership();
        assert!(!p.read_only && p.annot_sync, "the host's document is as it was");
        assert!(!app.session.open_ownership().read_only, "and a later open is the host's");
    }

    /// The same through the open worker, which makes the document a few frames after the answer.
    #[test]
    fn a_password_protected_attachment_opened_by_the_worker_is_read_only_too() {
        let mut app = PdfCraftApp::new();
        app.host_lock = true;
        app.session.set_open_ownership(host_rules());
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.state_mut().open_bytes("a.pdf", None, pdf_with_locked_attachment()).unwrap();
        h.run_steps(2);
        let (_, parent) = h.state().active_ids().unwrap();
        h.state_mut().attachment_action(parent, 0, true);
        h.run_steps(2);
        assert!(h.state().password_prompt.is_some());
        h.state_mut().submit_password(Some("pw".into()));
        // The session's rules change while the worker is busy: the answer's rules still hold.
        h.state_mut().session.set_open_ownership(Ownership { scope: EditScope::All, ..Default::default() });
        for _ in 0..100 {
            h.run_steps(2);
            if h.state().active_ids().is_some_and(|(_, id)| id != parent) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (_, new) = h.state().active_ids().unwrap();
        assert_ne!(new, parent);
        only_looked_at(h.state().session.get(new).unwrap().ownership());
        assert_eq!(h.state().session.open_ownership().scope, EditScope::All, "the rules in force are put back");
    }

    /// Only the page tool the administrator turned on works; the grid's other edits do not.
    #[test]
    fn one_page_tool_on_does_not_open_the_others() {
        let mut app = locked();
        app.session.set_open_ownership(with_policy(Ownership { scope: EditScope::Viewer, ..Default::default() }, vec![("page.rotate".into(), true)]));
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        let n = app.session.get(app.active_ids().unwrap().1).unwrap().info.pages.len();
        assert!(app.apply_edit(pdfcraft_engine::Edit::RotatePages { pages: vec![0], degrees: 90 }));
        assert!(!app.apply_edit(pdfcraft_engine::Edit::DeletePages { pages: vec![0] }));
        assert!(!app.apply_edit(pdfcraft_engine::Edit::MovePages { pages: vec![0], to: 1 }));
        assert!(!app.apply_edit(pdfcraft_engine::Edit::DuplicatePages { pages: vec![0], at: None }));
        assert_eq!(app.session.get(app.active_ids().unwrap().1).unwrap().info.pages.len(), n);
    }

    /// A refused action says so in Korean in a Korean UI and in English in any other.
    #[test]
    fn the_refusal_text_follows_the_language() {
        use crate::i18n::{Lang, set_current};
        set_current(Lang::from_code("ko").unwrap());
        assert_eq!(host_off_text(), HOST_OFF_KO);
        set_current(Lang::EN);
        assert_eq!(host_off_text(), HOST_OFF);
    }

    /// A host build opens in the automatic language (Korean) even when the saved preferences name
    /// another; the upstream build keeps the saved choice.
    #[test]
    fn a_saved_language_is_not_restored_in_a_host_build() {
        let mut app = PdfCraftApp::new();
        app.restore(r#"{"language":"en"}"#);
        let expected = if HOST_LOCK { crate::i18n::AUTO } else { "en" };
        assert_eq!(app.language, expected);
    }

    /// Every command in the manifest carries a Korean label, so the admin screen never shows an
    /// English-only row. The one exception is a bare term Korean keeps as is (PDF/A).
    #[test]
    fn every_label_ko_is_filled() {
        let manifest: serde_json::Value = serde_json::from_str(&commands_manifest()).unwrap();
        let hangul = |s: &str| s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c));
        for c in manifest["commands"].as_array().unwrap() {
            let (id, label, ko) = (c["id"].as_str().unwrap(), c["label"].as_str().unwrap(), c["labelKo"].as_str().unwrap_or_default());
            assert!(!ko.trim().is_empty(), "{id}: labelKo is empty");
            assert!(hangul(ko) || id == "standards.pdfa" && ko == label, "{id}: labelKo is not Korean: {ko}");
        }
    }

    /// `ppm/commands.json` is what the admin screen reads; it must say what the table says.
    /// `UPDATE_COMMANDS_JSON=1 cargo test -p pdfcraft-ui-egui --lib commands_json` rewrites it.
    #[test]
    fn the_checked_in_commands_json_matches_the_table() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ppm/commands.json");
        let now = commands_manifest();
        if std::env::var_os("UPDATE_COMMANDS_JSON").is_some() {
            std::fs::write(path, &now).unwrap();
        }
        let file = std::fs::read_to_string(path).expect("ppm/commands.json exists (UPDATE_COMMANDS_JSON=1 writes it)");
        assert!(file == now, "ppm/commands.json is out of date: run with UPDATE_COMMANDS_JSON=1");
        let v: serde_json::Value = serde_json::from_str(&file).unwrap();
        assert_eq!(v["commands"].as_array().unwrap().len(), COMMANDS.len() + POLICY_ONLY.len());
    }

    /// `pdfcraft:open` `mode`: `builder` is the only way to the wider scope.
    #[test]
    fn the_mode_value_is_passed_through_and_defaults_to_the_narrowest() {
        assert_eq!(parse_mode(Some("builder")), EditScope::All);
        assert_eq!(parse_mode(Some("viewer")), EditScope::Viewer);
        assert_eq!(parse_mode(None), EditScope::Viewer, "no mode, or a value of another type");
        for odd in ["", "Builder", "admin", "builder ", "all"] {
            assert_eq!(parse_mode(Some(odd)), EditScope::Viewer, "{odd:?}");
        }
        // Without the lock there are no modes.
        if !HOST_LOCK {
            assert_eq!(open_scope(Some("viewer")), EditScope::All);
        } else {
            assert_eq!(open_scope(Some("viewer")), EditScope::Viewer);
            assert_eq!(open_scope(Some("builder")), EditScope::All);
        }
    }

    #[test]
    fn a_locked_build_runs_only_what_the_scope_allows() {
        let mut app = locked();
        assert!(app.host_allows("comment.note") && app.host_allows("help.about"));
        assert!(!app.host_allows("file.open") && !app.host_allows("page.rotate"), "no document: a viewer's commands only");
        app.session.set_open_ownership(Ownership { scope: EditScope::Viewer, ..Default::default() });
        app.open_bytes("v.pdf", None, pdf()).unwrap();
        assert!(!app.host_allows("page.rotate") && !app.host_allows("redact.apply"));
        assert!(app.host_viewer() && !app.host_allows("sign.fill.text") && !app.host_allows("sign.fill.signature"), "Fill & Sign is not in /viewer");
        assert!(!app.execute("page.rotate") && !app.execute("file.open") && !app.execute("print.dialog"), "refused without a word");
        assert!(app.toast.is_none() && app.dialog.is_none());
        app.session.set_open_ownership(Ownership { scope: EditScope::All, ..Default::default() });
        app.open_bytes("b.pdf", None, pdf()).unwrap();
        assert!(app.host_allows("page.rotate") && app.host_allows("redact.apply"), "a builder edits the document");
        assert!(!app.host_viewer() && app.host_allows("sign.fill.text"), "and fills and signs");
        assert!(!app.host_allows("file.open") && !app.host_allows("file.save_as") && !app.host_allows("print.dialog"), "but still not the files");
        // The same commands run in a build without the lock.
        app.host_lock = false;
        assert!(app.host_allows("file.open"));
    }

    /// Protection and digital signing stay shut in a builder's document, whichever way they are
    /// started: a command (menu, shortcut, palette), the edit behind it, or the signing flow.
    #[test]
    fn a_builder_cannot_protect_or_digitally_sign() {
        const SHUT: &[&str] = &[
            "protect.password",
            "protect.remove",
            "protect.unlock",
            "protect.properties",
            "sign.digital",
            "sign.certify",
            "sign.certify_invisible",
        ];
        let mut app = locked();
        app.session.set_open_ownership(Ownership { scope: EditScope::All, ..Default::default() });
        app.open_bytes("b.pdf", None, pdf()).unwrap();
        for id in SHUT {
            assert_eq!(grade(id, EditScope::All), Grade::Locked, "{id}");
            assert!(!app.host_allows(id) && !app.execute(id), "{id}");
            let overrides = [(id.to_string(), true)];
            assert!(!enabled(id, EditScope::All, &overrides), "{id}: no policy opens it");
        }
        assert!(app.dialog.is_none() && app.quick_tool == crate::QuickTool::Select && app.sign_draft.is_none());
        assert!(app.host_allows("sign.fill.signature") && app.host_allows("sign.fill.text") && app.host_allows("comment.stamp"));
        // The edits and the signing flow check too, for a start that isn't a command (a click on a signature field).
        assert!(!app.apply_edit(pdfcraft_engine::Edit::RemoveProtection));
        app.start_signing(0, None, Some("Sig1".into()), None);
        assert!(app.dialog.is_none() && app.sign_draft.is_none(), "the signing dialog doesn't open");
        assert!(app.finish_signing().is_err());
    }

    #[test]
    fn a_locked_build_shows_no_blocked_command_in_the_palette_or_menus() {
        let app = locked();
        let shown: Vec<&str> = COMMANDS.iter().map(|c| c.id).filter(|id| app.host_allows(id)).collect();
        assert!(shown.contains(&"comment.note") && shown.contains(&"help.about"));
        for id in LOCKED.iter().chain(OFF).chain(BUILDER).chain(BUILDER_ONLY) {
            assert!(!shown.contains(id), "{id}");
        }
    }

    #[test]
    fn a_locked_build_opens_nothing_the_user_picks_or_pastes() {
        let mut app = locked();
        app.pick_override = Some(vec!["/does/not/matter.pdf".into()]);
        app.open_dialog();
        app.pick(PickFor::Open, rfd::AsyncFileDialog::new(), true);
        assert!(!app.pickers.pending(), "no picker was started");
        app.open_recent("/does/not/matter.pdf");
        app.open_path("/does/not/matter.pdf");
        assert!(app.views.is_empty() && app.toast.is_none());
        // What lands in the user's inbox (a pick, a drop) is never opened.
        app.inbox.lock().unwrap().push(("x.pdf".into(), pdf()));
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(3);
        assert!(h.state().views.is_empty(), "the user's file was not opened");
    }

    #[test]
    fn a_locked_build_opens_no_web_links() {
        let mut app = locked();
        app.open_url("https://example.com", LINK_CMDS);
        assert_eq!(app.last_opened_url, None);
        app.request_document_url("https://example.com", LinkOrigin::Link);
        assert!(app.pending_link.is_none(), "a document's link doesn't even ask");
        app.host_lock = false;
        app.open_url("https://example.com", LINK_CMDS);
        assert_eq!(app.last_opened_url.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn a_locked_build_without_a_host_opens_no_document() {
        let app = locked();
        assert!(app.host_missing());
        app.startup_inbox.lock().unwrap().push(("doc.pdf".into(), pdf()));
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        assert!(h.state().views.is_empty(), "no host: nothing opens");
        assert_eq!(h.state().startup_inbox.lock().unwrap().len(), 1, "the file stays where it was, unread");
        // With a host connected the same file opens, as a viewer's.
        h.state_mut().host_save = Some(Box::new(|_, _, _, _| Ok(())));
        h.run_steps(4);
        let state = h.state();
        assert_eq!(state.views.len(), 1);
        let (_, id) = state.active_ids().unwrap();
        assert_eq!(state.session.get(id).unwrap().ownership().scope, EditScope::Viewer, "no mode sent: the narrowest");
    }

    #[test]
    fn the_hosts_mode_reaches_the_document() {
        let app = locked();
        app.startup_inbox.lock().unwrap().push(("doc.pdf".into(), pdf()));
        app.ownership_inbox.lock().unwrap().push(Ownership { scope: open_scope_for_test("builder"), ..Default::default() });
        let mut app = app;
        app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
        let mut h = egui_kittest::Harness::builder().build_eframe(move |_| app);
        h.run_steps(4);
        let state = h.state();
        let (_, id) = state.active_ids().unwrap();
        assert_eq!(state.session.get(id).unwrap().ownership().scope, EditScope::All);
    }

    /// A locked app showing a one-page document opened with `own`, its quick bar drawn.
    fn quick_bar_harness(own: Ownership) -> egui_kittest::Harness<'static, PdfCraftApp> {
        let mut app = locked();
        app.host_save = Some(Box::new(|_, _, _, _| Ok(())));
        app.set_option("language", "en").unwrap();
        app.session.set_open_ownership(own);
        app.open_bytes("a.pdf", None, pdf()).unwrap();
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_| app);
        h.run_steps(4);
        h
    }

    /// The markup and drawing tools are plain buttons: one click picks one, and a tool the policy
    /// turned off has no button. The same in a viewer's and in a builder's document.
    #[test]
    fn the_markup_and_drawing_tools_are_buttons_of_the_quick_bar() {
        use crate::comments::{CommentTool, GROUPS};
        use egui_kittest::kittest::Queryable;
        let direct: Vec<_> = GROUPS[1].iter().chain(GROUPS[2]).copied().collect();
        for scope in [EditScope::Viewer, EditScope::All] {
            let off = [("comment.underline".to_string(), false), ("comment.cloud".to_string(), false)];
            let mut h = quick_bar_harness(with_policy(Ownership { scope, ..Default::default() }, off.to_vec()));
            for tool in &direct {
                let shown = h.query_by_label(tool.label()).is_some();
                let policy = h.state().host_allows(tool.command());
                assert_eq!(shown, policy, "{scope:?} {}: a button exactly when the policy allows it", tool.label());
            }
            assert!(h.state().host_allows("comment.highlight"), "{scope:?}: the highlighter is on");
            let on: Vec<_> = direct.iter().filter(|t| h.state().host_allows(t.command())).collect();
            for tool in on {
                h.get_by_label(tool.label()).click();
                h.run_steps(2);
                assert_eq!(h.state().quick_tool, crate::QuickTool::Comment(*tool), "{scope:?} {}: one click picks it", tool.label());
            }
            assert!(h.query_by_label(CommentTool::Underline.label()).is_none() && h.query_by_label(CommentTool::Cloud.label()).is_none());
        }
    }

    /// `parse_mode`, whatever the build: the document gets what the host said.
    fn open_scope_for_test(mode: &str) -> EditScope {
        parse_mode(Some(mode))
    }
}
