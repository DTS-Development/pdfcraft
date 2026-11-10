//! Public entry point for server-side text extraction: bytes in, per-page text with geometry out.
//!
//! The editor reads page text through `PageRenderer` (`RequestKind::Text`); a server module has no
//! renderer pool, so this opens the document itself and runs the same extractor
//! ([`crate::text`]) on the requested pages. Coordinates are therefore the editor's page view
//! space: points, origin at the top-left of the displayed page, y down, `/Rotate` and the
//! effective crop box applied. [`crate::PageInfo::view_to_user`] converts back to PDF user space.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hayro::hayro_interpret::{InterpreterSettings, content_byte_budget_left, set_content_byte_budget};
use hayro::hayro_syntax::page::Rotation;
use hayro::hayro_syntax::{DecryptionError, LoadPdfError, Pdf};

use crate::raster::panic_message;
use crate::text::{ARRANGE_PAGE_WORK, PageStop, PageText, extract_page_limited, take_arrange_used};
use crate::{OpenError, PageInfo};

/// The text of one page and where the page sits.
#[derive(Clone, Debug)]
pub struct ExtractedPage {
    /// 0-based page index.
    pub index: usize,
    /// Displayed size, effective crop box and rotation: the inputs of the view ↔ user conversion.
    pub info: PageInfo,
    pub text: PageText,
}

/// Why extraction stopped.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error("page {0} does not exist (the document has {1} pages)")]
    NoSuchPage(usize, usize),
    #[error("page {0}'s text could not be read: {1}")]
    Page(usize, String),
    #[error("the text has more than {0} glyphs")]
    TooManyGlyphs(usize),
    /// The page ran more content operators, or read more content bytes, than one page may.
    #[error("page {0} needs more work to read than a page may take")]
    TooMuchWork(usize),
    /// The pages together passed what one call may do; the number is the page it stopped on.
    #[error("the document needs more work to read than one call may do (the limit was passed on page {0})")]
    TooMuchWorkInCall(usize),
}

/// Content operators one page may run; every page starts with the whole allowance, so the error
/// names the page that went over. (The pages together are held to [`CALL_OPS`], so a long document
/// of very dense pages can still be refused for its total.) An ordinary page runs a few hundred to a few hundred thousand; a large drawing or map
/// page a few million (a path is 3 to 5 operators), so 10 million leaves such a page room. A page
/// of 10 million operators takes about a second to turn away, and a file's cost is bounded by its
/// page count times that.
const PAGE_OPS: usize = 10_000_000;
/// Content operators all the pages of one call may run together. Pages that share one heavy
/// content stream each stay under [`PAGE_OPS`], so hundreds of them filled just below it would
/// still hold the caller for minutes. 20 million is a few seconds of work at most (about 0.17
/// microseconds an operator in the Node module) and four times what a real 937-page file of
/// 880,000 glyphs runs (4.8 million, the largest of the files measured; its busiest page ran
/// 15,000). The page that passes it is the one named.
const CALL_OPS: usize = 20_000_000;
/// Content bytes (inflated or read from the file) one page, and a whole call, may decode and scan.
/// A form painted over and over is decoded each time, so its size counts each time; the operator
/// count alone wouldn't see it (a 1 MB blank form painted 50,000 times is a few operators).
/// Reading 1 GB takes about a second.
const PAGE_BYTES: usize = 256 << 20;
const CALL_BYTES: usize = 1 << 30;
/// Reading-order layout work one call may do in all its pages together, in the units of
/// `ARRANGE_PAGE_WORK`. About a billion units take a second. The real 937-page document spends 21
/// million in all (its busiest page 0.9 million); a page of thousands of pieces spends 75-94 million,
/// so a few hundred such pages, which share one content stream, end the call here instead of after
/// half a minute.
const ARRANGE_CALL_WORK: usize = 500_000_000;

/// The work one extraction may do: operators and content bytes, per page and per call.
#[derive(Clone, Copy)]
struct Limits {
    page_ops: usize,
    call_ops: usize,
    page_bytes: usize,
    call_bytes: usize,
    page_arrange: usize,
    call_arrange: usize,
}

