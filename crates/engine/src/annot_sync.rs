//! Annotation sharing with the embedding page (`pdfcraft:open` `annotSync`), docs/alternate/annot-db-contract.md.
//!
//! In this mode the annotations live in the host's database, one row per `/NM`, and the PDF is
//! only the canvas:
//!
//! - **Out.** After every edit, undo and redo the engine compares the actor's own annotations
//!   with what it last handed out (a `/NM` to fingerprint cache) and keeps the difference until
//!   the interface takes it ([`Session::take_annot_changes`]). Doing this in the engine covers
//!   every edit that makes annotations in bulk (stamps, paste, XFDF import, a `Batch`) without
//!   the interface knowing about them.
//! - **In.** The server's annotations are applied outside the edit pipeline
//!   ([`Session::annots_reset`], [`Session::annots_apply`], [`Session::annots_reject`]): no undo
//!   step, no dirty flag, no owner claim, `/PpmOwner` is the server's author. The same change is
//!   written into every stored undo and redo state, so undoing never removes or brings back an
//!   annotation of someone else.
//!
//! Each undo state is a whole document of its own, so a patched-in annotation can have a
//! different object number in each of them. Nothing here compares object numbers: `/IRT` is
//! read as the target's `/NM`, and the fingerprints skip `/P`, `/Parent` and `/Popup`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use pdfcraft_annot::AnnotError;
use pdfcraft_annot::ownership::{is_protected, owner_of};
use pdfcraft_cos::{Dict, ObjRef, Object};

use crate::{DocId, Document, Edit, EditError, Scope, Session, render_threads};
use pdfcraft_render::RenderPool;

type Cos = pdfcraft_cos::Document;

/// One of the actor's annotations that is new or different since the last [`AnnotChanges`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnotUpsert {
    /// The annotation's `/NM`.
    pub annot_id: String,
    /// 0-based page (the interface sends it to the page 1-based, as the server counts).
    pub page: usize,
    /// The annotation's type as the database names it: its `/Subj`, else the XFDF tag ("Highlight",
    /// "Stamp", ...), at most 100 UTF-16 units.
    pub subtype: String,
    /// The annotation as one XFDF `<annots>` entry (`name` = `/NM`, XFDF `page` attribute = this page, 0-based).
    pub xfdf: String,
}

/// What the user's edits did to the actor's annotations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnnotChanges {
    pub upserts: Vec<AnnotUpsert>,
    pub deletes: Vec<String>,
}

impl AnnotChanges {
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.deletes.is_empty()
    }
}

/// An annotation as the server holds it (`pdfcraft:annots-reset` / `-apply` item).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteAnnot {
    pub annot_id: String,
    pub author_id: String,
    pub xfdf: String,
}

/// A change of the actor's that the server refused (`pdfcraft:annots-reject` item): `server` is
/// the annotation's state there, `None` when the server has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedAnnot {
    pub annot_id: String,
    pub server: Option<RemoteAnnot>,
}

/// How a server message treats the actor's own annotations.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// The server's whole list, the truth for everything but what the actor changed after the page
    /// fetched it.
    Reset,
    /// A push from the socket: another user's edit of the actor's own annotation is put in, unless
    /// the actor has a change of it not yet handed out.
    Apply,
    /// The server refused a change: the local copy is put back as the server has it.
    Reject,
}

/// What a server message comes to: items to put in, `/NM`s to remove everywhere, the actor's own
/// to remove from the working document and the states that hold the same version, and (reset) the `/NM`s it must not touch.
struct Plan {
    upserts: Vec<RemoteAnnot>,
    deletes: Vec<String>,
    mine_deletes: Vec<String>,
    mode: Mode,
    guard: HashSet<String>,
}

/// One message from the server, for [`Session::annots_run`].
pub enum AnnotOp {
    /// The server's whole list; `seq` is the last change message of the actor's the page had
    /// received (see [`Session::record_annot_sent`]) when it fetched the list.
    Reset {
        items: Vec<RemoteAnnot>,
        seq: u64,
    },
    Apply {
        upserts: Vec<RemoteAnnot>,
        deletes: Vec<String>,
    },
    Reject(Vec<RejectedAnnot>),
}

/// What a remote application did: how many items were applied or skipped as already current, and
/// the ones that could not be applied (annot id, reason), and the pages the change touched (the
/// page an annotation was on and the one it is on now), so a view draws only those again.
/// `pages_unknown` is set when an applied item's page could not be found: every page is then touched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteReport {
    pub applied: usize,
    pub skipped: usize,
    pub failed: Vec<(String, String)>,
    pub pages: std::collections::BTreeSet<usize>,
    pub pages_unknown: bool,
}

/// The XFDF functions this module needs; the one place that names the xfdf crate's API.
mod xf {
    use super::{Cos, RemoteAnnot};

    pub struct Exported {
        pub page: usize,
        pub subtype: String,
        pub xfdf: String,
    }

    /// One scan of the document, for exporting many of its annotations.
    pub fn index(doc: &Cos) -> pdfcraft_xfdf::AnnotIndex {
        pdfcraft_xfdf::AnnotIndex::new(doc)
    }

    /// The annotation named `nm` as XFDF (the xfdf crate cuts `subtype` to the host's 100 units).
    pub fn export(doc: &Cos, idx: &pdfcraft_xfdf::AnnotIndex, nm: &str) -> Option<Exported> {
        pdfcraft_xfdf::export_annotation_in(doc, idx, nm).ok().map(|e| Exported { page: e.page, subtype: e.kind, xfdf: e.xfdf })
    }

