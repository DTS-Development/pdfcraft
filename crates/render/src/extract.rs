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

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::page::Rotation;
use hayro::hayro_syntax::{DecryptionError, LoadPdfError, Pdf};

use crate::raster::panic_message;
use crate::text::{PageText, extract_page_filtered};
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
}

/// Extract the text of `pages` (0-based indices; `None` is every page), in ascending page order
/// without duplicates. `max_glyphs` bounds the glyphs of all the returned pages together: the
/// call fails instead of returning a truncated text. A password-protected document fails with
/// [`OpenError::NeedsPassword`] (or [`OpenError::WrongPassword`] when `password` is wrong).
pub fn extract_pages(
    bytes: Arc<Vec<u8>>,
    password: Option<&str>,
    pages: Option<&[usize]>,
    max_glyphs: usize,
) -> Result<Vec<ExtractedPage>, ExtractError> {
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
    let mut out = Vec::with_capacity(wanted.len());
    let mut glyphs = 0usize;
    for index in wanted {
        // The interpreter can panic on a damaged page: report it as that page's error.
        let done = catch_unwind(AssertUnwindSafe(|| {
            let info = page_info(&pdf, index);
            extract_page_filtered(&pdf, index, &settings, Default::default()).map(|text| (info, text))
        }))
        .map_err(|p| ExtractError::Page(index + 1, panic_message(&p)))?;
        let (info, text) = done.map_err(|why| ExtractError::Page(index + 1, why))?;
        glyphs = glyphs.saturating_add(text.glyphs.len());
        if glyphs > max_glyphs {
            return Err(ExtractError::TooManyGlyphs(max_glyphs));
        }
        out.push(ExtractedPage { index, info, text });
    }
    Ok(out)
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
}
