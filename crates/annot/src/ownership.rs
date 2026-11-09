//! Who owns a comment (the embedding page's `actor`), kept in the private key `/PpmOwner`.
//!
//! The rules (docs/alternate/ownership-contract.md of the PPM host):
//! - Every annotation except Popup, Widget and Link has an owner. A pop-up follows its parent.
//! - Only the owner changes or deletes a comment. Others reply to it. A comment without a
//!   `/PpmOwner` (an original PDF's, or one saved before owners existed) is nobody's but locked.
//! - A comment with replies from someone else can't be deleted.
//! - Page-level edits that would drop or move someone else's comments are refused.
//!
//! Checks here read the document; they never change it. [`claim_new`] is the one writer.

use std::collections::{HashMap, HashSet};

use super::*;

/// The private key holding the owner's user id (a PDF text string).
pub const OWNER_KEY: &str = "PpmOwner";

/// Annotations that have an owner: everything but pop-ups (which follow their parent), form
/// widgets and links (not comments the user writes).
pub fn is_protected(d: &Dict) -> bool {
    !matches!(d.name(b"Subtype"), Some(b"Popup" | b"Widget" | b"Link"))
}

/// The user id in `/PpmOwner`, if there is one.
pub fn owner_of(doc: &Document, d: &Dict) -> Option<String> {
    let o = doc.resolve(d.get(OWNER_KEY.as_bytes())?);
    let s = o.as_string()?.to_text();
    let s = s.trim_matches('\0');
    (!s.is_empty()).then(|| s.to_string())
}

fn entry_dict(doc: &Document, entry: &Object) -> Option<Dict> {
    doc.resolve(entry).as_dict().cloned()
}

/// The dictionary whose owner decides `d`: itself, or for a pop-up its parent. `None`: nobody
/// owns this kind of annotation (a link, a widget).
fn deciding(doc: &Document, d: &Dict) -> Option<Dict> {
    if d.name(b"Subtype") == Some(b"Popup") {
        // A pop-up without a readable parent has no owner to speak for it.
        return Some(d.reference(b"Parent").and_then(|p| doc.get(p).as_dict().cloned()).unwrap_or_default());
    }
    is_protected(d).then(|| d.clone())
}

fn owned_by(doc: &Document, d: &Dict, actor: &str) -> bool {
    owner_of(doc, d).is_some_and(|o| o == actor)
}

/// Is the annotation at `(page, index)` a link or a form widget? Those belong to the document's
/// own layout, not to any commenter. Positions that don't exist are not.
pub fn is_layout_annotation(doc: &Document, page: usize, index: usize) -> bool {
    let Ok(p) = page_ref(doc, page) else { return false };
    annots(doc, p).get(index).and_then(|e| entry_dict(doc, e)).is_some_and(|d| matches!(d.name(b"Subtype"), Some(b"Link" | b"Widget")))
}

/// May `actor` change the comment at `(page, index)`? Not when someone else (or nobody) owns it.
/// Positions that don't exist pass: the edit itself reports them.
pub fn check_target(doc: &Document, page: usize, index: usize, actor: &str) -> Result<(), AnnotError> {
    let p = page_ref(doc, page)?;
    let Some(d) = annots(doc, p).get(index).and_then(|e| entry_dict(doc, e)) else { return Ok(()) };
    match deciding(doc, &d) {
        Some(owner) if !owned_by(doc, &owner, actor) => Err(AnnotError::NotOwner),
        _ => Ok(()),
    }
}

/// May `actor` delete the comment at `(page, index)`? The owner's own, and only when everything
/// that goes with it (replies, theirs, pop-ups) is the owner's too.
pub fn check_delete(doc: &Document, page: usize, index: usize, actor: &str) -> Result<(), AnnotError> {
    check_target(doc, page, index, actor)?;
    let p = page_ref(doc, page)?;
    let list = annots(doc, p);
    let Some(target) = list.get(index).and_then(Object::as_ref) else { return Ok(()) };
    for r in doomed_with(doc, &list, Some(target)) {
        if r == target {
            continue;
        }
        let Some(d) = doc.get(r).as_dict().cloned() else { continue };
        if is_protected(&d) && !owned_by(doc, &d, actor) {
            return Err(AnnotError::ForeignReplies);
        }
    }
    Ok(())
}

/// Do the pages (0-based; unknown ones are skipped) hold a comment `actor` doesn't own?
pub fn check_pages(doc: &Document, pages: &[usize], actor: &str) -> Result<(), AnnotError> {
    let refs = page_refs(doc)?;
    for &i in pages {
        let Some(&p) = refs.get(i) else { continue };
        let foreign = annots(doc, p).iter().filter_map(|e| entry_dict(doc, e)).any(|d| is_protected(&d) && !owned_by(doc, &d, actor));
        if foreign {
            return Err(AnnotError::ForeignPage);
        }
    }
    Ok(())
}

