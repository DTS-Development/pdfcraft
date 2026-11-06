//! One annotation at a time, for the host's annotation database (PPM server).
//!
//! The server keeps each annotation as one XFDF document with exactly one element inside
//! `<annots>`, its `name` equal to the annotation id (our `/NM`) and a zero-based `page`. This
//! module writes that form ([`export_annotation`]) and applies the server's copies of other
//! users' annotations ([`apply_remote`], [`remove_by_name`]).
//!
//! The dialect is the one PDFTron WebViewer writes, which the /viewer reads from the same rows:
//! a stamp's picture is `<imagedata>` (a data URL), and extra values are the
//! `<trn-custom-data bytes='{"key":"string"}'/>` string map. PdfCraft's own values ride the same
//! channel so a round trip loses nothing:
//!
//! | PDF                                   | custom data key                              |
//! |---------------------------------------|----------------------------------------------|
//! | `/PpmStamp` (host stamp meta, JSON)   | `ppm-stamp`, or `ppm-signature` for a signature (JSON with `date` and no `templateId`) |
//! | `/PpmSignRole` (sign box, FreeText)   | `ppmSignRole`                                |
//! | `/PpmSigner` (sign box's assigned user) | `ppmSigner`                                 |
//! | `/IRT` of a non-note (caret on a strike-out) | `ppmIrt` (parent `/NM`); the server allows `inreplyto` on notes only |
//! | fork-only keys (`PCOutline`, `PCStamp`…) | `ppmFork` (JSON object)                   |
//! | any other key (kept verbatim)         | stored in `/PpmCustomData` (JSON object)     |
//!
//! Remote application never goes through ownership: the item's author becomes `/PpmOwner`, the
//! `/NM` is kept, and a locked or foreign annotation is replaced all the same (the server is
//! the authority).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use base64::Engine as _;
use pdfcraft_annot::PICTURE_SOURCE_KEY;
use pdfcraft_annot::ownership::OWNER_KEY;
use serde_json::{Map, Number, Value};

use super::*;

/// The most characters (UTF-16 code units, as the server counts them) one item's XFDF may have.
pub const MAX_XFDF_LEN: usize = 200_000;
/// Longest annotation id the server accepts.
const MAX_ID_LEN: usize = 200;
/// Longest picture a remote item may carry, decoded.
const MAX_PICTURE_BYTES: usize = 8 * 1024 * 1024;

const PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8" ?><xfdf xmlns="http://ns.adobe.com/xfdf/" xml:space="preserve"><fields /><annots>"#;
const SUFFIX: &str = "</annots></xfdf>";

const STAMP_KEY: &str = "PpmStamp";
const SIGN_ROLE_KEY: &str = "PpmSignRole";
const SIGNER_KEY: &str = "PpmSigner";
const CUSTOM_KEY: &str = "PpmCustomData";
/// Hash of the data URL a picture stamp was made from, so a re-applied item reuses its picture.
const PICTURE_HASH_KEY: &str = "PpmImageHash";

/// One annotation as the server stores it.
#[derive(Clone, Debug, PartialEq)]
pub struct AnnotXfdf {
    /// The annotation's `/NM`.
    pub annot_id: String,
    /// Zero-based page.
    pub page: usize,
    /// What /viewer calls the type: the `/Subj`, else the XFDF element name.
    pub kind: String,
    /// The whole XFDF document.
    pub xfdf: String,
}

/// An annotation of another user, as the server sends it.
#[derive(Clone, Copy, Debug)]
pub struct RemoteItem<'a> {
    pub annot_id: &'a str,
    pub author_id: &'a str,
    pub xfdf: &'a str,
}

/// What [`apply_remote`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteReport {
    /// Ids placed (inserted or replaced).
    pub applied: Vec<String>,
    /// Ids of the requested deletes that were present.
    pub removed: Vec<String>,
    /// (id, reason) for items that could not be applied. The document keeps what it had.
    pub failed: Vec<(String, String)>,
}

pub(crate) fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

// ── locating ────────────────────────────────────────────────────────────────────────────────

/// Where an annotation sits: page index, page object, index in the page's `/Annots`, object.
struct Spot {
    page: usize,
    page_ref: ObjRef,
    index: usize,
    obj: ObjRef,
}

