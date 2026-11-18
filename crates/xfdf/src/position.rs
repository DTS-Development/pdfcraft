//! Annotations addressed by position: page and index in the page's `/Annots`.
//!
//! A file that comes from another tool has annotations without `/NM`, which the name-based
//! functions of [`super::single`] can't reach. These work on `(page, index)` for every
//! annotation, the index being the entry's original place in the page's `/Annots` array (every
//! subtype counts, links and pop-ups too), so a list taken once stays valid for the exports and
//! the strip that follow it as long as the document isn't changed in between.

use super::single::{MAX_XFDF_LEN, MAX_XFDF_LEN_CEILING, annots_of, element_xfdf};
use std::collections::HashSet;

use super::*;

/// Subtypes that have an XFDF form here but that the host's database doesn't take.
const NOT_EXCHANGED: [&[u8]; 3] = [b"FileAttachment", b"Sound", b"Redact"];

/// One entry of a page's `/Annots`.
#[derive(Clone, Debug, PartialEq)]
pub struct AnnotEntry {
    /// Zero-based page.
    pub page: usize,
    /// Place in the page's `/Annots` array.
    pub index: usize,
    pub subtype: String,
    /// The `/NM`, if any.
    pub nm: Option<String>,
    /// The `/T` (author).
    pub title: Option<String>,
    /// Page and index of the annotation this one replies to (`/IRT`).
    pub irt: Option<(usize, usize)>,
    /// Whether [`export_positions`] can turn it into XFDF that [`apply_remote`] puts back: false for
    /// links, widgets, pop-ups, attachments, sounds, redactions, kinds without an XFDF form, an
    /// XFDF over the size limit, and a reply whose parent isn't itself exchangeable.
    pub exportable: bool,
}

/// What to export: the annotation at (`page`, `index`), as `name`, replying to `in_reply_to`.
#[derive(Clone, Copy, Debug)]
pub struct ExportRequest<'a> {
    pub page: usize,
    pub index: usize,
    pub name: &'a str,
    pub in_reply_to: Option<&'a str>,
}

fn entries(doc: &Document) -> Vec<(usize, usize, ObjRef)> {
    let mut out = Vec::new();
    for (pi, p) in pdfcraft_model::pages(doc).iter().enumerate() {
        for (i, a) in annots_of(doc, p.obj).iter().enumerate() {
            if let Some(r) = a.as_ref() {
                out.push((pi, i, r));
            }
        }
    }
    out
}

/// Every entry of every page's `/Annots`, in order. An entry that isn't a reference to a
/// dictionary is skipped (its index is still counted).
pub fn list_annotations(doc: &Document) -> Vec<AnnotEntry> {
    list_annotations_with(doc, MAX_XFDF_LEN)
}