/// Do the pages (0-based; unknown ones are skipped) hold a sealed comment, a signature? Deleting,
/// moving or flattening such a page would lose or alter the signature, whoever asks.
pub fn check_pages_unsigned(doc: &Document, pages: &[usize]) -> Result<(), AnnotError> {
    let refs = page_refs(doc)?;
    for &i in pages {
        let Some(&p) = refs.get(i) else { continue };
        if annots(doc, p).iter().filter_map(|e| entry_dict(doc, e)).any(|d| is_sealed(doc, &d)) {
            return Err(AnnotError::SignedPage);
        }
    }
    Ok(())
}

/// [`check_pages_unsigned`] for every page.
pub fn check_all_pages_unsigned(doc: &Document) -> Result<(), AnnotError> {
    let n = page_refs(doc)?.len();
    check_pages_unsigned(doc, &(0..n).collect::<Vec<_>>())
}

/// Every page.
pub fn check_all_pages(doc: &Document, actor: &str) -> Result<(), AnnotError> {
    let n = page_refs(doc)?.len();
    check_pages(doc, &(0..n).collect::<Vec<_>>(), actor)
}

/// One protected annotation as the document holds it.
struct Seen {
    page: usize,
    obj: Option<ObjRef>,
    dict: Dict,
}

fn scan(doc: &Document) -> Vec<Seen> {
    let Ok(pages) = page_refs(doc) else { return Vec::new() };
    let mut out = Vec::new();
    for (page, p) in pages.iter().enumerate() {
        for entry in annots(doc, *p) {
            let Some(dict) = entry_dict(doc, &entry) else { continue };
            if is_protected(&dict) {
                out.push(Seen { page, obj: entry.as_ref(), dict });
            }
        }
    }
    out
}

/// Most objects one comment may reach, and how deep: past either the comment counts as changed,
/// so the edit is refused (a file built to be walked forever must not freeze everyone's editing).
const MAX_OBJECTS: usize = 20_000;
const MAX_DEPTH: usize = 100;

/// A hash of a comment as the file holds it, everything it points to included, or `None` when
/// it reaches too much. Any change to the comment's object graph (`/AP` streams, fonts, flags,
/// dates) changes the hash; `/P`, the page it sits on, is left out (the page number stands for
/// it) and `/IRT` and `/Parent` stay plain references (followed, a reply would change whenever
/// its parent is edited, which the parent's owner may do). An object met again is hashed as the
/// number of its first visit, not walked again, so shared objects cost nothing.
type Essence = Option<(u64, u64)>;

/// Two comments are the same only when both hashed.
fn same(a: &Essence, b: &Essence) -> bool {
    matches!((a, b), (Some(x), Some(y)) if x == y)
}

fn essence(doc: &Document, d: &Dict) -> Essence {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    struct Walk<'a> {
        doc: &'a Document,
        seen: HashMap<ObjRef, usize>,
        // Two independent lanes make a 128-bit digest.
        h: (DefaultHasher, DefaultHasher),
    }
    impl Walk<'_> {
        fn put<T: Hash>(&mut self, v: T) {
            v.hash(&mut self.h.0);
            0xA5u8.hash(&mut self.h.1);
            v.hash(&mut self.h.1);
        }
        fn object(&mut self, o: &Object, depth: usize) -> Option<()> {
            if depth > MAX_DEPTH {
                return None;
            }
            match o {
                Object::Null => self.put(0u8),
                Object::Bool(b) => self.put((1u8, *b)),
                Object::Int(i) => self.put((2u8, *i)),
                Object::Real(r) => self.put((3u8, r.to_bits())),
                Object::String(s) => self.put((4u8, &s.bytes)),
                Object::Name(n) => self.put((5u8, n)),
                Object::Array(items) => {
                    self.put((6u8, items.len()));
                    for i in items {
                        self.object(i, depth + 1)?;
                    }
                }
                Object::Dict(d) => self.dict(d, depth)?,
                Object::Stream(st) => {
                    self.put(7u8);
                    self.dict(&st.dict, depth)?;
                    self.put(&st.raw[..]);
                }
                Object::Ref(r) => {
                    if let Some(n) = self.seen.get(r) {
                        self.put((8u8, *n));
                    } else {
                        if self.seen.len() >= MAX_OBJECTS {
                            return None;
                        }
                        self.seen.insert(*r, self.seen.len());
                        self.put(9u8);
                        let target = self.doc.get(*r);
                        self.object(&target, depth + 1)?;
                    }
                }
            }
            Some(())
        }
        fn dict(&mut self, d: &Dict, depth: usize) -> Option<()> {
            // In key order, so the digest doesn't depend on how the file lists them.
            let mut entries: Vec<_> = d.iter().filter(|(k, _)| k.as_slice() != b"P").collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            self.put((10u8, entries.len()));
            for (k, v) in entries {
                self.put(k);
                if matches!(k.as_slice(), b"IRT" | b"Parent") {
                    self.put(v.as_ref().map(|r| (r.num, r.generation)));
                } else {
                    self.object(v, depth + 1)?;
                }
            }
            Some(())
        }
    }

    let mut w = Walk { doc, seen: HashMap::new(), h: (DefaultHasher::new(), DefaultHasher::new()) };
    w.dict(d, 0)?;
    Some((w.h.0.finish(), w.h.1.finish()))
}