/// A name index over a document's annotations, built once so that looking an annotation up (or
/// exporting many) doesn't scan every page each time.
///
/// Entries are checked against the page when used, so an index stays right while annotations
/// are replaced, moved or deleted through [`apply_remote_cached`] and [`remove_by_name_in`]; one
/// built before other changes to the document must be rebuilt.
pub struct AnnotIndex {
    pages: Vec<ObjRef>,
    /// (page index, object) for every annotation with this `/NM`; links, widgets and pop-ups
    /// are left out, since an item from the server never stands for one of those.
    by_name: HashMap<String, Vec<(usize, ObjRef)>>,
    /// The name of every annotation that has one, for `inreplyto`.
    names: HashMap<ObjRef, String>,
}

impl AnnotIndex {
    pub fn new(doc: &Document) -> Self {
        let pages = pdfcraft_model::pages(doc);
        let names = annot_names(doc, &pages);
        let mut by_name: HashMap<String, Vec<(usize, ObjRef)>> = HashMap::new();
        for (pi, p) in pages.iter().enumerate() {
            for a in p.dict.get(b"Annots").map(|a| doc.resolve(a)).and_then(|a| a.as_array().cloned()).unwrap_or_default() {
                if let Some(r) = a.as_ref()
                    && let Some(nm) = names.get(&r)
                    && !is_server_blind(doc, r)
                {
                    by_name.entry(nm.clone()).or_default().push((pi, r));
                }
            }
        }
        Self { pages: pages.iter().map(|p| p.obj).collect(), by_name, names }
    }

    fn add(&mut self, nm: &str, page: usize, obj: ObjRef) {
        let list = self.by_name.entry(nm.into()).or_default();
        if !list.contains(&(page, obj)) {
            list.push((page, obj));
        }
        self.names.insert(obj, nm.into());
    }
}

/// Form widgets, links and pop-ups are not annotations of the server's kind.
fn is_server_blind(doc: &Document, r: ObjRef) -> bool {
    doc.get(r).as_dict().and_then(|d| d.name(b"Subtype")).is_some_and(|s| matches!(s, b"Widget" | b"Link" | b"Popup"))
}

/// Every annotation named `nm`, in page order.
fn locate(doc: &Document, idx: &AnnotIndex, nm: &str) -> Vec<Spot> {
    let mut found = Vec::new();
    for &(page, obj) in idx.by_name.get(nm).map(Vec::as_slice).unwrap_or_default() {
        let page_ref = idx.pages[page];
        // An entry whose object has since left the page (replaced, moved, deleted) is stale.
        if let Some(index) = annots_of(doc, page_ref).iter().position(|e| e.as_ref() == Some(obj)) {
            found.push(Spot { page, page_ref, index, obj });
        }
    }
    found.sort_by_key(|s| (s.page, s.index));
    found
}

// ── export ──────────────────────────────────────────────────────────────────────────────────

/// The annotation `nm` as a standalone XFDF document the server accepts.
///
/// Errors: [`DataError::NotFound`], [`DataError::TooLarge`] (over [`MAX_XFDF_LEN`], typically a
/// big stamp picture), [`DataError::Invalid`] (an id longer than the server allows, a reply
/// whose parent has no `/NM`, or a kind with no XFDF form such as a link).
pub fn export_annotation(doc: &Document, nm: &str) -> Result<AnnotXfdf, DataError> {
    export_annotation_in(doc, &AnnotIndex::new(doc), nm)
}

/// Several annotations at once: one scan of the document for all of `names`. The result keeps
/// the order of `names`; an annotation that can't be exported is `None`.
pub fn export_annotations(doc: &Document, names: &[String]) -> Vec<(String, Option<AnnotXfdf>)> {
    let idx = AnnotIndex::new(doc);
    names.iter().map(|nm| (nm.clone(), export_annotation_in(doc, &idx, nm).ok())).collect()
}

/// [`export_annotation`] with an index built once for many exports of the same document.
pub fn export_annotation_in(doc: &Document, idx: &AnnotIndex, nm: &str) -> Result<AnnotXfdf, DataError> {
    if utf16_len(nm) > MAX_ID_LEN {
        return Err(DataError::Invalid("the annotation name is too long".into()));
    }
    let Some(spot) = locate(doc, idx, nm).into_iter().next() else { return Err(DataError::NotFound(nm.into())) };
    let d = doc.get(spot.obj).as_dict().cloned().unwrap_or_default();
    element_xfdf(doc, &d, spot.page, &idx.names, nm)
}