const LIMITS: Limits = Limits { page_ops: PAGE_OPS, call_ops: CALL_OPS, page_bytes: PAGE_BYTES, call_bytes: CALL_BYTES, page_arrange: ARRANGE_PAGE_WORK, call_arrange: ARRANGE_CALL_WORK };

/// Extract the text of `pages` (0-based indices; `None` is every page), in ascending page order
/// without duplicates. `max_glyphs` bounds the glyphs of all the returned pages together: the
/// call fails instead of returning a truncated text. A password-protected document fails with
/// [`OpenError::NeedsPassword`] (or [`OpenError::WrongPassword`] when `password` is wrong).
///
/// Every page is kept until the end; [`extract_pages_each`] hands pages over one by one instead.
pub fn extract_pages(
    bytes: Arc<Vec<u8>>,
    password: Option<&str>,
    pages: Option<&[usize]>,
    max_glyphs: usize,
) -> Result<Vec<ExtractedPage>, ExtractError> {
    let mut out = Vec::new();
    extract_pages_each(bytes, password, pages, max_glyphs, |page| {
        out.push(page);
        Ok(())
    })?;
    Ok(out)
}

/// [`extract_pages`] that gives each page to `each` as soon as it is read, so only one page's text
/// is alive at a time: the caller turns it into its output and drops it. An error from `each`
/// ends the call. `max_glyphs` and the work limit are checked while a page is being read, not
/// after: the page that passes either is stopped at once with [`ExtractError::TooManyGlyphs`] or
/// [`ExtractError::TooMuchWork`].
pub fn extract_pages_each(
    bytes: Arc<Vec<u8>>,
    password: Option<&str>,
    pages: Option<&[usize]>,
    max_glyphs: usize,
    each: impl FnMut(ExtractedPage) -> Result<(), ExtractError>,
) -> Result<(), ExtractError> {
    extract_pages_limited(bytes, password, pages, max_glyphs, LIMITS, each)
}

/// [`extract_pages_each`] with the work limits given.
fn extract_pages_limited(
    bytes: Arc<Vec<u8>>,
    password: Option<&str>,
    pages: Option<&[usize]>,
    max_glyphs: usize,
    limits: Limits,
    mut each: impl FnMut(ExtractedPage) -> Result<(), ExtractError>,
) -> Result<(), ExtractError> {
    let pdf = catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(bytes, password.unwrap_or(""))))
        .map_err(|p| OpenError::Invalid(format!("the parser crashed: {}", panic_message(&p))))?
        .map_err(|e| match e {
            LoadPdfError::Decryption(DecryptionError::PasswordProtected) if password.is_some() => OpenError::WrongPassword,
            LoadPdfError::Decryption(DecryptionError::PasswordProtected) => OpenError::NeedsPassword,
            LoadPdfError::Decryption(other) => OpenError::Unsupported(format!("encryption not supported: {other:?}")),
            LoadPdfError::Invalid => OpenError::Invalid("no readable page tree or cross-reference data was found".into()),
        })?;
    let count = catch_unwind(AssertUnwindSafe(|| pdf.pages().len()))
        .map_err(|p| OpenError::Invalid(format!("the page tree can't be read: {}", panic_message(&p))))?;
    let wanted: Vec<usize> = match pages {
        Some(list) => list.iter().copied().collect::<BTreeSet<_>>().into_iter().collect(),
        None => (0..count).collect(),
    };
    if let Some(&bad) = wanted.iter().find(|&&p| p >= count) {
        return Err(ExtractError::NoSuchPage(bad.saturating_add(1), count));
    }
    let settings = InterpreterSettings::default();
    let mut glyphs = 0usize;
    let mut ops_left = limits.call_ops;
    let mut bytes_left = limits.call_bytes;
    let mut arrange_left = limits.call_arrange;
    for index in wanted {
        // The page may use what is left of the glyph room, and its own work allowance or what is
        // left of the call's, whichever is less.
        let allowed = limits.page_ops.min(ops_left);
        let allowed_bytes = limits.page_bytes.min(bytes_left);
        let allowed_arrange = limits.page_arrange.min(arrange_left);
        let call_limited = allowed < limits.page_ops || allowed_bytes < limits.page_bytes || allowed_arrange < limits.page_arrange;
        let ops = Arc::new(AtomicUsize::new(allowed));
        set_content_byte_budget(Some(allowed_bytes));
        // The interpreter can panic on a damaged page: report it as that page's error.
        let done = catch_unwind(AssertUnwindSafe(|| {
            let info = page_info(&pdf, index);
            extract_page_limited(&pdf, index, &settings, Default::default(), Some((max_glyphs - glyphs, ops.clone(), allowed_arrange))).map(|text| (info, text))
        }));
        // The limit lives on this thread: lift it whatever happened.
        let arrange_used = take_arrange_used();
        let bytes_now = content_byte_budget_left().unwrap_or(0);
        set_content_byte_budget(None);
        let done = done.map_err(|p| ExtractError::Page(index + 1, panic_message(&p)))?;
        let work = |page: usize| if call_limited { ExtractError::TooMuchWorkInCall(page) } else { ExtractError::TooMuchWork(page) };
        // A page that ran out of bytes was read without the content it couldn't decode.
        if bytes_now == 0 {
            return Err(work(index + 1));
        }
        let (info, text) = done.map_err(|stop| match stop {
            PageStop::Failed(why) => ExtractError::Page(index + 1, why),
            PageStop::TooManyGlyphs => ExtractError::TooManyGlyphs(max_glyphs),
            PageStop::TooMuchWork => work(index + 1),
        })?;
        arrange_left = arrange_left.saturating_sub(arrange_used);
        ops_left -= allowed - ops.load(Ordering::Relaxed);
        bytes_left -= allowed_bytes - bytes_now;
        glyphs = glyphs.saturating_add(text.glyphs.len());
        if glyphs > max_glyphs {
            return Err(ExtractError::TooManyGlyphs(max_glyphs));
        }
        each(ExtractedPage { index, info, text })?;
    }
    Ok(())
}