/// The comments `actor` doesn't own, as they were: what an edit must leave as it was.
pub struct Foreign {
    by_ref: HashMap<ObjRef, (usize, Essence)>,
    inline: Vec<(usize, Essence)>,
    /// Page geometry (`/MediaBox`, `/CropBox`, `/Rotate`) of the pages holding such comments:
    /// the server checks it too, since a comment's place on the page depends on it.
    pages: HashMap<usize, Vec<Option<Object>>>,
}

fn page_geometry(doc: &Document, page: usize) -> Vec<Option<Object>> {
    let first = page_refs(doc).ok().and_then(|p| p.get(page).copied());
    // The value in effect: set on the page or inherited from the page tree.
    let inherited = |key: &[u8]| {
        let mut at = first;
        for _ in 0..32 {
            let dict = doc.get(at?).as_dict().cloned()?;
            if let Some(o) = dict.get(key) {
                return Some((*doc.resolve(o)).clone());
            }
            at = dict.reference(b"Parent");
        }
        None
    };
    [&b"MediaBox"[..], b"CropBox", b"Rotate"].iter().map(|k| inherited(k)).collect()
}

pub fn foreign_snapshot(doc: &Document, actor: &str) -> Foreign {
    let mut f = Foreign { by_ref: HashMap::new(), inline: Vec::new(), pages: HashMap::new() };
    for s in scan(doc).into_iter().filter(|s| !owned_by(doc, &s.dict, actor)) {
        let e = essence(doc, &s.dict);
        f.pages.entry(s.page).or_insert_with(|| page_geometry(doc, s.page));
        match s.obj {
            Some(r) => {
                f.by_ref.insert(r, (s.page, e));
            }
            None => f.inline.push((s.page, e)),
        }
    }
    f
}

/// Every comment in `before` is still there, on the same page, with the same essential fields:
/// the answer for any edit that could drop, move or rewrite comments in bulk. It is as strict as
/// the server's save check on purpose: an edit the server would refuse later must fail now.
pub fn check_foreign_kept(doc: &Document, before: &Foreign) -> Result<(), AnnotError> {
    if before.pages.is_empty() {
        return Ok(());
    }
    if before.pages.iter().any(|(page, geometry)| page_geometry(doc, *page) != *geometry) {
        return Err(AnnotError::ForeignPage);
    }
    let now = scan(doc);
    let by_ref: HashMap<ObjRef, (usize, Essence)> = now.iter().filter_map(|s| s.obj.map(|r| (r, (s.page, essence(doc, &s.dict))))).collect();
    let mut inline: Vec<(usize, Essence)> = now.iter().filter(|s| s.obj.is_none()).map(|s| (s.page, essence(doc, &s.dict))).collect();
    for (r, was) in &before.by_ref {
        // A comment edited into an inline copy (or promoted to a reference) counts as changed.
        if !by_ref.get(r).is_some_and(|now| now.0 == was.0 && same(&now.1, &was.1)) {
            return Err(AnnotError::ForeignPage);
        }
    }
    for was in &before.inline {
        let Some(at) = inline.iter().position(|n| n.0 == was.0 && same(&n.1, &was.1)) else { return Err(AnnotError::ForeignPage) };
        inline.swap_remove(at);
    }
    Ok(())
}