/// The XFDF document of the annotation dictionary `d` on zero-based `page`, with `names` giving the
/// id of each annotation (for `inreplyto`) and `nm` this one's.
pub(crate) fn element_xfdf(doc: &Document, d: &Dict, page: usize, names: &HashMap<ObjRef, String>, nm: &str) -> Result<AnnotXfdf, DataError> {
    if utf16_len(nm) > MAX_ID_LEN {
        return Err(DataError::Invalid("the annotation name is too long".into()));
    }
    if let Some(parent) = d.get(b"IRT").and_then(Object::as_ref)
        && !names.contains_key(&parent)
    {
        return Err(DataError::Invalid("the reply's parent has no name".into()));
    }
    // A kept picture's size is known before it is encoded: don't encode what can't be sent.
    if let Some(len) = kept_picture_len(doc, d)
        && len.div_ceil(3) * 4 > MAX_XFDF_LEN
    {
        return Err(DataError::TooLarge { len: len.div_ceil(3) * 4, max: MAX_XFDF_LEN });
    }
    let mut element = String::new();
    let Some(tag) = annot_element(doc, d, page, names, true, &mut element) else {
        return Err(DataError::Invalid("this kind of annotation has no XFDF form".into()));
    };
    let xfdf = format!("{PREFIX}{}{SUFFIX}", element.trim_end());
    let len = utf16_len(&xfdf);
    if len > MAX_XFDF_LEN {
        return Err(DataError::TooLarge { len, max: MAX_XFDF_LEN });
    }
    let kind = text_of(doc, d, b"Subj").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| tag.to_string());
    // The server counts UTF-16 units, not characters.
    let mut kept = 0;
    let kind: String = kind
        .chars()
        .take_while(|c| {
            kept += c.len_utf16();
            kept <= 100
        })
        .collect();
    Ok(AnnotXfdf { annot_id: nm.into(), page, kind, xfdf })
}

/// Bytes of the original picture file a stamp keeps, if it keeps one.
fn kept_picture_len(doc: &Document, d: &Dict) -> Option<usize> {
    match &*doc.get(d.reference(PICTURE_SOURCE_KEY.as_bytes())?) {
        Object::Stream(s) => Some(s.raw.len()),
        _ => None,
    }
}

/// The children after the generic ones: the stamp picture and the custom data.
pub(crate) fn rich_children(doc: &Document, d: &Dict, tag: &str, irt_name: Option<&str>, out: &mut String) {
    if tag == "stamp"
        && let Some(url) = picture_url(doc, d)
    {
        let _ = write!(out, "<imagedata>{url}</imagedata>");
    }
    let map = custom_map(doc, d, tag, irt_name);
    if !map.is_empty()
        && let Ok(json) = serde_json::to_string(&Value::Object(map))
    {
        let _ = write!(out, "<trn-custom-data bytes=\"{}\"/>", esc(&json));
    }
}