    /// Put the annotations in (replacing the ones with the same `/NM`, `/PpmOwner` the item's
    /// author) after removing `deletes`. Nothing here checks ownership. `pictures` is shared by the
    /// calls for one batch so a stamp's picture is decoded once.
    pub fn apply(
        doc: &mut Cos,
        upserts: &[&RemoteAnnot],
        deletes: &[String],
        pictures: &mut pdfcraft_xfdf::PictureCache,
    ) -> pdfcraft_xfdf::RemoteReport {
        let items: Vec<pdfcraft_xfdf::RemoteItem> =
            upserts.iter().map(|a| pdfcraft_xfdf::RemoteItem { annot_id: &a.annot_id, author_id: &a.author_id, xfdf: &a.xfdf }).collect();
        pdfcraft_xfdf::apply_remote_cached(doc, &items, deletes, pictures)
    }
}

/// Where an annotation that could not be written out stood: its allocation and fingerprint.
type FailedAt = (Option<Arc<Object>>, Option<(u64, u64)>);

/// An annotation of the actor's as last handed out.
#[derive(Clone)]
struct Entry {
    page: usize,
    /// Fingerprint of the annotation's object graph; `None` when it reaches too much to hash.
    fp: Option<(u64, u64)>,
    /// Hash of the XFDF that was handed out (or applied from the server).
    xfdf: u64,
    /// The annotation's dictionary as last seen: the same allocation means nothing changed, so
    /// an edit elsewhere costs this annotation no hashing.
    arc: Option<Arc<Object>>,
    /// Hash of (author, XFDF) as the server last sent this annotation, `0` when it never did. The
    /// same text again is not news.
    server: u64,
    /// The server has it (handed out, or applied from the server). An annotation that was in the
    /// file when it opened is not synced: it has never been handed out.
    synced: bool,
}

/// The sync state of one `annotSync` document.
#[derive(Clone, Default)]
pub(crate) struct SyncState {
    /// [`Document::generation`] the cache is current for.
    seen: u64,
    /// The actor's annotations by `/NM`.
    mine: HashMap<String, Entry>,
    /// Annotations of others applied from the server: `/NM` to a hash of (author, XFDF).
    remote: HashMap<String, u64>,
    /// Changes not yet taken by the interface; the flag says the annotation is a reply (`/IRT`),
    /// which goes out after the annotation it replies to.
    upserts: BTreeMap<String, (AnnotUpsert, bool)>,
    deletes: BTreeSet<String>,
    /// Annotations that could not be written out, as last seen (so they are not tried again until
    /// they change): the dictionary allocation and its fingerprint.
    failed: HashMap<String, FailedAt>,
    /// Annotations that just failed to be written out, for the interface to tell the user.
    problems: Vec<String>,
    /// The `/NM`s of each change message handed to the page, by its number, until a reset shows the
    /// page has seen them.
    sent: Vec<(u64, Vec<String>)>,
    /// The `/NM`s of sign boxes a signature just replaced; if another user placed one, the server
    /// is told it is gone (the only deletion of someone else's annotation the actor's edit sends).
    signed_away: BTreeSet<String>,
}

impl SyncState {
    fn queue_upsert(&mut self, u: AnnotUpsert, reply: bool) {
        self.deletes.remove(&u.annot_id);
        self.upserts.insert(u.annot_id.clone(), (u, reply));
    }

    pub(crate) fn note_signed_away(&mut self, names: Vec<String>) {
        self.signed_away.extend(names);
    }

    fn queue_delete(&mut self, nm: String) {
        self.upserts.remove(&nm);
        self.deletes.insert(nm);
    }

    /// Forget everything pending or cached about `nm`.
    fn forget(&mut self, nm: &str) {
        self.mine.remove(nm);
        self.remote.remove(nm);
        self.upserts.remove(nm);
        self.deletes.remove(nm);
        self.failed.remove(nm);
    }
}

fn hash_of<T: Hash + ?Sized>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

// ── reading annotations ─────────────────────────────────────────────────────────────────────

/// Call `f(page, index, object, dictionary allocation, dictionary)` for every annotation.
fn walk(cos: &Cos, mut f: impl FnMut(usize, usize, Option<ObjRef>, Option<&Arc<Object>>, &Dict)) {
    let Ok(pages) = pdfcraft_annot::page_refs(cos) else { return };
    for (page, p) in pages.iter().enumerate() {
        let page_obj = cos.get(*p);
        let Some(list) = page_obj.as_dict().and_then(|d| d.get(b"Annots")).map(|a| cos.resolve(a)) else { continue };
        let Some(list) = list.as_array() else { continue };
        for (index, entry) in list.iter().enumerate() {
            match entry.as_ref() {
                Some(r) => {
                    let arc = cos.get(r);
                    if let Some(d) = arc.as_dict() {
                        f(page, index, Some(r), Some(&arc), d);
                    }
                }
                None => {
                    if let Some(d) = entry.as_dict() {
                        f(page, index, None, None, d);
                    }
                }
            }
        }
    }
}

/// The page each of `names` is on.
fn pages_of(cos: &Cos, names: &HashSet<&str>) -> HashMap<String, usize> {
    let mut found = HashMap::new();
    walk(cos, |page, _, _, _, d| {
        if let Some(nm) = nm_of(cos, d).filter(|n| names.contains(n.as_str())) {
            found.entry(nm).or_insert(page);
        }
    });
    found
}

fn nm_of(cos: &Cos, d: &Dict) -> Option<String> {
    let o = cos.resolve(d.get(b"NM")?);
    let s = o.as_string()?.to_text();
    (!s.is_empty()).then_some(s)
}

/// The `/NM` of the annotation `d` replies to.
fn parent_nm(cos: &Cos, d: &Dict) -> Option<String> {
    let target = cos.get(d.reference(b"IRT")?);
    nm_of(cos, target.as_dict()?)
}

fn is_mine(cos: &Cos, d: &Dict, actor: &str) -> bool {
    is_protected(d) && owner_of(cos, d).as_deref() == Some(actor)
}

/// A check mark (`/StateModel /Marked`): private bookkeeping of this interface, not a comment to share.
fn is_check_mark(cos: &Cos, d: &Dict) -> bool {
    d.get(b"StateModel").map(|o| cos.resolve(o)).and_then(|o| o.as_string().map(|s| s.to_text())).as_deref() == Some("Marked")
}

