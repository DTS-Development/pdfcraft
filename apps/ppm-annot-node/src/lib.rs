//! The PPM server's view of a PDF's annotations, for Node (`wasm-bindgen --target nodejs`).
//!
//! Six functions, bytes as `Uint8Array` and everything else as JSON text. Pages are 1-based
//! everywhere here (as in the list); `index` is the entry's place in that page's `/Annots` array,
//! every subtype counted, so annotations without `/NM` are addressed the same way.
//!
//! * `list(pdf)` → `[{page, index, subtype, nm, title, irt: {page, index} | null, exportable}]`
//! * `exportXfdf(pdf, [{page, index, name, inReplyTo}])` → `[{name, xfdf} | {name, error}]`
//! * `strip(pdf, [{page, index}])` → PDF without those annotations and their pop-ups
//! * `merge(pdf, [{annotId, authorId, xfdf}])` → PDF with them added, appearances drawn
//! * `flatten(pdf)` → PDF with every comment and form field baked into the pages
//! * `extractText(pdf, options)` → `[{page, width, height, rotation, cropBox, lines: [{text, bbox, words?, glyphs?}]}]`,
//!   boxes in the editor's page view space (points, top-left origin, y down, `/Rotate` and crop box applied)
//!
//! The logic lives in plain functions ([`list_json`] …) so that it is testable on the host; the
//! `#[wasm_bindgen]` functions only convert errors.

use std::sync::Arc;

use pdfcraft_cos::{Document, SaveOptions, write_full};
use pdfcraft_edit::{EditError, flatten_export};
use pdfcraft_render::{ExtractError, OpenError, PageText, extract_pages_each};
use pdfcraft_xfdf::{ExportRequest, RemoteItem, apply_remote, export_positions, list_annotations, strip_positions};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

fn open(pdf: Vec<u8>) -> Result<Document, String> {
    Document::open(Arc::new(pdf)).map_err(|e| format!("the PDF can't be opened: {e}"))
}

fn save(doc: &Document) -> Result<Vec<u8>, String> {
    write_full(doc, &SaveOptions::default()).map_err(|e| format!("the PDF can't be saved: {e}"))
}

#[derive(Serialize)]
struct Position {
    page: usize,
    index: usize,
}

#[derive(Serialize)]
struct ListEntry {
    page: usize,
    index: usize,
    subtype: String,
    nm: Option<String>,
    title: Option<String>,
    irt: Option<Position>,
    exportable: bool,
}