/// A picture stamp's image as a data URL.
fn picture_url(doc: &Document, d: &Dict) -> Option<String> {
    if !matches!(d.get(b"PCPictureImage").map(|o| doc.resolve(o)).as_deref(), Some(Object::Bool(true))) {
        return None;
    }
    // The file the stamp came from goes out as it is; only a stamp without one is re-encoded.
    let kept = d.reference(PICTURE_SOURCE_KEY.as_bytes()).and_then(|r| match &*doc.get(r) {
        Object::Stream(s) => Some(s.raw.to_vec()),
        _ => None,
    });
    let (mime, bytes) = match kept.as_deref().and_then(sniff) {
        Some(mime) => (mime, kept.unwrap_or_default()),
        None => {
            let (ext, bytes) = pdfcraft_create::image_file(doc, d.reference(b"PCPicture")?).ok()?;
            (if ext == "jpg" { "image/jpeg" } else { "image/png" }, bytes)
        }
    };
    Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// The MIME type of an image file by its signature.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// How a fork-only key is carried in the `ppmFork` JSON object.
#[derive(Clone, Copy)]
enum Kind {
    Bool,
    Int,
    Text,
    Name,
    Nums,
    NumLists,
    /// A border-effect dictionary (`/BE`): the cloud style of a cloud polygon.
    Effect,
}

/// Keys PdfCraft's own annotations carry that XFDF has no field for.
const FORK_KEYS: [(&str, Kind); 10] = [
    ("IT", Kind::Name),
    ("PCStamp", Kind::Bool),
    ("PCByLine", Kind::Text),
    ("PCStampRotate", Kind::Int),
    ("PCFillSign", Kind::Bool),
    ("PCOutline", Kind::NumLists),
    ("PCPictureImage", Kind::Bool),
    ("PCPictureSize", Kind::Nums),
    ("PCPictureRotate", Kind::Int),
    ("BE", Kind::Effect),
];

fn num(v: f64) -> Value {
    Number::from_f64(v).map_or(Value::Null, Value::Number)
}

fn fork_value(doc: &Document, d: &Dict, key: &str, kind: Kind) -> Option<Value> {
    let k = key.as_bytes();
    Some(match kind {
        Kind::Bool => match d.get(k).map(|o| doc.resolve(o))?.as_ref() {
            Object::Bool(b) => Value::Bool(*b),
            _ => return None,
        },
        Kind::Int => Value::from(d.get(k).and_then(|o| doc.resolve(o).as_int())?),
        Kind::Text => Value::String(text_of(doc, d, k)?),
        Kind::Name => Value::String(String::from_utf8_lossy(d.name(k)?).into_owned()),
        Kind::Nums => Value::Array(nums_of(doc, d.get(k)).into_iter().map(num).collect()),
        Kind::Effect => {
            let be = d.get(k).map(|o| doc.resolve(o))?.as_dict().cloned()?;
            let mut m = Map::new();
            m.insert("S".into(), Value::String(String::from_utf8_lossy(be.name(b"S")?).into_owned()));
            m.insert("I".into(), be.get(b"I").and_then(|i| doc.resolve(i).as_f64()).map_or(Value::Null, num));
            Value::Object(m)
        }
        Kind::NumLists => {
            let list = d.get(k).map(|o| doc.resolve(o)).and_then(|o| o.as_array().cloned())?;
            Value::Array(list.iter().map(|g| Value::Array(nums_of(doc, Some(g)).into_iter().map(num).collect())).collect())
        }
    })
}

fn fork_object(v: &Value, kind: Kind) -> Option<Object> {
    let nums = |a: &Value| -> Option<Vec<f64>> { a.as_array()?.iter().map(Value::as_f64).collect() };
    Some(match kind {
        Kind::Bool => Object::Bool(v.as_bool()?),
        Kind::Int => Object::Int(v.as_i64()?),
        Kind::Text => PdfString::text(v.as_str()?).into(),
        Kind::Name => Object::name(v.as_str()?),
        Kind::Nums => arr(&nums(v)?),
        Kind::Effect => {
            let mut be = Dict::new();
            be.set(b"S".to_vec(), Object::name(v.get("S")?.as_str()?));
            if let Some(i) = v.get("I").and_then(Value::as_f64) {
                be.set(b"I".to_vec(), Object::Int(i as i64));
            }
            Object::Dict(be)
        }
        Kind::NumLists => Object::Array(v.as_array()?.iter().map(|g| nums(g).map(|n| arr(&n))).collect::<Option<Vec<_>>>()?),
    })
}

/// The custom data map of an annotation: what it held when it came in, plus its own values.
fn custom_map(doc: &Document, d: &Dict, tag: &str, irt_name: Option<&str>) -> Map<String, Value> {
    let mut map: Map<String, Value> = text_of(doc, d, CUSTOM_KEY.as_bytes())
        .and_then(|j| serde_json::from_str::<Value>(&j).ok())
        .and_then(|v| if let Value::Object(m) = v { Some(m) } else { None })
        .unwrap_or_default();
    if let Some(meta) = text_of(doc, d, STAMP_KEY.as_bytes()) {
        // A host signature carries only its date; a stamp carries the template it was made from.
        let signature = serde_json::from_str::<Value>(&meta).is_ok_and(|v| v.get("templateId").is_none() && v.get("date").is_some());
        map.insert((if signature { "ppm-signature" } else { "ppm-stamp" }).into(), Value::String(meta));
    }
    if tag == "freetext"
        && let Some(role) = text_of(doc, d, SIGN_ROLE_KEY.as_bytes())
    {
        map.insert("ppmSignRole".into(), Value::String(role));
        if let Some(signer) = text_of(doc, d, SIGNER_KEY.as_bytes()) {
            map.insert("ppmSigner".into(), Value::String(signer));
        }
    }
    if let Some(parent) = irt_name {
        map.insert("ppmIrt".into(), Value::String(parent.into()));
    }
    let fork: Map<String, Value> = FORK_KEYS.iter().filter_map(|(k, kind)| fork_value(doc, d, k, *kind).map(|v| ((*k).to_string(), v))).collect();
    // The kind-specific keys only matter where the generic XFDF fields can't rebuild them.
    if !fork.is_empty()
        && let Ok(json) = serde_json::to_string(&Value::Object(fork))
    {
        map.insert("ppmFork".into(), Value::String(json));
    }
    map
}

// ── remote application ──────────────────────────────────────────────────────────────────────

/// Remove the annotation `nm` with its pop-up and replies (and theirs), whatever its lock flag
/// or owner. `true` when something was removed.
pub fn remove_by_name(doc: &mut Document, nm: &str) -> bool {
    remove_by_name_in(doc, &AnnotIndex::new(doc), nm)
}

/// [`remove_by_name`] with an index that is still current ([`AnnotIndex`]).
pub fn remove_by_name_in(doc: &mut Document, idx: &AnnotIndex, nm: &str) -> bool {
    let mut removed = false;
    // A name can repeat (a file from elsewhere); each pass removes one, so bound the passes.
    for _ in 0..1000 {
        let Some(spot) = locate(doc, idx, nm).into_iter().next() else { break };
        if pdfcraft_annot::delete_annotation_unchecked(doc, spot.page, spot.index).is_err() {
            break;
        }
        removed = true;
    }
    removed
}

/// Pictures decoded for remote items, kept between calls so that applying one item to many
/// copies of a document (the working one and every undo state) decodes its image once and shares
/// the bytes.
#[derive(Default)]
pub struct PictureCache {
    by_hash: HashMap<String, Prepared>,
}

/// A decoded picture, held in a scratch document and copied into the documents that need it.
struct Prepared {
    scratch: Document,
    xobject: ObjRef,
    size: (f64, f64),
    source: ObjRef,
}

/// Apply the server's annotations: first `deletes` (by `/NM`), then `upserts` (inserted, or
/// replacing the annotation with the same `/NM`, even on another page).
///
/// Each upsert's `/PpmOwner` becomes its `author_id`, the `/NM` is kept, the appearance is
/// redrawn, a pop-up and the reply link (`inreplyto`, or `ppmIrt` for a caret) are rebuilt.
/// Replies may come before their parent in the slice. Nothing here checks ownership or locks,
/// and applying the same batch twice, or onto a snapshot that lacks the annotation, is fine.
pub fn apply_remote(doc: &mut Document, upserts: &[RemoteItem], deletes: &[String]) -> RemoteReport {
    apply_remote_cached(doc, upserts, deletes, &mut PictureCache::default())
}

/// [`apply_remote`] sharing `pictures` across calls: pass the same cache for every document the
/// same batch goes to.
pub fn apply_remote_cached(doc: &mut Document, upserts: &[RemoteItem], deletes: &[String], pictures: &mut PictureCache) -> RemoteReport {
    let mut report = RemoteReport::default();
    // One scan of the document; every lookup below goes through the index.
    let mut idx = AnnotIndex::new(doc);
    for nm in deletes {
        if remove_by_name_in(doc, &idx, nm) {
            report.removed.push(nm.clone());
        }
    }
    let mut by_name: HashMap<&str, ObjRef> = HashMap::new();
    // (reply, parent, id) to link once every parent of the batch is placed.
    let mut replies: Vec<(ObjRef, Parent, &str)> = Vec::new();
    for item in upserts {
        match place_item(doc, &mut idx, pictures, item) {
            Ok(placed) => {
                by_name.insert(item.annot_id, placed.obj);
                if let Some(parent) = placed.parent {
                    replies.push((placed.obj, parent, item.annot_id));
                }
                report.applied.push(item.annot_id.into());
            }
            Err(why) => report.failed.push((item.annot_id.into(), why)),
        }
    }
    for (reply, parent, id) in replies {
        let target = by_name.get(parent.name.as_str()).copied().or_else(|| locate(doc, &idx, &parent.name).first().map(|s| s.obj));
        let linked = target.filter(|t| *t != reply).is_some_and(|t| doc.update_dict(reply, |d| d.set(b"IRT".to_vec(), Object::Ref(t))).is_ok());
        // A caret's parent (a strike-out) is its own row on the server and may arrive in a later
        // batch: the caret stays, unlinked, until then.
        if !linked && parent.required {
            // A reply with no parent would show as a top-level note; leave it out.
            remove_by_name_in(doc, &idx, id);
            report.applied.retain(|a| a != id);
            report.failed.push((id.into(), format!("its parent {} is missing", parent.name)));
        }
    }
    report
}

struct Placed {
    obj: ObjRef,
    parent: Option<Parent>,
}

/// The `/NM` an annotation replies to.
struct Parent {
    name: String,
    /// A note reply (`inreplyto`) can't stand without its parent; a caret's link (`ppmIrt`) can.
    required: bool,
}

fn place_item(doc: &mut Document, idx: &mut AnnotIndex, pictures: &mut PictureCache, item: &RemoteItem) -> Result<Placed, String> {
    check_form_xml_depth(item.xfdf).map_err(|e| e.to_string())?;
    let xml = parse_form_xml(item.xfdf).map_err(|e| e.to_string())?;
    let root = xml.root_element();
    if root.tag_name().name() != "xfdf" {
        return Err("not an XFDF document".into());
    }
    let annots = root.children().find(|c| c.has_tag_name("annots")).ok_or("no annots element")?;
    let mut elements = annots.children().filter(|c| c.is_element());
    let (Some(node), None) = (elements.next(), elements.next()) else { return Err("annots must hold exactly one annotation".into()) };
    if node.attribute("name") != Some(item.annot_id) || item.annot_id.is_empty() {
        return Err("the annotation's name differs from its id".into());
    }
    let tag = node.tag_name().name().to_ascii_lowercase();
    let (subtype, _) = SUBTYPES.iter().find(|(_, t)| *t == tag).ok_or_else(|| format!("unknown annotation kind {tag}"))?;
    let page_no =
        node.attribute("page").and_then(|p| p.parse::<usize>().ok()).filter(|p| *p < idx.pages.len()).ok_or("the page is outside the document")?;
    let page_ref = idx.pages[page_no];
    let mut d = annot_from_xml(node, subtype, page_ref).ok_or("the annotation has no usable rectangle")?;
    d.set(OWNER_KEY.as_bytes().to_vec(), PdfString::text(item.author_id));
    // As in a file import: a redaction mark someone else made is always shown to the user.
    if d.name(b"Subtype") == Some(b"Redact".as_slice()) {
        let flags = d.int(b"F").unwrap_or(0);
        d.set(b"F".to_vec(), Object::Int(flags & !REDACT_UNSEEN));
    }

    let mut parent = node.attribute("inreplyto").filter(|p| !p.is_empty()).map(|name| Parent { name: name.into(), required: true });
    if let Some(custom) = read_custom(node)
        && let Some(irt) = apply_custom(&mut d, &custom)
    {
        parent = parent.or(Some(Parent { name: irt, required: false }));
    }
    let existing = locate(doc, idx, item.annot_id);
    if let Some(url) = node.children().find(|c| c.has_tag_name("imagedata")).and_then(|c| c.text()) {
        let reuse = existing.first().map(|s| s.obj);
        attach_picture(doc, &mut d, url.trim(), reuse, pictures)?;
    }

    // Replace in place (keeping the object, so replies that point at it stay linked), moving it
    // between pages when it moved. Duplicates of the name go.
    let mut obj = None;
    for (i, spot) in existing.iter().enumerate() {
        let old = doc.get(spot.obj).as_dict().cloned().unwrap_or_default();
        let popup = old.reference(b"Popup");
        // The first copy keeps its slot when it stays on its page; every other entry goes.
        let keep_here = i == 0 && spot.page_ref == page_ref;
        let kept: Vec<Object> = annots_of(doc, spot.page_ref)
            .into_iter()
            .filter(|e| e.as_ref().is_none_or(|r| popup != Some(r) && (r != spot.obj || keep_here)))
            .collect();
        doc.update_dict(spot.page_ref, |p| p.set(b"Annots".to_vec(), Object::Array(kept))).map_err(|e| e.to_string())?;
        if i == 0 {
            obj = Some(spot.obj);
        }
    }
    let r = match obj {
        Some(r) => {
            doc.set(r, Object::Dict(d));
            r
        }
        None => doc.add(Object::Dict(d)),
    };
    let mut list = annots_of(doc, page_ref);
    if !list.iter().any(|e| e.as_ref() == Some(r)) {
        list.push(Object::Ref(r));
        doc.update_dict(page_ref, |p| p.set(b"Annots".to_vec(), Object::Array(list))).map_err(|e| e.to_string())?;
    }
    if let Some(pop) = node.children().find(|c| c.has_tag_name("popup")) {
        add_popup(doc, page_ref, r, pop).map_err(|e| e.to_string())?;
    }
    idx.add(item.annot_id, page_no, r);
    // An annotation whose appearance can't be drawn stays (it is the user's data), undrawn.
    let _ = pdfcraft_annot::set_appearance(doc, r);
    Ok(Placed { obj: r, parent })
}

pub(crate) fn annots_of(doc: &Document, page: ObjRef) -> Vec<Object> {
    doc.get(page).as_dict().and_then(|p| p.get(b"Annots").cloned()).map(|a| doc.resolve(&a)).and_then(|a| a.as_array().cloned()).unwrap_or_default()
}

/// The `<trn-custom-data>` string map of an annotation element.
fn read_custom(node: roxmltree::Node) -> Option<Map<String, Value>> {
    let bytes = node.children().find(|c| c.has_tag_name("trn-custom-data"))?.attribute("bytes")?;
    match serde_json::from_str::<Value>(bytes).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// Put custom data into private keys (see the module table). Returns the `ppmIrt` parent.
fn apply_custom(d: &mut Dict, custom: &Map<String, Value>) -> Option<String> {
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut rest = Map::new();
    let mut parent = None;
    for (k, v) in custom {
        match k.as_str() {
            "ppm-stamp" | "ppm-signature" => d.set(STAMP_KEY.as_bytes().to_vec(), PdfString::text(&text(v))),
            "ppmSignRole" => d.set(SIGN_ROLE_KEY.as_bytes().to_vec(), PdfString::text(&text(v))),
            "ppmSigner" => d.set(SIGNER_KEY.as_bytes().to_vec(), PdfString::text(&text(v))),
            "ppmIrt" => parent = Some(text(v)),
            "ppmFork" => {
                if let Ok(Value::Object(fork)) = serde_json::from_str::<Value>(&text(v)) {
                    for (key, kind) in FORK_KEYS {
                        if let Some(o) = fork.get(key).and_then(|x| fork_object(x, kind)) {
                            d.set(key.as_bytes().to_vec(), o);
                        }
                    }
                }
            }
            _ => {
                rest.insert(k.clone(), v.clone());
            }
        }
    }
    if !rest.is_empty()
        && let Ok(json) = serde_json::to_string(&Value::Object(rest))
    {
        d.set(CUSTOM_KEY.as_bytes().to_vec(), PdfString::text(&json));
    }
    parent
}

fn hash_of(url: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    format!("{:016x}-{}", h.finish(), url.len())
}

/// Make `d` a picture stamp showing the data URL `url`. `reuse` is the object being replaced:
/// when it was made from the same URL its picture is kept instead of adding the image again.
/// Otherwise the picture is decoded once into `pictures` and copied from there.
fn attach_picture(doc: &mut Document, d: &mut Dict, url: &str, reuse: Option<ObjRef>, pictures: &mut PictureCache) -> Result<(), String> {
    let hash = hash_of(url);
    let old = reuse.and_then(|r| doc.get(r).as_dict().cloned());
    if let Some(old) = old.filter(|o| text_of(doc, o, PICTURE_HASH_KEY.as_bytes()).as_deref() == Some(&hash))
        && let Some(pic) = old.reference(b"PCPicture")
    {
        d.set(b"PCPicture".to_vec(), Object::Ref(pic));
        for key in [&b"PCPictureSize"[..], b"PCPictureImage", PICTURE_SOURCE_KEY.as_bytes()] {
            if let Some(v) = old.get(key) {
                d.set(key.to_vec(), v.clone());
            }
        }
        d.set(PICTURE_HASH_KEY.as_bytes().to_vec(), PdfString::text(&hash));
        return Ok(());
    }
    if !pictures.by_hash.contains_key(&hash) {
        let prepared = prepare_picture(url)?;
        pictures.by_hash.insert(hash.clone(), prepared);
    }
    let p = &pictures.by_hash[&hash];
    let mut copied = HashMap::new();
    let xobject = import_object(&p.scratch, doc, p.xobject, &mut copied);
    let source = import_object(&p.scratch, doc, p.source, &mut copied);
    d.set(PICTURE_SOURCE_KEY.as_bytes().to_vec(), Object::Ref(source));
    d.set(b"PCPicture".to_vec(), Object::Ref(xobject));
    d.set(b"PCPictureImage".to_vec(), Object::Bool(true));
    d.set(b"PCPictureSize".to_vec(), arr(&[p.size.0, p.size.1]));
    if d.get(b"Name").is_none() {
        d.set(b"Name".to_vec(), Object::name("PCCustomStamp"));
    }
    d.set(PICTURE_HASH_KEY.as_bytes().to_vec(), PdfString::text(&hash));
    Ok(())
}

/// The most pixels a remote picture may have.
const MAX_PICTURE_PIXELS: u64 = 16 * 1024 * 1024;

/// Decode a remote stamp picture (PNG or JPEG only) into a scratch document.
fn prepare_picture(url: &str) -> Result<Prepared, String> {
    let (header, data) = url.strip_prefix("data:").and_then(|u| u.split_once(',')).ok_or("the stamp picture is not a data URL")?;
    if !header.contains(";base64") {
        return Err("the stamp picture is not base64".into());
    }
    let clean: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() / 4 * 3 > MAX_PICTURE_BYTES {
        return Err("the stamp picture is too large".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(clean).map_err(|e| format!("the stamp picture: {e}"))?;
    // The server's items come from other users: only the two formats the stamp tools produce
    // reach the image decoders, and a picture's size is checked before it is decoded.
    if sniff(&bytes).is_none() {
        return Err("the stamp picture is neither PNG nor JPEG".into());
    }
    match pixel_size(&bytes) {
        Some((w, h)) if w > 0 && h > 0 && u64::from(w) * u64::from(h) <= MAX_PICTURE_PIXELS => {}
        _ => return Err("the stamp picture has too many pixels".into()),
    }
    let mut scratch = Document::new_empty();
    let (xobject, size) = pdfcraft_create::image_xobject(&mut scratch, "stamp.png", &bytes).map_err(|e| e.to_string())?;
    let source = scratch.add(Object::Stream(pdfcraft_cos::Stream::from_raw(Dict::new(), bytes)));
    Ok(Prepared { scratch, xobject, size, source })
}

/// Width and height from a PNG header or a JPEG frame header, without decoding.
///
/// Decoders disagree about odd JPEG byte layouts (bare or length-carrying `FF01`/`RSTn`,
/// `FF00`, junk between segments), and a picture that two of them read differently can show a
/// small frame here and a huge one to the decoder that draws it. So the JPEG walk is strict:
/// before the scan only well-formed segments are allowed, anything else is refused, and the
/// size must also be what the decoder itself reports.
pub(crate) fn pixel_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        if b.get(12..16)? != b"IHDR" {
            return None;
        }
        let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
        let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
        return Some((w, h));
    }
    let size = jpeg_frame_size(b)?;
    // The decoder's own reading of the same bytes must agree.
    let options = zune_jpeg::zune_core::options::DecoderOptions::default().set_max_width(65535).set_max_height(65535);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(zune_jpeg::zune_core::bytestream::ZCursor::new(b), options);
    dec.decode_headers().ok()?;
    let (w, h) = dec.dimensions()?;
    (u32::try_from(w).ok()? == size.0 && u32::try_from(h).ok()? == size.1).then_some(size)
}

/// The one frame header of a JPEG whose segments up to the scan are all well formed.
fn jpeg_frame_size(b: &[u8]) -> Option<(u32, u32)> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut size = None;
    let mut i = 2;
    loop {
        // Every segment starts right after the previous one: no junk, no fill bytes.
        if *b.get(i)? != 0xFF {
            return None;
        }
        let marker = *b.get(i + 1)?;
        match marker {
            0xDA => return size,
            // Markers with no length (or that readers treat as having one), and a repeated SOI.
            0x00 | 0x01 | 0xD0..=0xD9 | 0xFF => return None,
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]));
        if len < 2 {
            return None;
        }
        let end = (i + 2).checked_add(len)?;
        let seg = b.get(i + 4..end)?;
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if size.is_some() || seg.len() < 6 {
                return None;
            }
            size = Some((u32::from(u16::from_be_bytes([seg[3], seg[4]])), u32::from(u16::from_be_bytes([seg[1], seg[2]]))));
        }
        i = end;
    }
}