/// The annotation dictionary at `(page, index)`, read straight from the page's `/Annots`.
fn dict_at(cos: &Cos, page: usize, index: usize) -> Option<Dict> {
    let pages = pdfcraft_annot::page_refs(cos).ok()?;
    let page_obj = cos.get(*pages.get(page)?);
    let list = cos.resolve(page_obj.as_dict()?.get(b"Annots")?);
    let entry = list.as_array()?.get(index)?;
    match entry.as_ref() {
        Some(r) => cos.get(r).as_dict().cloned(),
        None => entry.as_dict().cloned(),
    }
}

/// Most objects an annotation may reach before it is no longer fingerprinted.
const MAX_OBJECTS: usize = 20_000;
const MAX_DEPTH: usize = 100;

/// A fingerprint of an annotation and everything it points to, or `None` when it reaches too
/// much. Object numbers do not enter: references are numbered by first visit, `/IRT` is the
/// target's `/NM`, and `/P`, `/Parent` and `/Popup` (which lead back out to other annotations
/// and the page) are left out.
fn fingerprint(cos: &Cos, d: &Dict) -> Option<(u64, u64)> {
    struct Walk<'a> {
        cos: &'a Cos,
        seen: HashMap<ObjRef, usize>,
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
                        let target = self.cos.get(*r);
                        self.object(&target, depth + 1)?;
                    }
                }
            }
            Some(())
        }
        fn dict(&mut self, d: &Dict, depth: usize) -> Option<()> {
            let mut entries: Vec<_> = d.iter().filter(|(k, _)| !matches!(k.as_slice(), b"P" | b"Parent" | b"Popup")).collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            self.put((10u8, entries.len()));
            for (k, v) in entries {
                self.put(k);
                if k.as_slice() == b"IRT" {
                    let target = v.as_ref().and_then(|r| self.cos.get(r).as_dict().and_then(|t| nm_of(self.cos, t)));
                    self.put((11u8, target));
                } else {
                    self.object(v, depth + 1)?;
                }
            }
            Some(())
        }
    }
    let mut w = Walk { cos, seen: HashMap::new(), h: (DefaultHasher::new(), DefaultHasher::new()) };
    w.dict(d, 0)?;
    Some((w.h.0.finish(), w.h.1.finish()))
}

// ── out: the actor's changes ────────────────────────────────────────────────────────────────

fn same_alloc(a: &Option<Arc<Object>>, b: Option<&Arc<Object>>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if Arc::ptr_eq(x, y))
}

/// Compare the actor's annotations in `cos` with the cache, queue what differs, and bring the
/// cache up to date.
fn diff(cos: &Cos, actor: &str, st: &mut SyncState) {
    let mut present: HashSet<String> = HashSet::new();
    let mut found: Vec<(String, usize, Option<Arc<Object>>, Dict)> = Vec::new();
    {
        let (mine, failed) = (&st.mine, &st.failed);
        walk(cos, |page, _, _, arc, d| {
            if !is_mine(cos, d, actor) || is_check_mark(cos, d) {
                return;
            }
            let Some(nm) = nm_of(cos, d) else { return };
            // A name used twice: the first is the one that counts.
            if !present.insert(nm.clone()) {
                return;
            }
            // The same allocation as last time: nothing changed, so no copy and no hashing.
            if mine.get(&nm).is_some_and(|p| p.page == page && same_alloc(&p.arc, arc)) || failed.get(&nm).is_some_and(|(a, _)| same_alloc(a, arc)) {
                return;
            }
            found.push((nm, page, arc.cloned(), d.clone()));
        });
    }
    // One scan of the document serves every export below; built only if something changed.
    let mut index = None;
    for (nm, page, arc, d) in found {
        let prev = st.mine.get(&nm);
        let fp = fingerprint(cos, &d);
        if let Some(p) = prev
            && p.page == page
            && fp.is_some()
            && p.fp == fp
        {
            if let Some(e) = st.mine.get_mut(&nm) {
                e.arc = arc;
            }
            continue;
        }
        if fp.is_some() && st.failed.get(&nm).is_some_and(|(_, f)| *f == fp) {
            st.failed.insert(nm, (arc, fp));
            continue;
        }
        // An annotation that cannot be written out stays as it was on the server, and the user
        // hears of it once; it is tried again when it changes.
        let Some(x) = xf::export(cos, index.get_or_insert_with(|| xf::index(cos)), &nm) else {
            if !st.failed.contains_key(&nm) {
                st.problems.push(nm.clone());
            }
            st.failed.insert(nm, (arc, fp));
            continue;
        };
        st.failed.remove(&nm);
        let xfdf = hash_of(&x.xfdf);
        let unchanged = prev.is_some_and(|p| p.xfdf == xfdf && p.page == x.page);
        let (synced, server) = prev.map_or((false, 0), |p| (p.synced, p.server));
        st.mine.insert(nm.clone(), Entry { page: x.page, fp, xfdf, server, arc, synced });
        if !unchanged {
            let reply = d.get(b"IRT").is_some();
            st.queue_upsert(AnnotUpsert { annot_id: nm, page: x.page, subtype: x.subtype, xfdf: x.xfdf }, reply);
        }
    }
    let gone: Vec<String> = st.mine.keys().filter(|k| !present.contains(*k)).cloned().collect();
    for nm in gone {
        let was_synced = st.mine.remove(&nm).is_some_and(|e| e.synced);
        // Never handed out: the server knows nothing of it.
        if was_synced {
            st.queue_delete(nm);
        } else {
            st.upserts.remove(&nm);
        }
    }
    // A sign box of another user that a signature replaced: the server is told, as for the
    // actor's own. Nothing else the actor's edit takes from someone else is sent.
    for nm in std::mem::take(&mut st.signed_away) {
        if !present.contains(&nm) && st.remote.remove(&nm).is_some() {
            st.queue_delete(nm);
        }
    }
    st.failed.retain(|k, _| present.contains(k));
}