pub fn list_json(pdf: Vec<u8>) -> Result<String, String> {
    let doc = open(pdf)?;
    let list: Vec<ListEntry> = list_annotations(&doc)
        .into_iter()
        .map(|e| ListEntry {
            page: e.page + 1,
            index: e.index,
            subtype: e.subtype,
            nm: e.nm,
            title: e.title,
            irt: e.irt.map(|(page, index)| Position { page: page + 1, index }),
            exportable: e.exportable,
        })
        .collect();
    serde_json::to_string(&list).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportIn {
    page: usize,
    index: usize,
    name: String,
    #[serde(default)]
    in_reply_to: Option<String>,
}

#[derive(Serialize)]
struct ExportOut {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    xfdf: Option<String>,
    /// Why this one could not be exported (the rest still are).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 1-based page → zero-based; 0 stays out of range so it is reported as missing.
fn page0(page: usize) -> usize {
    page.wrapping_sub(1)
}

pub fn export_json(pdf: Vec<u8>, requests: &str) -> Result<String, String> {
    let input: Vec<ExportIn> = serde_json::from_str(requests).map_err(|e| format!("bad export request: {e}"))?;
    let doc = open(pdf)?;
    let reqs: Vec<ExportRequest> =
        input.iter().map(|r| ExportRequest { page: page0(r.page), index: r.index, name: &r.name, in_reply_to: r.in_reply_to.as_deref() }).collect();
    let out: Vec<ExportOut> = export_positions(&doc, &reqs)
        .into_iter()
        .zip(&input)
        .map(|(res, r)| match res {
            Ok(item) => ExportOut { name: r.name.clone(), xfdf: Some(item.xfdf), error: None },
            Err(e) => ExportOut { name: r.name.clone(), xfdf: None, error: Some(e.to_string()) },
        })
        .collect();
    serde_json::to_string(&out).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct StripIn {
    page: usize,
    index: usize,
}

pub fn strip_pdf(pdf: Vec<u8>, targets: &str) -> Result<Vec<u8>, String> {
    let input: Vec<StripIn> = serde_json::from_str(targets).map_err(|e| format!("bad strip request: {e}"))?;
    if input.is_empty() {
        return Ok(pdf);
    }
    let mut doc = open(pdf)?;
    let at: Vec<(usize, usize)> = input.iter().map(|t| (page0(t.page), t.index)).collect();
    strip_positions(&mut doc, &at).map_err(|e| e.to_string())?;
    save(&doc)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeIn {
    annot_id: String,
    author_id: String,
    xfdf: String,
}

pub fn merge_pdf(pdf: Vec<u8>, items: &str) -> Result<Vec<u8>, String> {
    let input: Vec<MergeIn> = serde_json::from_str(items).map_err(|e| format!("bad merge request: {e}"))?;
    if input.is_empty() {
        return Ok(pdf);
    }
    let mut doc = open(pdf)?;
    let remote: Vec<RemoteItem> = input.iter().map(|i| RemoteItem { annot_id: &i.annot_id, author_id: &i.author_id, xfdf: &i.xfdf }).collect();
    let report = apply_remote(&mut doc, &remote, &[]);
    // Leaving an annotation out silently would lose it from the export: fail with every reason.
    if !report.failed.is_empty() {
        let why: Vec<String> = report.failed.iter().map(|(id, reason)| format!("{id}: {reason}")).collect();
        return Err(format!("annotations could not be merged: {}", why.join("; ")));
    }
    save(&doc)
}

/// Prefix of the error when flattening is refused because the document has too many annotations
/// (nothing is changed then); the caller tells this apart from a broken PDF by it.
pub const FLATTEN_REFUSED: &str = "flatten refused: ";

/// Bakes every comment and form field into the page content, as the /viewer export does with
/// `FlattenAnnotations` (NoView annotations and link borders included: `flatten_export`). This goes to the crate function, not through the engine: the engine's
/// ownership guard protects other people's comments from a user's edit, while this runs on the
/// server because the administrator set the integration to flatten its exports, whoever wrote them.
pub fn flatten_pdf(pdf: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut doc = open(pdf)?;
    let pages: Vec<usize> = (0..pdfcraft_model::pages(&doc).len()).collect();
    match flatten_export(&mut doc, &pages) {
        Ok(_) => {}
        // The crate refuses a document over its annotation limit before changing anything.
        Err(EditError::Invalid(why)) => return Err(format!("{FLATTEN_REFUSED}{why}")),
        Err(e) => return Err(format!("the PDF can't be flattened: {e}")),
    }
    save(&doc)
}

/// Default for `maxGlyphs`: bounds the memory of one call (a page of text is a few thousand glyphs).
pub const DEFAULT_MAX_GLYPHS: usize = 2_000_000;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtractOptions {
    /// 1-based pages; all when absent.
    pages: Option<Vec<usize>>,
    /// `line` (default), `word` (lines with words) or `glyph` (lines with words and glyphs).
    granularity: Option<String>,
    max_glyphs: Option<usize>,
}

#[derive(Serialize)]
struct TextBox {
    text: String,
    bbox: [f32; 4],
}

#[derive(Serialize)]
struct TextLine {
    text: String,
    bbox: [f32; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    words: Option<Vec<TextBox>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    glyphs: Option<Vec<TextBox>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PageTextOut {
    page: usize,
    width: f32,
    height: f32,
    rotation: u16,
    /// The effective crop box in PDF user space [x0, y0, x1, y1] (origin bottom-left, unrotated).
    crop_box: [f32; 4],
    lines: Vec<TextLine>,
}

fn union(boxes: impl Iterator<Item = [f32; 4]>) -> Option<[f32; 4]> {
    boxes.reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
}

/// Lines of a page in reading order: a line is a run of glyphs with one `line_of`; lines with no
/// visible text (only blanks) are dropped. Words break at the layout's word gaps and at blanks.
fn lines_of(text: &PageText, words: bool, glyphs: bool) -> Vec<TextLine> {
    let n = text.glyphs.len();
    let blank = |i: usize| text.glyphs[i].text.trim().is_empty();
    let mut lines = Vec::new();
    let mut start = 0;
    while start < n {
        let mut end = start + 1;
        while end < n && text.line_of[end] == text.line_of[start] {
            end += 1;
        }
        let visible: Vec<usize> = (start..end).filter(|&i| !blank(i)).collect();
        if let Some(bbox) = union(visible.iter().map(|&i| text.glyphs[i].rect)) {
            let boxed = |i: usize| TextBox { text: text.glyphs[i].text.clone(), bbox: text.glyphs[i].rect };
            let word_boxes = words.then(|| {
                let mut out: Vec<TextBox> = Vec::new();
                let mut open: Vec<usize> = Vec::new();
                let mut flush = |open: &mut Vec<usize>| {
                    if let Some(bbox) = union(open.iter().map(|&i| text.glyphs[i].rect)) {
                        out.push(TextBox { text: open.iter().map(|&i| text.glyphs[i].text.as_str()).collect(), bbox });
                    }
                    open.clear();
                };
                for i in start..end {
                    if blank(i) {
                        flush(&mut open);
                        continue;
                    }
                    if i > start && text.space_before[i] {
                        flush(&mut open);
                    }
                    open.push(i);
                }
                flush(&mut open);
                out
            });
            lines.push(TextLine {
                text: text.text_of(start..end).trim().to_string(),
                bbox,
                words: word_boxes,
                glyphs: glyphs.then(|| visible.iter().map(|&i| boxed(i)).collect()),
            });
        }
        start = end;
    }
    lines
}

pub fn extract_text_json(pdf: Vec<u8>, options: &str) -> Result<String, String> {
    let opts: ExtractOptions = if options.trim().is_empty() {
        ExtractOptions::default()
    } else {
        serde_json::from_str(options).map_err(|e| format!("bad extract options: {e}"))?
    };
    let (words, glyphs) = match opts.granularity.as_deref() {
        None | Some("line") => (false, false),
        Some("word") => (true, false),
        Some("glyph") => (true, true),
        Some(other) => return Err(format!("bad extract options: granularity must be line, word or glyph, not {other:?}")),
    };
    // Pages are 1-based.
    if opts.pages.as_ref().is_some_and(|p| p.contains(&0)) {
        return Err("bad extract options: pages are numbered from 1".into());
    }
    let pages: Option<Vec<usize>> = opts.pages.as_ref().map(|p| p.iter().map(|&n| page0(n)).collect());
    // Each page becomes its JSON right away and its text is dropped, so the peak is the output
    // plus one page instead of every page's glyphs and a second copy of them.
    let mut json = b"[".to_vec();
    let mut first = true;
    extract_pages_each(Arc::new(pdf), None, pages.as_deref(), opts.max_glyphs.unwrap_or(DEFAULT_MAX_GLYPHS), |p| {
        let out = PageTextOut {
            page: p.index + 1,
            width: p.info.width,
            height: p.info.height,
            rotation: p.info.rotation,
            crop_box: p.info.crop,
            lines: lines_of(&p.text, words, glyphs),
        };
        if !first {
            json.push(b',');
        }
        first = false;
        serde_json::to_writer(&mut json, &out).map_err(|e| ExtractError::Page(p.index + 1, e.to_string()))
    })
    .map_err(|e| match e {
        ExtractError::Open(OpenError::NeedsPassword) => "the PDF is password-protected".to_string(),
        ExtractError::Open(e) => format!("the PDF can't be opened: {e}"),
        other => other.to_string(),
    })?;
    json.push(b']');
    String::from_utf8(json).map_err(|e| e.to_string())
}

#[wasm_bindgen]
pub fn list(pdf: Vec<u8>) -> Result<String, JsError> {
    list_json(pdf).map_err(|e| JsError::new(&e))
}

#[wasm_bindgen(js_name = exportXfdf)]
pub fn export_xfdf(pdf: Vec<u8>, requests: &str) -> Result<String, JsError> {
    export_json(pdf, requests).map_err(|e| JsError::new(&e))
}

#[wasm_bindgen]
pub fn strip(pdf: Vec<u8>, targets: &str) -> Result<Vec<u8>, JsError> {
    strip_pdf(pdf, targets).map_err(|e| JsError::new(&e))
}

#[wasm_bindgen]
pub fn merge(pdf: Vec<u8>, items: &str) -> Result<Vec<u8>, JsError> {
    merge_pdf(pdf, items).map_err(|e| JsError::new(&e))
}

#[wasm_bindgen(js_name = flatten)]
pub fn flatten_bytes(pdf: Vec<u8>) -> Result<Vec<u8>, JsError> {
    flatten_pdf(pdf).map_err(|e| JsError::new(&e))
}

#[wasm_bindgen(js_name = extractText)]
pub fn extract_text(pdf: Vec<u8>, options: &str) -> Result<String, JsError> {
    extract_text_json(pdf, options).map_err(|e| JsError::new(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        assert!(list_json(b"not a pdf".to_vec()).is_err());
        assert!(export_json(Vec::new(), "[]").is_err());
        assert!(strip_pdf(b"x".to_vec(), "not json").unwrap_err().contains("bad strip request"));
        assert!(merge_pdf(b"x".to_vec(), "{}").unwrap_err().contains("bad merge request"));
        assert!(flatten_pdf(b"not a pdf".to_vec()).is_err());
    }

    #[test]
    fn nothing_to_do_returns_the_input() {
        assert_eq!(strip_pdf(b"anything".to_vec(), "[]").unwrap(), b"anything");
        assert_eq!(merge_pdf(b"anything".to_vec(), "[]").unwrap(), b"anything");
    }

    /// A one-page PDF whose /Annots hold a link and a square without an appearance.
    fn sample() -> Vec<u8> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Annots [4 0 R 5 0 R] >>",
            "<< /Type /Annot /Subtype /Link /Rect [10 10 50 30] /Border [0 0 0] >>",
            "<< /Type /Annot /Subtype /Square /Rect [200 200 300 260] /C [1 0 0] /F 4 /BS << /W 2 >> >>",
        ];
        let mut out = String::from("%PDF-1.7\n");
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out += &format!("{} 0 obj\n{o}\nendobj\n", i + 1);
        }
        let xref = out.len();
        out += &format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
        for o in offs {
            out += &format!("{o:010} 00000 n \n");
        }
        out += &format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1);
        out.into_bytes()
    }

    #[test]
    fn flatten_bakes_comments_and_keeps_links() {
        let out = flatten_pdf(sample()).unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_str(&list_json(out).unwrap()).unwrap();
        let kinds: Vec<&str> = list.iter().filter_map(|e| e["subtype"].as_str()).collect();
        assert_eq!(kinds, ["Link"]);
    }

    /// A one-page PDF drawing two lines (the second with a gap that makes two words).
    fn text_sample(rotate: i32) -> Vec<u8> {
        let content = "BT /F1 20 Tf 50 150 Td (Hello world) Tj 0 -30 Td (Second line) Tj ET";
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] /Rotate {rotate} /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            ),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut out = String::from("%PDF-1.7\n");
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out += &format!("{} 0 obj\n{o}\nendobj\n", i + 1);
        }
        let xref = out.len();
        out += &format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
        for o in offs {
            out += &format!("{o:010} 00000 n \n");
        }
        out += &format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1);
        out.into_bytes()
    }

    fn extract(options: &str, rotate: i32) -> serde_json::Value {
        serde_json::from_str(&extract_text_json(text_sample(rotate), options).unwrap()).unwrap()
    }

    #[test]
    fn extracts_lines_by_default() {
        let v = extract("", 0);
        let page = &v[0];
        assert_eq!((page["page"].as_u64(), page["width"].as_f64(), page["rotation"].as_u64()), (Some(1), Some(400.0), Some(0)));
        assert_eq!(page["cropBox"], serde_json::json!([0.0, 0.0, 400.0, 200.0]));
        let texts: Vec<&str> = page["lines"].as_array().unwrap().iter().map(|l| l["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["Hello world", "Second line"]);
        assert!(page["lines"][0].get("words").is_none() && page["lines"][0].get("glyphs").is_none());
        // y down: the first line (baseline user y = 150, 50 pt from the top) is above the second.
        let (a, b) = (page["lines"][0]["bbox"][3].as_f64().unwrap(), page["lines"][1]["bbox"][3].as_f64().unwrap());
        assert!(a < b && (a - 50.0).abs() < 6.0, "{a} {b}");
    }

    #[test]
    fn words_and_glyphs_are_opt_in() {
        let v = extract(r#"{"granularity":"word"}"#, 0);
        let words: Vec<&str> = v[0]["lines"][0]["words"].as_array().unwrap().iter().map(|w| w["text"].as_str().unwrap()).collect();
        assert_eq!(words, ["Hello", "world"]);
        assert!(v[0]["lines"][0].get("glyphs").is_none());
        let v = extract(r#"{"granularity":"glyph"}"#, 0);
        assert_eq!(v[0]["lines"][0]["glyphs"].as_array().unwrap().len(), "Helloworld".len());
    }

    #[test]
    fn rotated_pages_report_the_displayed_size() {
        let v = extract("", 90);
        assert_eq!((v[0]["width"].as_f64(), v[0]["height"].as_f64(), v[0]["rotation"].as_u64()), (Some(200.0), Some(400.0), Some(90)));
    }

    #[test]
    fn extract_options_are_checked() {
        let pdf = text_sample(0);
        assert!(extract_text_json(pdf.clone(), "{").unwrap_err().contains("bad extract options"));
        assert!(extract_text_json(pdf.clone(), r#"{"granularity":"x"}"#).unwrap_err().contains("granularity"));
        assert!(extract_text_json(pdf.clone(), r#"{"typo":1}"#).unwrap_err().contains("bad extract options"));
        assert!(extract_text_json(pdf.clone(), r#"{"pages":[2]}"#).unwrap_err().contains("page 2 does not exist"));
        assert!(extract_text_json(pdf.clone(), r#"{"pages":[0]}"#).is_err());
        assert!(extract_text_json(pdf.clone(), r#"{"maxGlyphs":5}"#).unwrap_err().contains("more than 5 glyphs"));
        assert_eq!(extract_text_json(pdf, r#"{"pages":[1,1]}"#).map(|s| s.len() > 2), Ok(true));
        assert!(extract_text_json(b"not a pdf".to_vec(), "").is_err());
    }
}