/// Copy object `r` of `from` into `to` with everything it points to; the byte buffers are shared.
pub(crate) fn import_object(from: &Document, to: &mut Document, r: ObjRef, copied: &mut HashMap<ObjRef, ObjRef>) -> ObjRef {
    if let Some(done) = copied.get(&r) {
        return *done;
    }
    // The number is taken before the walk, so a reference back to `r` finds it instead of looping.
    let new = to.add(Object::Null);
    copied.insert(r, new);
    let obj = remap(from, to, (*from.get(r)).clone(), copied);
    to.set(new, obj);
    new
}

fn remap(from: &Document, to: &mut Document, obj: Object, copied: &mut HashMap<ObjRef, ObjRef>) -> Object {
    match obj {
        Object::Ref(r) => Object::Ref(import_object(from, to, r, copied)),
        Object::Array(items) => Object::Array(items.into_iter().map(|o| remap(from, to, o, copied)).collect()),
        Object::Dict(d) => Object::Dict(remap_dict(from, to, d, copied)),
        Object::Stream(s) => Object::Stream(pdfcraft_cos::Stream { dict: remap_dict(from, to, s.dict, copied), raw: s.raw }),
        other => other,
    }
}

fn remap_dict(from: &Document, to: &mut Document, d: Dict, copied: &mut HashMap<ObjRef, ObjRef>) -> Dict {
    let mut out = Dict::new();
    for (k, v) in d.iter() {
        out.set(k.clone(), remap(from, to, v.clone(), copied));
    }
    out
}