/// Bring the cache up to date with the document's current state.
fn sync_now(doc: &mut Document) {
    let (Some(st), Some(actor), Some(editor)) = (doc.sync.as_mut(), doc.ownership.actor.as_deref(), doc.editor.as_ref()) else { return };
    if st.seen == doc.generation {
        return;
    }
    diff(&editor.cos, actor, st);
    st.seen = doc.generation;
}

impl Document {
    /// Start tracking the actor's annotations from the document as it is (none of them are
    /// handed out), or stop. Called when the ownership of a document is set.
    pub(crate) fn init_annot_sync(&mut self) {
        self.start_or_stop_annot_sync();
        // A document shared with the host draws no replies on the page, as the host's own viewer.
        self.set_replies_hidden(self.sync.is_some());
    }

    /// Draw the document's replies on the page, or leave them out (they stay in the file and the
    /// comments list). Draws the pages again when it changes.
    pub(crate) fn set_replies_hidden(&mut self, hidden: bool) {
        if self.config.hide_replies != hidden {
            self.config.hide_replies = hidden;
            self.renderer = RenderPool::new(self.display.clone(), render_threads(), self.config.clone());
            self.generation += 1;
            // Redrawing is not an edit: nothing to scan for.
            if let Some(st) = self.sync.as_mut() {
                st.seen = self.generation;
            }
        }
    }

    /// Are replies left out of the page picture?
    pub fn replies_hidden(&self) -> bool {
        self.config.hide_replies
    }

    fn start_or_stop_annot_sync(&mut self) {
        let on = self.ownership.annot_sync && self.ownership.actor.is_some() && self.editor.is_some();
        if !on {
            self.sync = None;
            return;
        }
        if self.sync.is_some() {
            return;
        }
        let mut st = SyncState { seen: self.generation, ..Default::default() };
        if let (Some(actor), Some(editor)) = (self.ownership.actor.as_deref(), self.editor.as_ref()) {
            diff(&editor.cos, actor, &mut st);
        }
        // What was in the file is not news.
        st.upserts.clear();
        st.deletes.clear();
        for e in st.mine.values_mut() {
            e.synced = false;
        }
        self.sync = Some(st);
    }

    /// Does this document exchange its annotations with the host?
    pub fn annot_sync(&self) -> bool {
        self.sync.is_some()
    }

    /// Page and index (in `/Annots`, both 0-based) of the annotation named `nm`.
    pub fn annot_position(&self, nm: &str) -> Option<(usize, usize)> {
        let editor = self.editor.as_ref()?;
        let mut at = None;
        walk(&editor.cos, |page, index, _, _, d| {
            if at.is_none() && nm_of(&editor.cos, d).as_deref() == Some(nm) {
                at = Some((page, index));
            }
        });
        at
    }

    /// The `/NM` of the annotation at `(page, index)`.
    pub fn annot_name_at(&self, page: usize, index: usize) -> Option<String> {
        let editor = self.editor.as_ref()?;
        nm_of(&editor.cos, &dict_at(&editor.cos, page, index)?)
    }

    /// The role, the assigned signer (`/PpmSigner`, if any) and the rectangle (user space,
    /// `[x0, y0, x1, y1]` with x0 <= x1 and y0 <= y1) of the sign box at `(page, index)`; `None` for
    /// any other annotation.
    pub fn sign_box_at(&self, page: usize, index: usize) -> Option<SignBox> {
        let editor = self.editor.as_ref()?;
        sign_box_of(&editor.cos, &dict_at(&editor.cos, page, index)?)
    }

    /// Does the annotation at `(page, index)` take no replies, statuses or marks: a sign box, a
    /// signature of any kind, or something in a thread under one (`pdfcraft_annot::takes_no_reply`)?
    /// The editor hides those commands.
    pub fn takes_no_reply(&self, page: usize, index: usize) -> bool {
        let Some(editor) = self.editor.as_ref() else { return false };
        pdfcraft_annot::takes_no_reply(&editor.cos, page, index) || sign_box_at_index(&editor.cos, page, index)
    }

    /// Is the annotation at `(page, index)` a reply to another annotation?
    pub fn annot_is_reply(&self, page: usize, index: usize) -> bool {
        self.editor.as_ref().is_some_and(|e| is_reply_at(&e.cos, page, index))
    }
}

/// A sign box's role, assigned signer and normalized rectangle.
pub type SignBox = (String, Option<String>, [f64; 4]);

/// A sign box is a FreeText annotation with a non-empty `/PpmSignRole`; its role, its `/PpmSigner`
/// (as stored, never trimmed or dropped, so a blank one can't turn the box back into a role box;
/// the bridge refuses an invalid one) and normalized `/Rect`.
pub(crate) fn sign_box_of(cos: &Cos, d: &Dict) -> Option<SignBox> {
    if d.name(b"Subtype") != Some(&b"FreeText"[..]) {
        return None;
    }
    let role = cos.resolve(d.get(pdfcraft_annot::SIGN_ROLE_KEY.as_bytes())?).as_string()?.to_text();
    let role = role.trim_matches('\0').trim().to_string();
    if role.is_empty() {
        return None;
    }
    let signer = d
        .get(pdfcraft_annot::SIGNER_KEY.as_bytes())
        .and_then(|o| cos.resolve(o).as_string().map(|s| s.to_text()));
    let rect = cos.resolve(d.get(b"Rect")?);
    let n: Vec<f64> = rect.as_array()?.iter().map(|o| cos.resolve(o).as_f64()).collect::<Option<_>>()?;
    if n.len() != 4 || n.iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some((role, signer, [n[0].min(n[2]), n[1].min(n[3]), n[0].max(n[2]), n[1].max(n[3])]))
}