impl Foreign {
    /// Leave out the comment at `(page, index)`: an edit that is meant to take it away (signing in
    /// a sign box another user placed) must not be refused for it.
    pub fn without(mut self, doc: &Document, page: usize, index: usize) -> Self {
        let Ok(p) = page_ref(doc, page) else { return self };
        let Some(entry) = annots(doc, p).into_iter().nth(index) else { return self };
        match entry.as_ref() {
            Some(r) => {
                self.by_ref.remove(&r);
            }
            None => {
                if let Some(dict) = entry_dict(doc, &entry) {
                    let e = essence(doc, &dict);
                    if let Some(at) = self.inline.iter().position(|(pg, was)| *pg == page && same(was, &e)) {
                        self.inline.swap_remove(at);
                    }
                }
            }
        }
        self
    }
}

/// What existed before an edit that creates comments.
pub struct Known {
    pages: HashSet<ObjRef>,
    refs: HashSet<ObjRef>,
    /// Comments of the actor's that were there: an import can overwrite one in place.
    own: HashSet<ObjRef>,
    /// Entries that were inline: (page, index in `/Annots`, dictionary).
    inline: Vec<(ObjRef, usize, Dict)>,
    names: HashSet<String>,
}

pub fn known(doc: &Document, actor: &str) -> Known {
    let mut k = Known { pages: HashSet::new(), refs: HashSet::new(), own: HashSet::new(), inline: Vec::new(), names: HashSet::new() };
    let Ok(pages) = page_refs(doc) else { return k };
    for p in pages {
        k.pages.insert(p);
        for (index, entry) in annots(doc, p).into_iter().enumerate() {
            match (entry.as_ref(), entry_dict(doc, &entry)) {
                (Some(r), d) => {
                    k.refs.insert(r);
                    if d.is_some_and(|d| is_protected(&d) && owned_by(doc, &d, actor)) {
                        k.own.insert(r);
                    }
                }
                (None, Some(d)) => k.inline.push((p, index, d)),
                (None, None) => {}
            }
            if let Some(name) = entry_dict(doc, &entry).and_then(|d| name_of(doc, &d)) {
                k.names.insert(name);
            }
        }
    }
    k
}

fn name_of(doc: &Document, d: &Dict) -> Option<String> {
    let o = doc.resolve(d.get(b"NM")?);
    let s = o.as_string()?.to_text();
    (!s.is_empty()).then_some(s)
}

/// Mark every comment the edit created as `actor`'s, and give each a `/NM` of its own (a copied
/// page's comments would repeat their original's). New are the entries that weren't there
/// before, and all of a page that wasn't. Returns how many were claimed.
pub fn claim_new(doc: &mut Document, before: &Known, actor: &str, fresh_name: &mut dyn FnMut() -> String) -> Result<usize, AnnotError> {
    let mut names = before.names.clone();
    // Inline entries from before, used up as they are matched (editing one may promote it to a
    // reference, which must not make it look new).
    let mut old_inline = before.inline.clone();
    let mut claimed = 0;
    for (page, p) in page_refs(doc)?.into_iter().enumerate() {
        let new_page = !before.pages.contains(&p);
        for (index, entry) in annots(doc, p).into_iter().enumerate() {
            let Some(d) = entry_dict(doc, &entry) else { continue };
            // The actor's own comment overwritten in place (an XFDF/FDF import with the same /NM)
            // stays theirs, whatever owner the file said.
            if let Some(r) = entry.as_ref().filter(|r| before.own.contains(r)) {
                if is_protected(&d) && !owned_by(doc, &d, actor) {
                    doc.update_dict(r, |d| d.set(OWNER_KEY.as_bytes().to_vec(), PdfString::text(actor)))?;
                }
                continue;
            }
            let mut is_new = new_page || entry.as_ref().is_none_or(|r| !before.refs.contains(&r));
            // Best effort: an old inline entry that an edit turned into a reference, or left in
            // place, is matched by equal dictionary or by position and subtype.
            if is_new && !new_page {
                let same =
                    |(pg, i, old): &(ObjRef, usize, Dict)| *pg == p && (*old == d || (*i == index && old.name(b"Subtype") == d.name(b"Subtype")));
                if let Some(at) = old_inline.iter().position(same) {
                    old_inline.swap_remove(at);
                    is_new = false;
                }
            }
            // A pop-up has no owner of its own, so nothing for it here.
            if !is_new || !is_protected(&d) {
                continue;
            }
            let (_, r) = annot_ref(doc, page, index)?;
            let name = name_of(doc, &d).filter(|n| !names.contains(n)).unwrap_or_else(&mut *fresh_name);
            names.insert(name.clone());
            doc.update_dict(r, |d| {
                d.set(OWNER_KEY.as_bytes().to_vec(), PdfString::text(actor));
                d.set(b"NM".to_vec(), PdfString::text(&name));
            })?;
            claimed += 1;
        }
    }
    Ok(claimed)
}