/// [`list_annotations`] with the export limit the host raised (see [`AnnotIndex::with_max_len`]).
pub fn list_annotations_with(doc: &Document, max_len: usize) -> Vec<AnnotEntry> {
    let max_len = max_len.clamp(MAX_XFDF_LEN, MAX_XFDF_LEN_CEILING);
    let found = entries(doc);
    let at: HashMap<ObjRef, (usize, usize)> = found.iter().map(|&(p, i, r)| (r, (p, i))).collect();
    let mut list: Vec<AnnotEntry> = found
        .iter()
        .filter_map(|&(page, index, r)| {
            let obj = doc.get(r);
            let d = obj.as_dict()?;
            let subtype = d.name(b"Subtype").map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
            let irt = d.get(b"IRT").and_then(Object::as_ref).and_then(|t| at.get(&t).copied());
            // Trial export under placeholder names: everything but the name decides it.
            let mut names: HashMap<ObjRef, String> = HashMap::from([(r, "a".to_string())]);
            if let Some(t) = d.get(b"IRT").and_then(Object::as_ref) {
                names.entry(t).or_insert_with(|| "b".to_string());
            }
            let kind_ok = SUBTYPES.iter().any(|(s, _)| s.as_bytes() == subtype.as_bytes()) && !NOT_EXCHANGED.contains(&subtype.as_bytes());
            let exportable = kind_ok && (d.get(b"IRT").is_none() || irt.is_some()) && element_xfdf(doc, d, page, &names, "a", max_len).is_ok();
            Some(AnnotEntry { page, index, subtype, nm: text_of(doc, d, b"NM"), title: text_of(doc, d, b"T"), irt, exportable })
        })
        .collect();
    // A reply to something that can't be exchanged can't be either.
    loop {
        let bad: HashSet<(usize, usize)> = list.iter().filter(|e| !e.exportable).map(|e| (e.page, e.index)).collect();
        let mut changed = false;
        for e in list.iter_mut() {
            if e.exportable && e.irt.is_some_and(|t| bad.contains(&t)) {
                e.exportable = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    list
}

/// The XFDF of each requested annotation, in the order of `requests`. `name` becomes the
/// element's `name` (whatever the file's `/NM` is) and `in_reply_to` the `inreplyto` of a reply.
/// The document is not changed.
pub fn export_positions(doc: &Document, requests: &[ExportRequest]) -> Vec<Result<AnnotXfdf, DataError>> {
    export_positions_with(doc, requests, MAX_XFDF_LEN)
}

/// [`export_positions`] with the export limit the host raised (clamped like [`AnnotIndex::with_max_len`]).
pub fn export_positions_with(doc: &Document, requests: &[ExportRequest], max_len: usize) -> Vec<Result<AnnotXfdf, DataError>> {
    let max_len = max_len.clamp(MAX_XFDF_LEN, MAX_XFDF_LEN_CEILING);
    let pages = pdfcraft_model::pages(doc);
    // Each page's /Annots is read once, not once per request.
    let mut lists: HashMap<usize, Vec<Object>> = HashMap::new();
    let mut targets: Vec<Option<ObjRef>> = Vec::with_capacity(requests.len());
    for r in requests {
        let list = pages.get(r.page).map(|p| lists.entry(r.page).or_insert_with(|| annots_of(doc, p.obj)));
        targets.push(list.and_then(|l| l.get(r.index)).and_then(Object::as_ref));
    }
    let mut names: HashMap<ObjRef, String> = HashMap::new();
    for (r, t) in requests.iter().zip(&targets) {
        if let Some(obj) = *t {
            names.insert(obj, r.name.to_string());
        }
    }
    // A parent that wasn't asked for still gets the name the caller gave for the reply link.
    for (r, t) in requests.iter().zip(&targets) {
        if let (Some(obj), Some(parent)) = (*t, r.in_reply_to)
            && let Some(irt) = doc.get(obj).as_dict().and_then(|d| d.get(b"IRT")).and_then(Object::as_ref)
        {
            names.entry(irt).or_insert_with(|| parent.to_string());
        }
    }
    let mut out = Vec::with_capacity(requests.len());
    for (r, t) in requests.iter().zip(&targets) {
        let Some(obj) = *t else {
            out.push(Err(DataError::NotFound(format!("page {} index {}", r.page + 1, r.index))));
            continue;
        };
        let mut d = doc.get(obj).as_dict().cloned().unwrap_or_default();
        // The name travels in the element; the file's own /NM (if any) is not what it is called.
        d.set(b"NM".to_vec(), PdfString::text(r.name));
        // The parent the caller named wins over whatever the file's parent was exported as: that one
        // entry is swapped in the shared map for this call and put back, instead of copying the map.
        let swapped = d.get(b"IRT").and_then(Object::as_ref).zip(r.in_reply_to).map(|(irt, parent)| (irt, names.insert(irt, parent.to_string())));
        out.push(element_xfdf(doc, &d, r.page, &names, r.name, max_len));
        if let Some((irt, before)) = swapped {
            match before {
                Some(old) => names.insert(irt, old),
                None => names.remove(&irt),
            };
        }
    }
    out
}

/// Remove the annotations at the given (page, index) positions with their pop-ups. Replies are
/// not removed unless they are listed. Nothing is changed when a position doesn't exist.
pub fn strip_positions(doc: &mut Document, targets: &[(usize, usize)]) -> Result<usize, DataError> {
    let pages = pdfcraft_model::pages(doc);
    let mut by_page: HashMap<usize, Vec<usize>> = HashMap::new();
    // Each page's /Annots length is read once, not once per target.
    let mut lens: HashMap<usize, usize> = HashMap::new();
    for &(page, index) in targets {
        let Some(p) = pages.get(page) else { return Err(DataError::NotFound(format!("page {}", page + 1))) };
        let len = *lens.entry(page).or_insert_with(|| annots_of(doc, p.obj).len());
        if index >= len {
            return Err(DataError::NotFound(format!("page {} index {index}", page + 1)));
        }
        by_page.entry(page).or_default().push(index);
    }
    let mut removed = 0;
    for (page, indexes) in by_page {
        let page_ref = pages[page].obj;
        let list = annots_of(doc, page_ref);
        let wanted: HashSet<usize> = indexes.iter().copied().collect();
        let mut doomed: HashSet<ObjRef> = HashSet::new();
        for &i in &indexes {
            if let Some(r) = list[i].as_ref() {
                doomed.insert(r);
                if let Some(pop) = doc.get(r).as_dict().and_then(|d| d.reference(b"Popup")) {
                    doomed.insert(pop);
                }
            }
        }
        // A pop-up that points at a removed parent goes too, even when the parent doesn't point back.
        for e in &list {
            if let Some(r) = e.as_ref()
                && doc
                    .get(r)
                    .as_dict()
                    .is_some_and(|d| d.name(b"Subtype") == Some(b"Popup") && d.reference(b"Parent").is_some_and(|p| doomed.contains(&p)))
            {
                doomed.insert(r);
            }
        }
        let kept: Vec<Object> = list
            .iter()
            .enumerate()
            .filter(|(i, e)| !wanted.contains(i) && !e.as_ref().is_some_and(|r| doomed.contains(&r)))
            .map(|(_, e)| e.clone())
            .collect();
        removed += list.len() - kept.len();
        doc.update_dict(page_ref, |d| d.set(b"Annots".to_vec(), Object::Array(kept))).map_err(DataError::Cos)?;
    }
    Ok(removed)
}
