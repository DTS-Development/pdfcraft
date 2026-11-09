//! Who may apply which edit, when the embedding page named an `actor` or made the session
//! read-only (see [`crate::Ownership`] and `pdfcraft_annot::ownership`). Every edit passes
//! [`check`] before it runs, whether on its own or inside a `Batch`.

use pdfcraft_annot::ownership as owner;

use crate::{Edit, EditClass, EditClasses, EditError, EditScope, Ownership};

/// Refuse `edit` on `doc` when the session is read-only, or when it would change a comment the
/// actor doesn't own. What an edit does to comments beyond its named target (page operations,
/// flattening, redaction, sanitizing) is held to the same rule afterwards, see
/// [`crate::run_edit_checked`].
pub(crate) fn check(doc: &pdfcraft_cos::Document, edit: &Edit, own: &Ownership) -> Result<(), EditError> {
    // A batch is only as read-only as the edits in it; an empty one changes nothing.
    if own.read_only && !matches!(edit, Edit::Batch { .. }) {
        return Err(EditError::HostReadOnly);
    }
    // A viewer takes part in comments, not in the document's own links and form widgets, whether
    // or not an actor is named.
    if own.scope == EditScope::Viewer
        && let Edit::DeleteAnnotation { page, index }
        | Edit::MoveAnnotation { page, index, .. }
        | Edit::ResizeAnnotation { page, index, .. }
        | Edit::StyleAnnotation { page, index, .. }
        | Edit::FillAnnotation { page, index, .. }
        | Edit::FontSizeAnnotation { page, index, .. }
        | Edit::SetAnnotationInfo { page, index, .. }
        | Edit::LockAnnotation { page, index, .. } = edit
        && owner::is_layout_annotation(doc, *page, *index)
    {
        return Err(EditError::HostViewerOnly);
    }
    // The server refuses a reply to a reply, so a document that shares its annotations does too.
    if own.annot_sync {
        crate::annot_sync::check_reply_target(doc, edit)?;
    }
    let Some(actor) = own.actor.as_deref() else { return Ok(()) };
    match edit {
        Edit::DeleteAnnotation { page, index } => owner::check_delete(doc, *page, *index, actor)?,
        // Replying is how anyone takes part in someone else's comment; setting its status or
        // marking it changes it, so those are the owner's.
        Edit::SetAnnotationContents { page, index, .. }
        | Edit::SetAnnotationStatus { page, index, .. }
        | Edit::MarkAnnotation { page, index, .. }
        | Edit::LockAnnotation { page, index, .. }
        | Edit::MoveAnnotation { page, index, .. }
        | Edit::ResizeAnnotation { page, index, .. }
        | Edit::StyleAnnotation { page, index, .. }
        | Edit::FillAnnotation { page, index, .. }
        | Edit::FontSizeAnnotation { page, index, .. }
        | Edit::SetAnnotationInfo { page, index, .. }
        | Edit::EraseInk { page, index, .. } => owner::check_target(doc, *page, *index, actor)?,
        // Pages go with their comments; a copy of someone else's comment would be a new comment
        // that says it is theirs.
        Edit::DeletePages { pages } | Edit::DuplicatePages { pages, .. } => owner::check_pages(doc, pages, actor)?,
        Edit::Flatten { comments: true, .. } => owner::check_all_pages(doc, actor)?,
        _ => {}
    }
    Ok(())
}

/// Edits that can bring new comments into the document (their own, copied pages' or imported).
/// Each comment they add is marked as the actor's right after the edit.
pub(crate) fn creates_comments(edit: &Edit) -> bool {
    matches!(
        edit,
        Edit::AddMeasurement(_)
            | Edit::AddAnnotation(_)
            | Edit::AddCustomStamp { .. }
            | Edit::AddSignBox { .. }
            | Edit::SignInBox { .. }
            | Edit::ReplyToAnnotation { .. }
            | Edit::SetAnnotationStatus { .. }
            | Edit::MarkAnnotation { .. }
            | Edit::ReplaceText { .. }
            | Edit::EraseInk { .. }
            | Edit::ImportData { .. }
            | Edit::InsertPagesFrom { .. }
            | Edit::DuplicatePages { .. }
            | Edit::ReplacePages { .. }
    )
}

/// What a viewer's host policy decides about an edit.
enum Work {
    /// Comments of every kind, answering them, filling in form values: always a viewer's.
    Comment,
    /// Page operations.
    Page,
    /// Direct editing of text, pictures and links.
    Content,
    /// Measurements.
    Measure,
    /// Everything else, which no policy opens to a viewer.
    Other,
}