/// The `/NM`s of the sign boxes that `edit` (or a batch's edits) signs in, read before it runs.
pub(crate) fn signed_in_names(cos: &Cos, edit: &Edit) -> Vec<String> {
    match edit {
        Edit::SignInBox { page, index, .. } => dict_at(cos, *page, *index).and_then(|d| nm_of(cos, &d)).into_iter().collect(),
        Edit::Batch { edits, .. } => edits.iter().flat_map(|e| signed_in_names(cos, e)).collect(),
        _ => Vec::new(),
    }
}

/// Is the annotation at `(page, index)` of `cos` a sign box?
pub(crate) fn sign_box_at_index(cos: &Cos, page: usize, index: usize) -> bool {
    dict_at(cos, page, index).is_some_and(|d| sign_box_of(cos, &d).is_some())
}

/// Is the annotation at `(page, index)` of `cos` a reply to another?
pub(crate) fn is_reply_at(cos: &Cos, page: usize, index: usize) -> bool {
    dict_at(cos, page, index).is_some_and(|d| d.get(b"IRT").is_some())
}

/// Replies to replies are refused (the server does): the edit says which annotation it replies
/// to or sets the state of.
pub(crate) fn check_reply_target(cos: &Cos, edit: &Edit) -> Result<(), EditError> {
    if let Edit::ReplyToAnnotation { page, index, .. } | Edit::SetAnnotationStatus { page, index, .. } | Edit::MarkAnnotation { page, index, .. } =
        edit
        && is_reply_at(cos, *page, *index)
    {
        return Err(EditError::Invalid("a reply can't be replied to".into()));
    }
    Ok(())
}

/// Undo or redo to `target` must not remove an annotation of the actor's that someone else has
/// replied to in the current state; the server would refuse the delete.
fn check_step(current: &Cos, target: &Cos, actor: &str) -> Result<(), AnnotError> {
    let mut parents: HashSet<String> = HashSet::new();
    let mut mine: Vec<String> = Vec::new();
    walk(current, |_, _, _, _, d| {
        if !is_protected(d) {
            return;
        }
        if owner_of(current, d).as_deref() == Some(actor) {
            mine.extend(nm_of(current, d));
        } else if let Some(p) = parent_nm(current, d) {
            parents.insert(p);
        }
    });
    mine.retain(|nm| parents.contains(nm));
    if mine.is_empty() {
        return Ok(());
    }
    let mut kept: HashSet<String> = HashSet::new();
    walk(target, |_, _, _, _, d| kept.extend(nm_of(target, d)));
    if mine.iter().any(|nm| !kept.contains(nm)) {
        return Err(AnnotError::ForeignReplies);
    }
    Ok(())
}

// ── in: the server's annotations ────────────────────────────────────────────────────────────

impl Session {
    fn sync_doc_mut(&mut self, id: DocId) -> Result<&mut Document, EditError> {
        let doc = self.doc_mut(id)?;
        if doc.sync.is_none() {
            return Err(EditError::Invalid("the page didn't ask for annotation sharing (annotSync) when it opened this document".into()));
        }
        Ok(doc)
    }

    /// The newest open document that exchanges annotations with the host.
    pub fn annot_sync_doc(&self) -> Option<DocId> {
        self.docs.iter().rev().find(|d| d.sync.is_some()).map(|d| d.id)
    }

    /// What the user's edits changed in the actor's annotations since this was last called.
    /// `None` when there is nothing, or the document doesn't share annotations.
    pub fn take_annot_changes(&mut self, id: DocId) -> Option<AnnotChanges> {
        let doc = self.docs.iter_mut().find(|d| d.id == id)?;
        sync_now(doc);
        let st = doc.sync.as_mut()?;
        if st.upserts.is_empty() && st.deletes.is_empty() {
            return None;
        }
        // Replies go after what they reply to, so a host that sends them in batches never sends a
        // reply ahead of its parent.
        let mut queued: Vec<(AnnotUpsert, bool)> = std::mem::take(&mut st.upserts).into_values().collect();
        queued.sort_by(|a, b| (a.1, a.0.page, &a.0.annot_id).cmp(&(b.1, b.0.page, &b.0.annot_id)));
        let changes =
            AnnotChanges { upserts: queued.into_iter().map(|(u, _)| u).collect(), deletes: std::mem::take(&mut st.deletes).into_iter().collect() };
        // Everything handed out is now known to the server.
        for u in &changes.upserts {
            if let Some(e) = st.mine.get_mut(&u.annot_id) {
                e.synced = true;
            }
        }
        Some(changes)
    }

    /// The interface sent a change message numbered `seq` carrying these `/NM`s. A reset made
    /// before the page had that message leaves them alone.
    pub fn record_annot_sent(&mut self, id: DocId, seq: u64, names: Vec<String>) {
        if let Some(st) = self.docs.iter_mut().find(|d| d.id == id).and_then(|d| d.sync.as_mut()) {
            st.sent.push((seq, names));
        }
    }

    /// The annotations of the actor's that could not be written out since this was last called
    /// (too large for the database, or of a kind it has no form for).
    pub fn take_annot_problems(&mut self, id: DocId) -> Vec<String> {
        let Some(st) = self.docs.iter_mut().find(|d| d.id == id).and_then(|d| d.sync.as_mut()) else { return Vec::new() };
        std::mem::take(&mut st.problems)
    }

    /// The server's whole list, taken as the truth: what it lacks is removed and what it has is put
    /// in (an annotation already here keeps its place, so replies stay linked). The actor's own
    /// annotations follow it too, in the working document and in the undo and redo states that hold
    /// the same version of them (a state with a version the actor made keeps it), except the ones the actor changed after the page fetched the list: those are the
    /// numbers above `seq` (see [`Session::record_annot_sent`]) or not yet handed out. Annotations
    /// that came with the file (never sent) are left alone.
    pub fn annots_reset(&mut self, id: DocId, items: Vec<RemoteAnnot>) -> Result<RemoteReport, EditError> {
        self.annots_reset_at(id, items, u64::MAX)
    }

