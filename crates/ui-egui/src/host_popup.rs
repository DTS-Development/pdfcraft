//! The host's buttons on the bar of a selected annotation (`pdfcraft:annotation-popup-actions`).
//!
//! The host sets a list of `{ id, label, icon }` (at most 8). While an annotation of a shared document
//! (`annotSync`) is selected, a small bar with one icon button per action floats next to it; a click tells
//! the page `pdfcraft:annotation-popup-action` with the action's `id`, the annotation's id, its page (from 0)
//! and its place on the page as shown (the same space as `sign-box-selected` and `goto`).
//!
//! Not for sign boxes (they have their own message) or replies; signatures are included. The list is the
//! host's state: it stays for every document, also those opened later, until the host sets another or
//! `null`. It is drawn from the list each frame, so a new list shows in an open bar at once. The bar
//! does not need the user to be allowed to edit: it only reports the click (viewer and `readOnly` too).

use std::sync::Arc;

use egui::{Rect, pos2};

use crate::canvas::DocView;

/// The most buttons the host can ask for.
pub const MAX_ACTIONS: usize = 8;
/// Longest `id`, in characters (alphanumerics and `-_.:`).
pub const MAX_ID_CHARS: usize = 64;
/// Longest `label`, in UTF-16 units (it is the button's tooltip and name for assistive technology).
pub const MAX_LABEL_UNITS: usize = 20;

/// One button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupAction {
    pub id: String,
    pub label: String,
    /// The name of one of the embedded icons (`icons::exists`).
    pub icon: String,
}

/// A click waiting for the app to report it: the action's id and the annotation's id (its position can move
/// before the click is reported, so it is looked up again then).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PopupClick {
    pub id: String,
    pub name: String,
}

/// The `actions` of a `pdfcraft:annotation-popup-actions` message: `null`, missing or `[]` take the buttons away.
/// Anything else that is not exactly a list of valid buttons is an error, and the page is left with none (never a stale list).
pub fn parse_actions(value: Option<&serde_json::Value>) -> Result<Vec<PopupAction>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else { return Ok(Vec::new()) };
    let list = value.as_array().ok_or("actions must be an array or null")?;
    if list.len() > MAX_ACTIONS {
        return Err(format!("at most {MAX_ACTIONS} actions"));
    }
    let mut out: Vec<PopupAction> = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| format!("actions[{i}] must be an object"))?;
        if let Some(key) = obj.keys().find(|k| !matches!(k.as_str(), "id" | "label" | "icon")) {
            return Err(format!("actions[{i}]: unknown field {key}"));
        }
        let text = |key: &str| obj.get(key).and_then(|v| v.as_str()).ok_or_else(|| format!("actions[{i}].{key} must be a string"));
        let (id, label, icon) = (text("id")?, text("label")?, text("icon")?);
        let id_ok = !id.is_empty()
            && id.chars().count() <= MAX_ID_CHARS
            && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
        if !id_ok {
            return Err(format!("actions[{i}].id must be 1 to {MAX_ID_CHARS} characters of letters, digits and -_.:"));
        }
        let units = label.encode_utf16().count();
        if units == 0 || units > MAX_LABEL_UNITS || label.chars().any(char::is_control) || label != label.trim() {
            return Err(format!("actions[{i}].label must be 1 to {MAX_LABEL_UNITS} characters, without control characters or outer spaces"));
        }
        if !crate::icons::exists(icon) {
            return Err(format!("actions[{i}].icon: unknown icon {icon}"));
        }
        if out.iter().any(|a| a.id == id) {
            return Err(format!("actions[{i}].id {id} is used twice"));
        }
        out.push(PopupAction { id: id.into(), label: label.into(), icon: icon.into() });
    }
    Ok(out)
}