/// Refuse `edit` when the host opened the document as a viewer and the edit is not a comment or
/// something its policy turned on. A `Batch` passes here and is held to the rule edit by edit,
/// as each one goes through [`crate::run_edit`].
pub(crate) fn check_scope(edit: &Edit, scope: EditScope, allow: EditClasses) -> Result<(), EditError> {
    if scope == EditScope::All {
        return Ok(());
    }
    let ok = match work(edit) {
        Work::Comment => true,
        Work::Page => allow.has(EditClass::Page),
        Work::Content => allow.has(EditClass::Content),
        Work::Measure => allow.has(EditClass::Measure),
        Work::Other => false,
    };
    if ok { Ok(()) } else { Err(EditError::HostViewerOnly) }
}

/// Which kind of work `edit` is. Every variant is named, so a new edit can't take effect in a
/// viewer before someone has put it on one side.
fn work(edit: &Edit) -> Work {
    match edit {
        Edit::AddAnnotation(..)
        | Edit::AddCustomStamp { .. }
        | Edit::AddSignBox { .. }
        | Edit::SignInBox { .. }
        | Edit::DeleteAnnotation { .. }
        | Edit::SetAnnotationContents { .. }
        | Edit::ReplyToAnnotation { .. }
        | Edit::SetAnnotationStatus { .. }
        | Edit::EraseInk { .. }
        | Edit::ReplaceText { .. }
        | Edit::MarkAnnotation { .. }
        | Edit::LockAnnotation { .. }
        | Edit::MoveAnnotation { .. }
        | Edit::ResizeAnnotation { .. }
        | Edit::StyleAnnotation { .. }
        | Edit::FillAnnotation { .. }
        | Edit::FontSizeAnnotation { .. }
        | Edit::SetAnnotationInfo { .. }
        | Edit::SetFieldValue { .. }
        | Edit::ResetForm { .. }
        | Edit::ApplyScriptChanges { .. }
        | Edit::XfaEvent { .. }
        | Edit::Batch { .. } => Work::Comment,
        Edit::RotatePages { .. }
        | Edit::DeletePages { .. }
        | Edit::MovePages { .. }
        | Edit::ReversePages { .. }
        | Edit::InsertBlankPage { .. }
        | Edit::DuplicatePages { .. }
        | Edit::ReplacePages { .. }
        | Edit::SetPageBox { .. }
        | Edit::InsertPagesFrom { .. }
        | Edit::NumberPages { .. } => Work::Page,
        Edit::EditTextLine { .. }
        | Edit::EditPageImage { .. }
        | Edit::MoveObjects { .. }
        | Edit::EditTextBlock { .. }
        | Edit::RegisterTextFont { .. }
        | Edit::AddText { .. }
        | Edit::AddImage { .. }
        | Edit::UpdateContent { .. }
        | Edit::DeleteContent { .. }
        | Edit::ReplaceImage { .. }
        | Edit::AddLink { .. }
        | Edit::SetLink { .. }
        | Edit::DeleteLink { .. }
        | Edit::RemoveLinks { .. }
        | Edit::AddLinks { .. } => Work::Content,
        Edit::AddMeasurement(..) | Edit::SetMeasurementScale { .. } => Work::Measure,
        Edit::SetTransition { .. }
        | Edit::SetInfo { .. }
        | Edit::AddBookmark { .. }
        | Edit::RenameBookmark { .. }
        | Edit::DeleteBookmark { .. }
        | Edit::MoveBookmark { .. }
        | Edit::SetBookmarkPage { .. }
        | Edit::BookmarksFromStructure
        | Edit::AddField { .. }
        | Edit::SetFieldProps { .. }
        | Edit::SetFieldImage { .. }
        | Edit::DuplicateField { .. }
        | Edit::DeleteField { .. }
        | Edit::SetTabOrder { .. }
        | Edit::SetInitialView(..)
        | Edit::AutoTagText
        | Edit::SetAltText { .. }
        | Edit::AutoTagFormFields
        | Edit::SetTagProperty { .. }
        | Edit::MoveTag { .. }
        | Edit::MarkDecorative { .. }
        | Edit::MarkTagArtifact { .. }
        | Edit::SetDocumentScript { .. }
        | Edit::SetFieldScript { .. }
        | Edit::ConvertPdfA { .. }
        | Edit::AddOcrText { .. }
        | Edit::MoveInTabOrder { .. }
        | Edit::AddHeaderFooter { .. }
        | Edit::AddWatermark { .. }
        | Edit::AddBackground { .. }
        | Edit::RemoveMarks { .. }
        | Edit::ApplyRedactions { .. }
        | Edit::ClearRedactions
        | Edit::ImportData { .. }
        | Edit::RemoveHidden { .. }
        | Edit::Sanitize
        | Edit::XfaToStatic
        | Edit::Flatten { .. }
        | Edit::FlattenFillSign
        | Edit::Protect(..)
        | Edit::RemoveProtection => Work::Other,
    }
}