/// Geometry of page `index`, as [`crate::inspect`] reports it (a degenerate size gets a letter-size
/// placeholder so the conversions never divide by zero).
fn page_info(pdf: &Pdf, index: usize) -> PageInfo {
    let pages = pdf.pages();
    let page = &pages[index];
    let (w, h) = page.render_dimensions();
    let c = page.intersected_crop_box();
    let rotation = match page.rotation() {
        Rotation::None => 0,
        Rotation::Horizontal => 90,
        Rotation::Flipped => 180,
        Rotation::FlippedHorizontal => 270,
    };
    let (width, height) = if w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0 { (w, h) } else { (612.0, 792.0) };
    PageInfo { width, height, label: (index + 1).to_string(), crop: [c.x0 as f32, c.y0 as f32, c.x1 as f32, c.y1 as f32], rotation }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Document, Object, Stream, dictionary};

    /// A one-page PDF with /Helvetica text from `ops` and extra page entries (`/Rotate`, `/CropBox`).
    fn pdf(media: [i64; 4], extra: Vec<(&str, Object)>, ops: &str) -> Arc<Vec<u8>> {
        let mut doc = Document::with_version("1.7");
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let content = doc.add_object(Stream::new(dictionary! {}, ops.as_bytes().to_vec()));
        let pages = doc.new_object_id();
        let mut page = dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => media.iter().map(|&n| Object::Integer(n)).collect::<Vec<_>>(),
            "Resources" => dictionary! {"Font" => dictionary! {"F1" => font}}, "Contents" => content
        };
        for (k, v) in extra {
            page.set(k, v);
        }
        let page = doc.add_object(page);
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        Arc::new(bytes)
    }

    fn ints(v: [i64; 4]) -> Object {
        Object::Array(v.iter().map(|&n| Object::Integer(n)).collect())
    }

    fn one(bytes: Arc<Vec<u8>>) -> ExtractedPage {
        extract_pages(bytes, None, None, usize::MAX).unwrap().remove(0)
    }

    const HELLO: &str = "BT /F1 20 Tf 50 150 Td (Hello) Tj ET";

    /// The centre of the glyphs' union, converted back to user space, lies on the baseline the
    /// text was drawn at (x = 50 .. 50 + width, y = 150 .. 150 + ascent).
    fn user_rect(p: &ExtractedPage) -> [f32; 4] {
        let r = p.text.glyphs.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |a, g| {
            [a[0].min(g.rect[0]), a[1].min(g.rect[1]), a[2].max(g.rect[2]), a[3].max(g.rect[3])]
        });
        p.info.view_rect_to_user(r)
    }

    fn assert_near_hello(r: [f32; 4]) {
        // "Hello" in 20 pt Helvetica starts at user x = 50 and is about 45 pt wide; the baseline is y = 150.
        assert!((r[0] - 50.0).abs() < 2.0, "x0 {r:?}");
        assert!((r[2] - 95.0).abs() < 6.0, "x1 {r:?}");
        assert!(r[1] < 150.0 + 0.5 && r[1] > 150.0 - 8.0, "y0 {r:?}");
        assert!(r[3] > 150.0 + 10.0 && r[3] < 150.0 + 30.0, "y1 {r:?}");
    }

    #[test]
    fn upright_page_is_top_left_y_down() {
        let p = one(pdf([0, 0, 400, 200], vec![], HELLO));
        assert_eq!((p.info.width, p.info.height, p.info.rotation), (400.0, 200.0, 0));
        assert_eq!(p.text.plain_text(), "Hello");
        // Baseline at user y = 150 on a 200 pt page: 50 pt from the top.
        let g = &p.text.glyphs[0];
        assert!((g.rect[0] - 50.0).abs() < 2.0 && g.rect[1] < 50.0 && g.rect[3] > 45.0 && g.rect[3] < 55.0, "{:?}", g.rect);
        assert_near_hello(user_rect(&p));
    }

    #[test]
    fn rotated_pages_swap_the_view_size_and_convert_back() {
        for rot in [90, 180, 270] {
            let p = one(pdf([0, 0, 400, 200], vec![("Rotate", Object::Integer(rot))], HELLO));
            let (w, h) = if rot == 180 { (400.0, 200.0) } else { (200.0, 400.0) };
            assert_eq!((p.info.width, p.info.height, p.info.rotation as i64), (w, h, rot), "rotate {rot}");
            assert_eq!(p.text.plain_text(), "Hello", "rotate {rot}");
            assert_near_hello(user_rect(&p));
        }
    }

    #[test]
    fn a_crop_box_moves_the_origin() {
        let p = one(pdf([0, 0, 400, 300], vec![("CropBox", ints([20, 40, 320, 240]))], HELLO));
        assert_eq!((p.info.width, p.info.height), (300.0, 200.0));
        assert_eq!(p.info.crop, [20.0, 40.0, 320.0, 240.0]);
        // x = 50 is 30 pt right of the crop's left edge; the baseline y = 150 is 90 pt below its top (240).
        let g = &p.text.glyphs[0];
        assert!((g.rect[0] - 30.0).abs() < 2.0 && (g.rect[3] - 90.0).abs() < 5.0, "{:?}", g.rect);
        assert_near_hello(user_rect(&p));
    }

    #[test]
    fn rotation_and_crop_box_together() {
        let p = one(pdf([0, 0, 400, 300], vec![("CropBox", ints([20, 40, 320, 240])), ("Rotate", Object::Integer(90))], HELLO));
        assert_eq!((p.info.width, p.info.height), (200.0, 300.0));
        assert_near_hello(user_rect(&p));
    }

    #[test]
    fn pages_are_chosen_and_checked() {
        let bytes = pdf([0, 0, 400, 200], vec![], HELLO);
        assert_eq!(extract_pages(bytes.clone(), None, Some(&[0, 0]), usize::MAX).unwrap().len(), 1);
        assert!(matches!(extract_pages(bytes.clone(), None, Some(&[1]), usize::MAX), Err(ExtractError::NoSuchPage(2, 1))));
        assert!(matches!(extract_pages(bytes, None, None, 3), Err(ExtractError::TooManyGlyphs(3))));
        assert!(matches!(extract_pages(Arc::new(b"nope".to_vec()), None, None, 10), Err(ExtractError::Open(_))));
    }

    /// A page of `depth` nested forms, each painting the next one twice (the second time moved a
    /// little, so the copies don't overlap): a few KB that draw 2^depth times. The innermost form
    /// shows "A".
    fn nested(depth: usize) -> Arc<Vec<u8>> {
        let mut doc = Document::with_version("1.7");
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let mut child = None;
        for level in (0..=depth).rev() {
            let (body, resources) = match child {
                None => ("BT /F1 10 Tf 10 10 Td (A) Tj ET".to_string(), dictionary! {"Font" => dictionary! {"F1" => font}}),
                Some(c) => {
                    let shift = 0.3 * f64::from(1u32 << (level / 2));
                    let cm = if level % 2 == 0 { format!("{shift:.4} 0") } else { format!("0 {shift:.4}") };
                    (format!("/C Do q 1 0 0 1 {cm} cm /C Do Q"), dictionary! {"XObject" => dictionary! {"C" => c}, "Font" => dictionary! {"F1" => font}})
                }
            };
            let form = Stream::new(
                dictionary! {"Type" => "XObject", "Subtype" => "Form", "BBox" => ints([0, 0, 400, 400]), "Resources" => resources},
                body.into_bytes(),
            );
            child = Some(doc.add_object(form));
        }
        let content = doc.add_object(Stream::new(dictionary! {}, b"/X0 Do".to_vec()));
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => ints([0, 0, 400, 400]), "Contents" => content,
            "Resources" => dictionary! {"XObject" => dictionary! {"X0" => child.unwrap()}, "Font" => dictionary! {"F1" => font}}
        });
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        Arc::new(bytes)
    }

    /// 2^22 glyphs from a 5 KB file: the page is stopped as soon as the room is used up, not after
    /// the interpreter has drawn them all (35 s before the check moved into the interpreter).
    #[test]
    fn a_page_that_paints_glyphs_without_end_is_stopped_at_the_limit() {
        let bytes = nested(22);
        let t = std::time::Instant::now();
        let r = extract_pages(bytes, None, None, 1000);
        assert!(matches!(r, Err(ExtractError::TooManyGlyphs(1000))), "{r:?}");
        assert!(t.elapsed().as_secs_f32() < 1.0, "took {:?}", t.elapsed());
    }

    /// Few operators and few enough glyphs, but 2^17 text pieces for the reading-order layout to sort
    /// (about 4 s in the Node module before layout counted its work, 19 s at 2^20).
    #[test]
    fn a_page_of_many_text_pieces_is_stopped_by_the_layout_work_limit() {
        let t = std::time::Instant::now();
        let r = extract_pages(nested(17), None, None, usize::MAX);
        assert!(matches!(r, Err(ExtractError::TooMuchWork(1))), "{r:?}");
        assert!(t.elapsed().as_secs_f32() < 2.0, "took {:?}", t.elapsed());
        // The limit is lifted afterwards: layout on this thread is unlimited again.
        assert!(!crate::text::arrange_is_limited());
    }

    /// The layout work of a call is limited apart from each page's: pages of many text pieces that
    /// are each under their own allowance stop at the page where the total runs out, and the error
    /// says the call, not the page, was over.
    #[test]
    fn the_layout_work_limit_of_a_call_names_the_page_that_passes_it() {
        // 300 pieces 40 points apart stay separate segments, so each page spends a good deal of layout work.
        let text: String = (0..300).map(|i| format!("BT /F1 10 Tf {} 10 Td (A) Tj ET\n", 5 + i * 40)).collect();
        let mut doc = Document::with_version("1.7");
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let pages = doc.new_object_id();
        let content = doc.add_object(Stream::new(dictionary! {}, text.into_bytes()));
        let kids: Vec<Object> = (0..10)
            .map(|_| {
                Object::Reference(doc.add_object(dictionary! {
                    "Type" => "Page", "Parent" => pages, "MediaBox" => ints([0, 0, 13000, 100]), "Contents" => content,
                    "Resources" => dictionary! {"Font" => dictionary! {"F1" => font}}
                }))
            })
            .collect();
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => kids, "Count" => 10}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let bytes = Arc::new(bytes);
        // What one page spends: read it with room to spare, then allow the call two and a half pages' worth.
        extract_pages_limited(bytes.clone(), None, Some(&[0]), usize::MAX, LIMITS, |_| Ok(())).unwrap();
        let one = crate::text::last_arrange_used();
        assert!(one > 0);
        let mut read = 0;
        let limits = Limits { call_arrange: one * 5 / 2, ..LIMITS };
        let r = extract_pages_limited(bytes.clone(), None, None, usize::MAX, limits, |_| {
            read += 1;
            Ok(())
        });
        assert!(matches!(r, Err(ExtractError::TooMuchWorkInCall(3))), "{r:?}");
        assert_eq!(read, 2);
        assert!(extract_pages_limited(bytes, None, None, usize::MAX, Limits { call_arrange: one * 10 + 10, ..LIMITS }, |_| Ok(())).is_ok());
    }

    /// Work that isn't glyphs (and isn't nested paints, which have their own limit): a content
    /// stream of millions of operators, which a small compressed file can inflate to.
    #[test]
    fn a_page_with_millions_of_operators_is_stopped_by_the_work_limit() {
        let bytes = pdf([0, 0, 400, 200], vec![], &"q\nQ\n".repeat(6_000_000));
        let t = std::time::Instant::now();
        let r = extract_pages(bytes, None, None, usize::MAX);
        assert!(matches!(r, Err(ExtractError::TooMuchWork(1))), "{r:?}");
        assert!(t.elapsed().as_secs_f32() < 5.0, "took {:?}", t.elapsed());
    }

    /// The allowance is per page: pages that each use most of it are all read, and a page over it
    /// is the one named, whatever the pages before it used.
    #[test]
    fn the_work_limit_is_per_page_and_names_the_page() {
        let heavy = "q\nQ\n".repeat(4_000_000); // 8 million operators, under the limit
        let over = "q\nQ\n".repeat(6_000_000);
        let two = |a: &str, b: &str| {
            let mut doc = Document::with_version("1.7");
            let pages = doc.new_object_id();
            let kids: Vec<Object> = [a, b]
                .iter()
                .map(|ops| {
                    let content = doc.add_object(Stream::new(dictionary! {}, ops.as_bytes().to_vec()));
                    Object::Reference(doc.add_object(dictionary! {"Type" => "Page", "Parent" => pages, "MediaBox" => ints([0, 0, 100, 100]), "Contents" => content}))
                })
                .collect();
            doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => kids, "Count" => 2}.into());
            let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
            doc.trailer.set("Root", catalog);
            let mut bytes = Vec::new();
            doc.save_to(&mut bytes).unwrap();
            Arc::new(bytes)
        };
        assert_eq!(extract_pages(two(&heavy, &heavy), None, None, usize::MAX).unwrap().len(), 2);
        let r = extract_pages(two(&heavy, &over), None, None, usize::MAX);
        assert!(matches!(r, Err(ExtractError::TooMuchWork(2))), "{r:?}");
    }

    /// The call's total is limited apart from each page's: pages that are each under their own
    /// allowance stop at the page where the total runs out.
    #[test]
    fn the_work_limit_of_a_call_names_the_page_that_passes_it() {
        let ops = "q\nQ\n".repeat(100); // 200 operators a page
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let content = doc.add_object(Stream::new(dictionary! {}, ops.as_bytes().to_vec()));
        let kids: Vec<Object> = (0..10)
            .map(|_| Object::Reference(doc.add_object(dictionary! {"Type" => "Page", "Parent" => pages, "MediaBox" => ints([0, 0, 100, 100]), "Contents" => content})))
            .collect();
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => kids, "Count" => 10}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let bytes = Arc::new(bytes);
        // 1100 operators for the call: five pages fit, the sixth has 100 left and passes them.
        let mut read = 0;
        let r = extract_pages_limited(bytes.clone(), None, None, usize::MAX, Limits { page_ops: 500, call_ops: 1100, ..LIMITS }, |_| {
            read += 1;
            Ok(())
        });
        assert!(matches!(r, Err(ExtractError::TooMuchWorkInCall(6))), "{r:?}");
        assert_eq!(read, 5);
        assert!(extract_pages_limited(bytes, None, None, usize::MAX, Limits { page_ops: 500, call_ops: 2100, ..LIMITS }, |_| Ok(())).is_ok());
    }

    /// A page that paints a 1 MB blank form 50,000 times is a few operators but 50 GB of bytes
    /// to decode and scan: the byte limit stops it within a second (42 s before).
    #[test]
    fn a_form_painted_again_and_again_is_stopped_by_the_byte_limit() {
        let mut doc = Document::with_version("1.7");
        let mut blank = vec![b' '; 1 << 20];
        blank.push(b'n');
        let form = doc.add_object(Stream::new(dictionary! {"Type" => "XObject", "Subtype" => "Form", "BBox" => ints([0, 0, 10, 10])}, blank));
        let content = doc.add_object(Stream::new(dictionary! {}, "/X Do\n".repeat(50_000).into_bytes()));
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => ints([0, 0, 100, 100]), "Contents" => content,
            "Resources" => dictionary! {"XObject" => dictionary! {"X" => form}}
        });
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let t = std::time::Instant::now();
        let r = extract_pages(Arc::new(bytes), None, None, usize::MAX);
        assert!(matches!(r, Err(ExtractError::TooMuchWork(1))), "{r:?}");
        assert!(t.elapsed().as_secs_f32() < 1.0, "took {:?}", t.elapsed());
        // The limit is lifted afterwards: the thread reads without one again.
        assert_eq!(hayro::hayro_interpret::content_byte_budget_left(), None);
    }

    /// A page that paints one shading 10,000 times (`sh`), whose data is a 1 MB stream, is a 2 KB
    /// file and 10 GB of decoding: the byte limit sits where every stream is decoded, so shading
    /// and function streams are stopped like forms (30 s before).
    #[test]
    fn shading_and_function_streams_count_against_the_byte_limit() {
        let nums = |v: &[i64]| Object::Array(v.iter().map(|n| Object::Integer(*n)).collect());
        let flate = |dict: lopdf::Dictionary, data: Vec<u8>| {
            let mut s = Stream::new(dict, data);
            s.compress().unwrap();
            s
        };
        for kind in ["mesh", "sampled", "postscript", "transfer"] {
            let mut doc = Document::with_version("1.7");
            let shading = match kind {
                "mesh" => doc.add_object(flate(
                    dictionary! {"ShadingType" => 4, "ColorSpace" => "DeviceGray", "BitsPerCoordinate" => 32,
                        "BitsPerComponent" => 16, "BitsPerFlag" => 8, "Decode" => nums(&[0, 1, 0, 1, 0, 1])},
                    vec![0u8; 1 << 20],
                )),
                _ => {
                    let function = if kind == "sampled" || kind == "transfer" {
                        flate(dictionary! {"FunctionType" => 0, "Domain" => nums(&[0, 1]), "Range" => nums(&[0, 1]),
                            "Size" => nums(&[1 << 20]), "BitsPerSample" => 8}, vec![0u8; 1 << 20])
                    } else {
                        let mut program = vec![b' '; 1 << 20];
                        program.insert(0, b'{');
                        program.push(b'}');
                        flate(dictionary! {"FunctionType" => 4, "Domain" => nums(&[0, 1]), "Range" => nums(&[0, 1])}, program)
                    };
                    let function = doc.add_object(function);
                    if kind == "transfer" {
                        // Not a shading: the same function stream, reached from `gs /TR`.
                        function
                    } else {
                    doc.add_object(dictionary! {"ShadingType" => 2, "ColorSpace" => "DeviceGray",
                        "Coords" => ints([0, 0, 1, 1]), "Function" => function})
                    }
                }
            };
            let content = doc.add_object(Stream::new(dictionary! {}, (if kind == "transfer" { "/S gs\n" } else { "/S sh\n" }).repeat(10_000).into_bytes()));
            let pages = doc.new_object_id();
            let page = doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages, "MediaBox" => ints([0, 0, 100, 100]), "Contents" => content,
                "Resources" => if kind == "transfer" {
                    dictionary! {"ExtGState" => dictionary! {"S" => dictionary! {"TR" => shading}}}
                } else {
                    dictionary! {"Shading" => dictionary! {"S" => shading}}
                }
            });
            doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1}.into());
            let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
            doc.trailer.set("Root", catalog);
            let mut bytes = Vec::new();
            doc.save_to(&mut bytes).unwrap();
            let t = std::time::Instant::now();
            let r = extract_pages(Arc::new(bytes), None, None, usize::MAX);
            assert!(matches!(r, Err(ExtractError::TooMuchWork(1))), "{kind}: {r:?}");
            // The mesh costs more than its bytes (it is also turned into triangles): 0.8 s alone,
            // about 1.3 s with the other tests running; 30 s before.
            assert!(t.elapsed().as_secs_f32() < 3.0, "{kind} took {:?}", t.elapsed());
            assert_eq!(hayro::hayro_interpret::content_byte_budget_left(), None);
        }
    }

    /// A damaged encoded stream among good ones costs only its own size: the document is
    /// still read, and only the damaged stream is left out.
    #[test]
    fn a_damaged_stream_does_not_use_up_the_byte_limit() {
        let mut doc = Document::with_version("1.7");
        let mut bad = Stream::new(dictionary! {"Type" => "XObject", "Subtype" => "Form", "BBox" => ints([0, 0, 10, 10]), "Filter" => "ASCIIHexDecode"}, b"zz not hex data".to_vec());
        bad.allows_compression = false;
        let bad = doc.add_object(bad);
        let good = doc.add_object(Stream::new(dictionary! {"Type" => "XObject", "Subtype" => "Form", "BBox" => ints([0, 0, 10, 10])}, b"q Q".to_vec()));
        let content = doc.add_object(Stream::new(dictionary! {}, "/B Do /G Do\n".repeat(200).into_bytes()));
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => ints([0, 0, 100, 100]), "Contents" => content,
            "Resources" => dictionary! {"XObject" => dictionary! {"B" => bad, "G" => good}}
        });
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let r = extract_pages(Arc::new(bytes), None, None, usize::MAX);
        assert!(r.is_ok(), "{r:?}");
    }

    /// Many pages sharing one content stream: each is under its own allowance, the call's bytes
    /// run out, and the page it ran out on is named.
    #[test]
    fn the_byte_limit_of_a_call_names_the_page_that_passes_it() {
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let content = doc.add_object(Stream::new(dictionary! {}, " ".repeat(10_000).into_bytes()));
        let kids: Vec<Object> = (0..500)
            .map(|_| Object::Reference(doc.add_object(dictionary! {"Type" => "Page", "Parent" => pages, "MediaBox" => ints([0, 0, 100, 100]), "Contents" => content})))
            .collect();
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => kids, "Count" => 500}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let mut read = 0;
        let r = extract_pages_limited(Arc::new(bytes), None, None, usize::MAX, Limits { call_bytes: 55_000, ..LIMITS }, |_| {
            read += 1;
            Ok(())
        });
        assert!(matches!(r, Err(ExtractError::TooMuchWorkInCall(6))), "{r:?}");
        assert_eq!(read, 5);
    }

    /// An ordinary page is not mistaken for either.
    #[test]
    fn a_shallow_nesting_is_read_in_full() {
        let p = extract_pages(nested(3), None, None, 100).unwrap().remove(0);
        assert!(!p.text.glyphs.is_empty() && p.text.glyphs.len() <= 8, "{}", p.text.glyphs.len());
    }

    /// Each page is handed over as soon as it is read.
    #[test]
    fn pages_are_handed_over_one_at_a_time() {
        let bytes = pdf([0, 0, 400, 200], vec![], HELLO);
        let mut seen = Vec::new();
        extract_pages_each(bytes, None, None, usize::MAX, |p| {
            seen.push(p.index);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![0]);
    }
}