/// Draw the bar next to the selected annotation of `doc`'s view, and note a click in `view.host_popup_click`.
pub(crate) fn show(ui: &mut egui::Ui, viewport: Rect, view: &mut DocView, doc: &pdfcraft_engine::Document) {
    if view.host_popup.is_empty() || !doc.annot_sync() || doc.comments_hidden() || ui.input(|i| i.pointer.is_decidedly_dragging()) {
        return;
    }
    let Some((page, index)) = view.comments.selected else { return };
    let Some(a) = doc.info.annotations.iter().find(|a| a.page == page && a.index == index) else { return };
    // Replies and pop-ups are not comments of their own; a sign box has its own message; the page needs a name to refer to it.
    if a.is_reply()
        || a.subtype == "Popup"
        || a.subtype == "Widget"
        || doc.sign_box_at(page, index).is_some()
        || doc.annot_name_at(page, index).is_none()
    {
        return;
    }
    let Some(xf) = view.page_xform(page) else { return };
    let rect = xf.user_rect(&doc.info, page, a.rect);
    if !viewport.intersects(rect) {
        return;
    }
    let actions: Arc<Vec<PopupAction>> = view.host_popup.clone();
    let width = actions.len() as f32 * 34.0 + 12.0;
    let x = (rect.center().x - width / 2.0).clamp(viewport.left() + 8.0, (viewport.right() - width).max(viewport.left() + 8.0));
    let y = if rect.top() - 46.0 > viewport.top() { rect.top() - 42.0 } else { (rect.bottom() + 6.0).min(viewport.bottom() - 42.0) };
    let mut chosen = None;
    egui::Area::new(egui::Id::new(("host-popup-actions", view.id.0))).order(egui::Order::Foreground).fixed_pos(pos2(x, y)).show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                for action in actions.iter() {
                    if crate::icons::button(ui, &action.icon, 30.0, false, &action.label).clicked() {
                        chosen = Some(action.id.clone());
                    }
                }
            });
        });
    });
    if let Some(id) = chosen
        && let Some(name) = doc.annot_name_at(page, index)
    {
        view.host_popup_click = Some(PopupClick { id, name });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(v: serde_json::Value) -> Vec<PopupAction> {
        parse_actions(Some(&v)).unwrap()
    }

    #[test]
    fn nothing_or_an_empty_list_takes_the_buttons_away() {
        assert!(parse_actions(None).unwrap().is_empty());
        assert!(parse_actions(Some(&json!(null))).unwrap().is_empty());
        assert!(ok(json!([])).is_empty());
    }

    #[test]
    fn reads_a_list_in_order() {
        let list =
            ok(json!([{"id": "open-sheet", "label": "코멘트", "icon": "message-square"}, {"id": "go:1.a_b", "label": "Locate", "icon": "compass"}]));
        assert_eq!(list.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["open-sheet", "go:1.a_b"]);
        assert_eq!(list[0], PopupAction { id: "open-sheet".into(), label: "코멘트".into(), icon: "message-square".into() });
    }

    #[test]
    fn refuses_what_is_not_exactly_a_valid_list() {
        let a = |id: &str, label: &str, icon: &str| json!({"id": id, "label": label, "icon": icon});
        let bad = [
            json!("x"),
            json!({}),
            json!([1]),
            json!([a("", "L", "compass")]),
            json!([a(&"i".repeat(65), "L", "compass")]),
            json!([a("a b", "L", "compass")]),
            json!([a("é", "L", "compass")]),
            json!([a("a", "", "compass")]),
            json!([a("a", &"가".repeat(21), "compass")]),
            json!([a("a", " L", "compass")]),
            json!([a("a", "L\n", "compass")]),
            json!([a("a", "L", "no-such-icon")]),
            json!([a("a", "L", "compass"), a("a", "M", "compass")]),
            json!([{"id": "a", "label": "L"}]),
            json!([{"id": "a", "label": "L", "icon": "compass", "svg": "<svg/>"}]),
            json!((0..9).map(|i| a(&format!("a{i}"), "L", "compass")).collect::<Vec<_>>()),
        ];
        for value in bad {
            assert!(parse_actions(Some(&value)).is_err(), "{value}");
        }
        // The limits themselves are fine.
        assert_eq!(ok(json!((0..8).map(|i| a(&format!("a{i}"), &"가".repeat(20), "compass")).collect::<Vec<_>>())).len(), 8);
        assert!(parse_actions(Some(&json!([a(&"i".repeat(64), "L", "compass")]))).is_ok());
    }
}