    /// [`Session::annots_reset`] for a list fetched when the page had the change messages up to `seq`.
    pub fn annots_reset_at(&mut self, id: DocId, items: Vec<RemoteAnnot>, seq: u64) -> Result<RemoteReport, EditError> {
        self.annots_one(id, AnnotOp::Reset { items, seq })
    }

    /// Changes the server pushed: annotations to put in (the same `/NM` is replaced) and `/NM`s to remove.
    pub fn annots_apply(&mut self, id: DocId, upserts: Vec<RemoteAnnot>, deletes: Vec<String>) -> Result<RemoteReport, EditError> {
        self.annots_one(id, AnnotOp::Apply { upserts, deletes })
    }

    /// The server refused these changes of the actor's: put each annotation back as the server has it.
    pub fn annots_reject(&mut self, id: DocId, items: Vec<RejectedAnnot>) -> Result<RemoteReport, EditError> {
        self.annots_one(id, AnnotOp::Reject(items))
    }

    fn annots_one(&mut self, id: DocId, op: AnnotOp) -> Result<RemoteReport, EditError> {
        Ok(self.annots_run(id, vec![op])?.pop().unwrap_or_default())
    }

    /// Run several server messages in order with one refresh at the end; one report per message.
    pub fn annots_run(&mut self, id: DocId, ops: Vec<AnnotOp>) -> Result<Vec<RemoteReport>, EditError> {
        let doc = self.sync_doc_mut(id)?;
        let mut reports = Vec::with_capacity(ops.len());
        let mut changed = false;
        let mut failure = None;
        for op in ops {
            let plan = Self::op_items(doc, op);
            match Self::patch_remote(doc, plan) {
                Ok((report, did)) => {
                    changed |= did;
                    reports.push(report);
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        if changed {
            // Same refresh as a comment edit, but no undo step, no dirty flag.
            if let Err(e) = Session::refresh_scoped(doc, Scope::Comments) {
                Session::show_released(doc);
                return Err(e);
            }
        }
        failure.map_or(Ok(reports), Err)
    }

    /// What one server message comes to.
    fn op_items(doc: &mut Document, op: AnnotOp) -> Plan {
        let plain = |upserts, deletes, mode| Plan { upserts, deletes, mine_deletes: Vec::new(), mode, guard: HashSet::new() };
        match op {
            AnnotOp::Apply { upserts, deletes } => plain(upserts, deletes, Mode::Apply),
            AnnotOp::Reject(items) => {
                let mut upserts = Vec::new();
                let mut deletes = Vec::new();
                for i in items {
                    match i.server {
                        Some(mut s) => {
                            s.annot_id = i.annot_id;
                            upserts.push(s);
                        }
                        None => deletes.push(i.annot_id),
                    }
                }
                // Always applied: the local copy is what differs from the server.
                plain(upserts, deletes, Mode::Reject)
            }
            AnnotOp::Reset { items, seq } => {
                sync_now(doc);
                let Some(st) = doc.sync.as_mut() else { return plain(items, Vec::new(), Mode::Reset) };
                // The page has seen the messages up to `seq`; later ones, and changes not yet
                // handed out, are newer than the list.
                st.sent.retain(|(n, _)| *n > seq);
                let guard: HashSet<String> = st
                    .sent
                    .iter()
                    .flat_map(|(_, names)| names.iter().cloned())
                    .chain(st.upserts.keys().cloned())
                    .chain(st.deletes.iter().cloned())
                    .collect();
                let listed: HashSet<&str> = items.iter().map(|i| i.annot_id.as_str()).collect();
                let gone = |nm: &&String| !listed.contains(nm.as_str()) && !guard.contains(*nm);
                let deletes: Vec<String> = st.remote.keys().filter(gone).cloned().collect();
                // Only those the server knows: one that came with the file was never sent, so its absence
                // from the list says nothing.
                let mine_deletes: Vec<String> = st.mine.iter().filter(|(nm, e)| e.synced && gone(nm)).map(|(nm, _)| nm.clone()).collect();
                Plan { upserts: items, deletes, mine_deletes, mode: Mode::Reset, guard }
            }
        }
    }

    /// Apply the server's changes outside the edit pipeline: to the working document and to every
    /// state undo and redo can bring back, except that a reset changes the actor's own annotations
    /// in the working document and the states that hold the same version of it. The caller refreshes. Returns the report and whether the
    /// document changed.
    fn patch_remote(doc: &mut Document, plan: Plan) -> Result<(RemoteReport, bool), EditError> {
        let Plan { upserts, deletes, mine_deletes, mode, guard } = plan;
        // Changes the user made but the interface hasn't taken yet stay pending.
        sync_now(doc);
        let mut report = RemoteReport::default();
        let actor = doc.ownership.actor.clone().unwrap_or_default();
        let hash_item = |a: &RemoteAnnot| hash_of(&(&a.author_id, &a.xfdf));
        let mut live: Vec<RemoteAnnot> = Vec::new();
        // The actor's own annotations a reset differs on: they change in the working document and the states holding the same version.
        let mut current_only: Vec<RemoteAnnot> = Vec::new();
        // The ones of the actor's the cache knows: they go only into states that have them.
        let mut cached_mine: HashSet<String> = HashSet::new();
        if let Some(st) = doc.sync.as_mut() {
            for a in upserts {
                let h = hash_item(&a);
                if mode == Mode::Reset && guard.contains(&a.annot_id) {
                    report.skipped += 1;
                    continue;
                }
                if mode != Mode::Reject {
                    if st.remote.get(&a.annot_id) == Some(&h) {
                        report.skipped += 1;
                        continue;
                    }
                    // Deleted here and not yet told: the server's copy is the one to go.
                    if st.deletes.contains(&a.annot_id) {
                        report.skipped += 1;
                        continue;
                    }
                }
                if let Some(e) = st.mine.get_mut(&a.annot_id) {
                    // The same text as the server last sent or as the actor's own copy is not news.
                    let same = e.server == h || (mode == Mode::Reset && e.xfdf == hash_of(&a.xfdf));
                    if mode == Mode::Reset {
                        if same {
                            e.server = h;
                            e.synced = true;
                            report.skipped += 1;
                        } else {
                            current_only.push(a);
                        }
                        continue;
                    }
                    if mode == Mode::Apply && (st.upserts.contains_key(&a.annot_id) || same) {
                        report.skipped += 1;
                        continue;
                    }
                    cached_mine.insert(a.annot_id.clone());
                }
                live.push(a);
            }
        } else {
            live = upserts;
        }
        // A sealed annotation (a signature) is final here: the server's news can't replace it, and
        // neither can its absence from a list remove it. A rejection is the server withdrawing the
        // actor's own refused change, so that one goes through.
        let (mut deletes, mut mine_deletes) = (deletes, mine_deletes);
        if mode != Mode::Reject
            && let Some(editor) = doc.editor.as_ref()
        {
            let mut sealed: HashSet<String> = HashSet::new();
            walk(&editor.cos, |_, _, _, _, d| {
                if pdfcraft_annot::is_sealed(&editor.cos, d) {
                    sealed.extend(nm_of(&editor.cos, d));
                }
            });
            if !sealed.is_empty() {
                let before = live.len() + current_only.len() + deletes.len() + mine_deletes.len();
                live.retain(|a| !sealed.contains(&a.annot_id));
                current_only.retain(|a| !sealed.contains(&a.annot_id));
                deletes.retain(|nm| !sealed.contains(nm));
                mine_deletes.retain(|nm| !sealed.contains(nm));
                report.skipped += before - (live.len() + current_only.len() + deletes.len() + mine_deletes.len());
            }
        }
        let reason = doc.read_only_reason.clone().unwrap_or_default();
        let editor = doc.editor.as_mut().ok_or(EditError::ReadOnly(reason))?;
        let mut pictures = pdfcraft_xfdf::PictureCache::default();
        let doc_items: Vec<&RemoteAnnot> = live.iter().chain(&current_only).collect();
        let doc_deletes: Vec<String> = deletes.iter().chain(&mine_deletes).cloned().collect();
        // The actor's own annotations a reset changes: how each looks now, to tell the states that
        // hold the same version (they get the change) from those holding one the actor made later or
        // earlier (they stay, so undoing that edit still works).
        let own_changed: HashSet<&str> = current_only.iter().map(|a| a.annot_id.as_str()).chain(mine_deletes.iter().map(String::as_str)).collect();
        let before: HashMap<&str, u64> = if own_changed.is_empty() {
            HashMap::new()
        } else {
            let idx = xf::index(&editor.cos);
            own_changed.iter().filter_map(|nm| xf::export(&editor.cos, &idx, nm).map(|x| (*nm, hash_of(&x.xfdf)))).collect()
        };
        // The pages the items are on before and after: the ones to draw again.
        let wanted: HashSet<&str> = doc_items.iter().map(|a| a.annot_id.as_str()).chain(doc_deletes.iter().map(String::as_str)).collect();
        let old_pages = pages_of(&editor.cos, &wanted);
        let done = xf::apply(&mut editor.cos, &doc_items, &doc_deletes, &mut pictures);
        report.failed = done.failed.clone();
        report.applied = done.applied.len() + done.removed.len();
        if report.applied > 0 {
            let changed: HashSet<&str> = done.applied.iter().chain(&done.removed).map(String::as_str).collect();
            let new_pages = pages_of(&editor.cos, &changed);
            for nm in &changed {
                match (old_pages.get(*nm), new_pages.get(*nm)) {
                    (None, None) => report.pages_unknown = true,
                    (old, new) => report.pages.extend(old.into_iter().chain(new).copied()),
                }
            }
        }
        // The stored states get what the working document got; one that can't take an item is a
        // state undo then shows without it.
        let placed: HashSet<&str> = done.applied.iter().map(String::as_str).collect();
        let on_states: Vec<&RemoteAnnot> = live.iter().filter(|a| placed.contains(a.annot_id.as_str())).collect();
        let own_placed: Vec<&RemoteAnnot> = current_only.iter().filter(|a| placed.contains(a.annot_id.as_str())).collect();
        if !on_states.is_empty() || !deletes.is_empty() || !before.is_empty() {
            let any_mine = on_states.iter().any(|a| cached_mine.contains(&a.annot_id));
            for (_, state, _, _) in editor.undo.iter_mut().chain(editor.redo.iter_mut()) {
                let here: HashSet<String> = if any_mine {
                    let mut names = HashSet::new();
                    walk(state, |_, _, _, _, d| names.extend(nm_of(state, d)));
                    names
                } else {
                    HashSet::new()
                };
                let items: Vec<&RemoteAnnot> =
                    on_states.iter().copied().filter(|a| !cached_mine.contains(&a.annot_id) || here.contains(&a.annot_id)).collect();
                let (mut items, mut gone) = (items, deletes.clone());
                if !before.is_empty() {
                    let idx = xf::index(state);
                    // A version that can't be exported (here or in the working document) is not known to be the same.
                    let same = |nm: &str| match (xf::export(state, &idx, nm), before.get(nm)) {
                        (Some(x), Some(b)) => hash_of(&x.xfdf) == *b,
                        _ => false,
                    };
                    items.extend(own_placed.iter().copied().filter(|a| same(&a.annot_id)));
                    gone.extend(mine_deletes.iter().filter(|nm| same(nm)).cloned());
                }
                xf::apply(state, &items, &gone, &mut pictures);
            }
        }
        if done.applied.is_empty() && done.removed.is_empty() {
            return Ok((report, false));
        }
        let touched: HashSet<String> = done.applied.iter().chain(&doc_deletes).cloned().collect();
        let hashes: HashMap<&str, u64> =
            doc_items.iter().filter(|a| placed.contains(a.annot_id.as_str())).map(|a| (a.annot_id.as_str(), hash_item(a))).collect();
        doc.generation += 1;
        // What the server sent is not the user's change: take it into the cache so it isn't handed back.
        let generation = doc.generation;
        let (Some(st), Some(editor)) = (doc.sync.as_mut(), doc.editor.as_ref()) else { return Ok((report, true)) };
        for nm in &touched {
            st.forget(nm);
        }
        let cos = &editor.cos;
        let mut seen: HashSet<String> = HashSet::new();
        type Touched = (String, Option<Arc<Object>>, usize, Option<(u64, u64)>, bool);
        let mut entries: Vec<Touched> = Vec::new();
        let prune = !done.removed.is_empty();
        let mut present: HashSet<String> = HashSet::new();
        walk(cos, |page, _, _, arc, d| {
            let Some(nm) = nm_of(cos, d) else { return };
            if prune {
                present.insert(nm.clone());
            }
            if !touched.contains(&nm) || !seen.insert(nm.clone()) {
                return;
            }
            entries.push((nm, arc.cloned(), page, fingerprint(cos, d), is_mine(cos, d, &actor)));
        });
        // One scan of the document for all the exports below, built when the first is needed.
        let mut index = None;
        for (nm, arc, page, fp, mine) in entries {
            let server = hashes.get(nm.as_str()).copied().unwrap_or(0);
            if mine {
                let xfdf = xf::export(cos, index.get_or_insert_with(|| xf::index(cos)), &nm).map_or(0, |x| hash_of(&x.xfdf));
                st.mine.insert(nm, Entry { page, fp, xfdf, server, arc, synced: true });
            } else if server != 0 {
                st.remote.insert(nm, server);
            }
        }
        // Replies that went with a removed annotation are gone too.
        if prune {
            st.mine.retain(|k, _| present.contains(k));
            st.remote.retain(|k, _| present.contains(k));
            st.upserts.retain(|k, _| present.contains(k));
            st.failed.retain(|k, _| present.contains(k));
        }
        st.seen = generation;
        Ok((report, true))
    }

    /// Refuse an undo or redo that would take away an annotation someone replied to.
    pub(crate) fn check_history_step(doc: &Document, undo: bool) -> Result<(), EditError> {
        let (Some(actor), Some(editor)) = (doc.ownership.actor.as_deref(), doc.editor.as_ref()) else { return Ok(()) };
        if doc.sync.is_none() {
            return Ok(());
        }
        let step = if undo { editor.undo.last() } else { editor.redo.last() };
        let Some((_, target, _, _)) = step else { return Ok(()) };
        Ok(check_step(&editor.cos, target, actor)?)
    }
}

/// Is `edit` work on annotations only, which in `annotSync` mode goes to the host's database and
/// so is nothing to save?
pub(crate) fn is_annotation_work(edit: &Edit) -> bool {
    match edit {
        Edit::Batch { edits, .. } => edits.iter().all(is_annotation_work),
        Edit::ImportData { .. } => true,
        other => crate::scope_of(other) == Scope::Comments,
    }
}

#[cfg(test)]
mod sign_box_tests {
    use super::*;
    use pdfcraft_cos::PdfString;

    fn annot(subtype: &str, role: Option<&str>, rect: [i64; 4]) -> Dict {
        annot_with(subtype, role, None, rect)
    }

    fn annot_with(subtype: &str, role: Option<&str>, signer: Option<&str>, rect: [i64; 4]) -> Dict {
        let mut d = Dict::new();
        d.set(b"Subtype".to_vec(), Object::name(subtype));
        d.set(b"Rect".to_vec(), Object::Array(rect.iter().map(|v| Object::Int(*v)).collect()));
        if let Some(r) = role {
            d.set(b"PpmSignRole".to_vec(), PdfString::text(r));
        }
        if let Some(s) = signer {
            d.set(b"PpmSigner".to_vec(), PdfString::text(s));
        }
        d
    }

    #[test]
    fn only_a_freetext_with_a_role_is_a_sign_box_and_its_rect_is_normalized() {
        let cos = Cos::new_empty();
        assert_eq!(sign_box_of(&cos, &annot("FreeText", Some("reviewer"), [50, 90, 10, 30])), Some(("reviewer".into(), None, [10.0, 30.0, 50.0, 90.0])));
        assert_eq!(
            sign_box_of(&cos, &annot_with("FreeText", Some("reviewer"), Some("u-kim"), [0, 0, 4, 2])),
            Some(("reviewer".into(), Some("u-kim".into()), [0.0, 0.0, 4.0, 2.0]))
        );
        // The signer is passed on as stored, blank or padded too; a signer without a role is no sign box.
        assert_eq!(sign_box_of(&cos, &annot_with("FreeText", Some("reviewer"), Some(" "), [0, 0, 4, 2])).unwrap().1, Some(" ".into()));
        assert_eq!(sign_box_of(&cos, &annot_with("FreeText", Some("reviewer"), Some(" kim "), [0, 0, 4, 2])).unwrap().1, Some(" kim ".into()));
        assert_eq!(sign_box_of(&cos, &annot_with("FreeText", None, Some("u-kim"), [0, 0, 1, 1])), None);
        assert_eq!(sign_box_of(&cos, &annot("FreeText", None, [0, 0, 1, 1])), None);
        assert_eq!(sign_box_of(&cos, &annot("FreeText", Some(" "), [0, 0, 1, 1])), None);
        assert_eq!(sign_box_of(&cos, &annot("Stamp", Some("reviewer"), [0, 0, 1, 1])), None);
    }
}
