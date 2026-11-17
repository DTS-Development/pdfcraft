//! Compare files: the text differences between two open documents, a report, and the
//! differences marked as comments in the newer one.

use std::sync::Arc;

pub use pdfcraft_compare::{Change, Comparison, Kind, Side};

use crate::{DocId, Edit, EditError, ImageResolution, Markup, NewAnnotation, NoteIcon, Session, Shape};

/// Acrobat's compare colours: replaced blue, inserted green, deleted red.
pub fn colour(kind: Kind) -> crate::Rgb {
    match kind {
        Kind::Replaced => [0.2, 0.45, 0.95],
        Kind::Inserted => [0.2, 0.75, 0.3],
        Kind::Deleted => [0.9, 0.25, 0.25],
    }
}

impl crate::Document {
    /// Standards ▸ Verify PDF/A: the rules the document breaks for `level`.
    pub fn pdfa_verify(&self, level: pdfcraft_preflight::Level) -> Vec<pdfcraft_preflight::Issue> {
        self.editor.as_ref().map(|e| pdfcraft_preflight::verify(&e.cos, level)).unwrap_or_default()
    }

    /// The standards the document declares (Standards panel).
    pub fn standards(&self) -> pdfcraft_preflight::Declared {
        self.editor.as_ref().map(|e| pdfcraft_preflight::declared(&e.cos)).unwrap_or_default()
    }

    /// Every word of the document in reading order, with page and box.
    pub fn words(&self) -> Vec<pdfcraft_compare::Word> {
        let config = pdfcraft_render::RenderConfig { password: self.password.as_deref().map(Arc::from), ..Default::default() };
        let mut r = pdfcraft_render::PageRenderer::new(self.bytes.clone(), config);
        let mut out = Vec::new();
        for (page, info) in self.info.pages.iter().enumerate() {
            let res = r.render(pdfcraft_render::RenderRequest { page, kind: pdfcraft_render::RequestKind::Text, scale: 1.0, ..Default::default() });
            if let Some(t) = res.text {
                out.extend(crate::js::page_words(&t, info).into_iter().map(|(text, rect)| pdfcraft_compare::Word { text, page, rect }));
            }
        }
        out
    }
}

impl Session {
    /// Compare files: the text differences from document `old` to document `new`.
    pub fn compare(&self, old: DocId, new: DocId) -> Result<Comparison, EditError> {
        let a = self.get(old).ok_or(EditError::NoDocument)?;
        let b = self.get(new).ok_or(EditError::NoDocument)?;
        Ok(pdfcraft_compare::compare(&a.words(), &b.words()))
    }

    /// Visual compare: regions where page n of `new` looks different from page n of `old`
    /// (rendered at `dpi`), as (page, user-space box in `new`).
    pub fn compare_visual(&self, old: DocId, new: DocId, dpi: f32) -> Result<Vec<(usize, [f64; 4])>, EditError> {
        let a = self.get(old).ok_or(EditError::NoDocument)?;
        let b = self.get(new).ok_or(EditError::NoDocument)?;
        let renderer = |d: &crate::Document| {
            pdfcraft_render::PageRenderer::new(
                d.bytes.clone(),
                pdfcraft_render::RenderConfig { password: d.password.as_deref().map(Arc::from), ..Default::default() },
            )
        };
        let (mut ra, mut rb) = (renderer(a), renderer(b));
        let scale = dpi.clamp(18.0, 150.0) / 72.0;
        let mut out = Vec::new();
        for (page, info) in b.info.pages.iter().enumerate().take(a.info.pages.len()) {
            let req = pdfcraft_render::RenderRequest { page, scale, ..Default::default() };
            let (x, y) = (ra.render(req), rb.render(req));
            if x.error.is_some() || y.error.is_some() {
                continue;
            }
            let s = y.width as f32 / info.width.max(1e-3);
            for r in pdfcraft_compare::visual_regions((&y.rgba, y.width, y.height), (&x.rgba, x.width, x.height), 24) {
                let p = info.view_to_user(r[0] as f32 / s, r[1] as f32 / s);
                let q = info.view_to_user(r[2] as f32 / s, r[3] as f32 / s);
                out.push((page, [p[0].min(q[0]) as f64, p[1].min(q[1]) as f64, p[0].max(q[0]) as f64, p[1].max(q[1]) as f64]));
            }
        }
        Ok(out)
    }

    /// The compare report as a new PDF (not opened).
    pub fn compare_report(&self, old: DocId, new: DocId) -> Result<Arc<Vec<u8>>, EditError> {
        let c = self.compare(old, new)?;
        let name = |id| self.get(id).map(|d| d.name.clone()).unwrap_or_default();
        self.create_report("Compare Report", &pdfcraft_compare::report(&c, &name(old), &name(new)))
    }

    /// Mark the differences in `new` as comments: highlights over replaced and inserted text
    /// (blue, green) and a note where text was deleted (red), authored "Compare". One undoable
    /// step; returns how many comments were added.
    pub fn mark_differences(&mut self, old: DocId, new: DocId) -> Result<usize, EditError> {
        let c = self.compare(old, new)?;
        let mut edits = Vec::new();
        for ch in &c.changes {
            let color = colour(ch.kind);
            let contents = match ch.kind {
                Kind::Replaced => format!("Replaced: \"{}\" with \"{}\"", ch.old.text, ch.new.text),
                Kind::Inserted => format!("Inserted: \"{}\"", ch.new.text),
                Kind::Deleted => format!("Deleted: \"{}\"", ch.old.text),
            };
            let shape = if ch.new.rects.is_empty() {
                let Some(r) = ch.new.near else { continue };
                Shape::Note { at: [r[0], r[3]], icon: NoteIcon::Note }
            } else {
                Shape::TextMarkup {
                    kind: Markup::Highlight,
                    quads: ch.new.rects.iter().map(|r| [r[0], r[3], r[2], r[3], r[0], r[1], r[2], r[1]]).collect(),
                }
            };
            let mut style = crate::Style::default_for(&shape);
            style.color = color;
            edits.push(Edit::AddAnnotation(NewAnnotation { page: ch.new.page, shape, style, contents, author: "Compare".into() }));
        }
        let n = edits.len();
        if n > 0 {
            self.apply(new, Edit::Batch { label: "Mark differences".into(), edits })?;
        }
        Ok(n)
    }
}

/// Overlay colours (the viewer's visual diff): the current file's ink red, the chosen file's blue,
/// ink in both gray.
pub const OVERLAY_CURRENT: [u8; 3] = [255, 0, 0];
pub const OVERLAY_CHOSEN: [u8; 3] = [0, 90, 255];
pub const OVERLAY_SAME: [u8; 3] = [140, 140, 140];

/// How an overlay is inked: the current file's colour, the chosen file's colour, and how strong
/// every mark is (1 as dark as the ink was, 0 invisible). Both-files ink stays [`OVERLAY_SAME`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayStyle {
    pub current: [u8; 3],
    pub chosen: [u8; 3],
    pub strength: f32,
}

impl Default for OverlayStyle {
    fn default() -> Self {
        Self { current: OVERLAY_CURRENT, chosen: OVERLAY_CHOSEN, strength: 1.0 }
    }
}

/// Below this ink (0 white .. 1 black) a pixel counts as paper, so page tints don't show.
const INK_FLOOR: f32 = 0.12;

/// How dark a pixel of a premultiplied RGBA rendering is, over white paper.
fn ink(px: &[u8]) -> f32 {
    let white = 255 - u32::from(px[3]);
    let l = |c: u8| (u32::from(c) + white).min(255) as f32;
    let lum = (0.299 * l(px[0]) + 0.587 * l(px[1]) + 0.114 * l(px[2])) / 255.0;
    let ink = 1.0 - lum;
    if ink < INK_FLOOR { 0.0 } else { ink }
}

/// One overlay page (opaque RGBA, `width` x `height`) from two renderings placed at the top-left
/// corner (page origins align, nothing is stretched): ink only in `current` red, only in `chosen`
/// blue, in both gray, each as dark as the ink was. `out` is reused from page to page.
pub fn overlay_page_into(out: &mut Vec<u8>, current: (&[u8], u32, u32), chosen: (&[u8], u32, u32), width: u32, height: u32) {
    overlay_page_styled_into(out, current, chosen, width, height, &OverlayStyle::default());
}

/// [`overlay_page_into`] with the host's colours and strength.
pub fn overlay_page_styled_into(
    out: &mut Vec<u8>,
    current: (&[u8], u32, u32),
    chosen: (&[u8], u32, u32),
    width: u32,
    height: u32,
    style: &OverlayStyle,
) {
    let strength = style.strength.clamp(0.0, 1.0);
    out.clear();
    out.resize(width as usize * height as usize * 4, 255);
    fn row<'a>(img: (&'a [u8], u32, u32), y: u32) -> &'a [u8] {
        if y >= img.2 {
            return &[];
        }
        let start = y as usize * img.1 as usize * 4;
        img.0.get(start..start + img.1 as usize * 4).unwrap_or(&[])
    }
    for y in 0..height {
        let (ra, rb) = (row(current, y), row(chosen, y));
        let line = &mut out[y as usize * width as usize * 4..(y as usize + 1) * width as usize * 4];
        for x in 0..width as usize {
            let ink_at = |r: &[u8]| r.get(x * 4..x * 4 + 4).map_or(0.0, ink);
            let (a, b) = (ink_at(ra), ink_at(rb));
            if a == 0.0 && b == 0.0 {
                continue;
            }
            let (rgb, k) = match (a > 0.0, b > 0.0) {
                (true, true) => (OVERLAY_SAME, a.max(b)),
                (true, false) => (style.current, a),
                _ => (style.chosen, b),
            };
            let k = k * strength;
            for (c, v) in rgb.iter().enumerate() {
                line[x * 4 + c] = (255.0 - (255.0 - f32::from(*v)) * k).round() as u8;
            }
        }
    }
}

/// [`overlay_page_into`] into a new buffer.
pub fn overlay_page(current: (&[u8], u32, u32), chosen: (&[u8], u32, u32), width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    overlay_page_into(&mut out, current, chosen, width, height);
    out
}

/// The overlay of two documents as a PDF, one page per pair of pages.
pub struct Overlay {
    pub pdf: Arc<Vec<u8>>,
    pub current_pages: usize,
    pub chosen_pages: usize,
}

/// Longest side, in pixels, an overlay page may have.
const OVERLAY_MAX_SIDE: f32 = 3000.0;

/// The zlib data of an 8-bit RGB PNG of opaque RGBA pixels: its IDAT chunks joined. A PDF image
/// can hold that stream as it is (with the PNG predictor), so the page is compressed once.
fn rgb_png_data(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut png = Vec::new();
    let mut enc = png::Encoder::new(&mut png, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut e| e.write_image_data(&rgb)).map_err(|e| e.to_string())?;
    // Chunks after the 8-byte signature: length, type, data, CRC.
    let (mut at, mut zlib) = (8usize, Vec::new());
    while let Some(head) = png.get(at..at + 8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let body = png.get(at + 8..at + 8 + len).ok_or("a PNG chunk runs past the end")?;
        if &head[4..8] == b"IDAT" {
            zlib.extend_from_slice(body);
        }
        at += 12 + len;
    }
    if zlib.is_empty() { Err("the PNG has no image data".into()) } else { Ok(zlib) }
}

/// An overlay being built one page pair at a time, so a window can keep drawing (and the build
/// can be dropped) between pages. It keeps the rendered files' bytes but not the session.
pub struct OverlayJob {
    ra: pdfcraft_render::PageRenderer,
    rb: pdfcraft_render::PageRenderer,
    scale: f32,
    next: usize,
    total: usize,
    current_pages: usize,
    chosen_pages: usize,
    /// The finished pages, each compressed into the PDF as soon as it is composed, so only one
    /// page's pixels are alive at a time.
    pages: pdfcraft_create::ImagePages,
    /// The composite's pixels, reused for every page.
    canvas: Vec<u8>,
    style: OverlayStyle,
}

impl OverlayJob {
    /// Ink the pages with these colours (before the first [`Self::step`]).
    pub fn set_style(&mut self, style: OverlayStyle) {
        self.style = style;
    }

    /// (pages done, pages in all).
    pub fn progress(&self) -> (usize, usize) {
        (self.next, self.total)
    }

    pub fn is_done(&self) -> bool {
        self.next >= self.total
    }

    /// Compose the next pair of pages.
    pub fn step(&mut self) -> Result<(), EditError> {
        if self.is_done() {
            return Ok(());
        }
        let req = pdfcraft_render::RenderRequest { page: self.next, scale: self.scale, ..Default::default() };
        let (x, y) = (self.ra.render(req), self.rb.render(req));
        if let Some(e) = x.error.as_ref().or(y.error.as_ref()) {
            return Err(EditError::Invalid(e.to_string()));
        }
        let (w, h) = (x.width.max(y.width), x.height.max(y.height));
        overlay_page_styled_into(&mut self.canvas, (&x.rgba, x.width, x.height), (&y.rgba, y.width, y.height), w, h, &self.style);
        // The canvas is opaque, so premultiplied or not it is the same pixels.
        let zlib = rgb_png_data(&self.canvas, w, h).map_err(EditError::Invalid)?;
        self.pages.add_png_data((w, h), zlib)?;
        self.next += 1;
        Ok(())
    }

    /// The pages as one PDF that keeps the pages' size in points: only the page tree and the
    /// file are made here, the pages were embedded as they were composed.
    pub fn finish(mut self, session: &Session) -> Result<Overlay, EditError> {
        self.canvas = Vec::new();
        let doc = self.pages.finish(Some("overlay-1.png"))?;
        let pdf = session.write_new(&doc)?;
        Ok(Overlay { pdf, current_pages: self.current_pages, chosen_pages: self.chosen_pages })
    }
}

impl Session {
    /// Start a visual compare, overlaid: page n of `current` against page n of `chosen` (the
    /// shorter file's pages), drawn at `dpi` (less when a page would pass [`OVERLAY_MAX_SIDE`]
    /// pixels).
    pub fn compare_overlay_job(&self, current: DocId, chosen: DocId, dpi: f32) -> Result<OverlayJob, EditError> {
        let a = self.get(current).ok_or(EditError::NoDocument)?;
        let b = self.get(chosen).ok_or(EditError::NoDocument)?;
        let n = a.info.pages.len().min(b.info.pages.len());
        if n == 0 {
            return Err(EditError::NoDocument);
        }
        let renderer = |d: &crate::Document| {
            pdfcraft_render::PageRenderer::new(
                d.bytes.clone(),
                pdfcraft_render::RenderConfig { password: d.password.as_deref().map(Arc::from), ..Default::default() },
            )
        };
        // One scale for every page, so one dpi sizes them all back to points.
        let longest = a.info.pages.iter().take(n).chain(b.info.pages.iter().take(n)).map(|p| p.width.max(p.height)).fold(1.0f32, f32::max);
        let scale = (dpi.clamp(18.0, 150.0) / 72.0).min(OVERLAY_MAX_SIDE / longest).max(0.05);
        Ok(OverlayJob {
            ra: renderer(a),
            rb: renderer(b),
            scale,
            next: 0,
            total: n,
            current_pages: a.info.pages.len(),
            chosen_pages: b.info.pages.len(),
            pages: pdfcraft_create::ImagePages::new(ImageResolution::Dpi(f64::from(72.0 * scale)))?,
            canvas: Vec::new(),
            style: OverlayStyle::default(),
        })
    }

    /// The whole overlay at once (see [`Session::compare_overlay_job`] to build it in steps).
    pub fn compare_overlay(&self, current: DocId, chosen: DocId, dpi: f32) -> Result<Overlay, EditError> {
        let mut job = self.compare_overlay_job(current, chosen, dpi)?;
        while !job.is_done() {
            job.step()?;
        }
        job.finish(self)
    }
}

/// Export a PDF ▸ Word, PowerPoint, HTML or RTF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfficeFormat {
    Docx,
    Pptx,
    Html,
    Rtf,
}

impl OfficeFormat {
    pub fn extension(self) -> &'static str {
        match self {
            OfficeFormat::Docx => "docx",
            OfficeFormat::Pptx => "pptx",
            OfficeFormat::Html => "html",
            OfficeFormat::Rtf => "rtf",
        }
    }

    pub fn from_extension(ext: &str) -> Option<OfficeFormat> {
        match ext.to_ascii_lowercase().as_str() {
            "docx" => Some(OfficeFormat::Docx),
            "pptx" => Some(OfficeFormat::Pptx),
            "html" | "htm" => Some(OfficeFormat::Html),
            "rtf" => Some(OfficeFormat::Rtf),
            _ => None,
        }
    }
}

impl crate::Document {
    /// The pages as paragraphs and images (for Word, HTML and RTF export), including what
    /// form XObjects draw.
    pub fn export_pages(&self) -> Vec<pdfcraft_export::Page> {
        let Some(cos) = self.editor.as_ref().map(|e| &e.cos) else { return Vec::new() };
        self.info
            .pages
            .iter()
            .enumerate()
            .map(|(i, info)| {
                // Export uses the visible page in points, y up. Raw PDF coordinates include
                // CropBox offsets and precede /Rotate and /UserUnit.
                let visible_rect = |r: [f64; 4]| {
                    let a = info.user_to_view(r[0] as f32, r[1] as f32);
                    let b = info.user_to_view(r[2] as f32, r[3] as f32);
                    [a[0].min(b[0]) as f64, (info.height - a[1].max(b[1])) as f64, a[0].max(b[0]) as f64, (info.height - a[1].min(b[1])) as f64]
                };
                let crop_width = f64::from(info.crop[2] - info.crop[0]);
                let display_width = f64::from(if info.rotation % 180 == 0 { info.width } else { info.height });
                let unit = if crop_width > 0.0 && crop_width.is_finite() && display_width.is_finite() { display_width / crop_width } else { 1.0 };
                let blocks = pdfcraft_edit::reading_blocks(cos, i)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|b| !b.text.trim().is_empty())
                    .map(|b| {
                        let f = b.base_font.to_ascii_lowercase();
                        pdfcraft_export::Block {
                            text: b.text,
                            rect: visible_rect(b.rect),
                            size: b.size * unit,
                            bold: f.contains("bold") || f.contains("black") || f.contains("heavy"),
                            italic: f.contains("italic") || f.contains("oblique"),
                            color: b.color,
                            rotation: b.rotation - f64::from(info.rotation),
                        }
                    })
                    .collect();
                let images = pdfcraft_edit::reading_images(cos, i)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|im| {
                        let (ext, bytes) = pdfcraft_create::image_file(cos, im.object?).ok()?;
                        Some(pdfcraft_export::Image {
                            ext: if ext == "jpg" { "jpg" } else { "png" },
                            bytes,
                            rect: visible_rect(im.rect),
                            size: [im.matrix[0].hypot(im.matrix[1]) * unit, im.matrix[2].hypot(im.matrix[3]) * unit],
                            rotation: im.matrix[1].atan2(im.matrix[0]).to_degrees() - f64::from(info.rotation),
                        })
                    })
                    .collect();
                pdfcraft_export::Page { width: info.width as f64, height: info.height as f64, blocks, images }
            })
            .collect()
    }

    /// The document as a Word, PowerPoint, HTML or RTF file.
    pub fn export_office(&self, format: OfficeFormat) -> Vec<u8> {
        let pages = self.export_pages();
        let title = self.info.title.clone().unwrap_or_else(|| self.name.trim_end_matches(".pdf").to_string());
        match format {
            OfficeFormat::Docx => pdfcraft_export::docx(&pages, &title),
            OfficeFormat::Pptx => pdfcraft_export::pptx(&pages, &title),
            OfficeFormat::Html => pdfcraft_export::html(&pages, &title).into_bytes(),
            OfficeFormat::Rtf => pdfcraft_export::rtf(&pages).into_bytes(),
        }
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    /// A 4x1 opaque strip: `ink` = black pixels, the rest white.
    fn strip(black: &[usize]) -> Vec<u8> {
        (0..4).flat_map(|i| if black.contains(&i) { [0, 0, 0, 255] } else { [255, 255, 255, 255] }).collect()
    }

    fn px(img: &[u8], i: usize) -> [u8; 3] {
        [img[i * 4], img[i * 4 + 1], img[i * 4 + 2]]
    }

    #[test]
    fn ink_only_in_the_current_file_is_red_only_in_the_chosen_is_blue_in_both_gray() {
        let cur = strip(&[0, 1]);
        let chosen = strip(&[1, 2]);
        let out = overlay_page((&cur, 4, 1), (&chosen, 4, 1), 4, 1);
        assert_eq!(px(&out, 0), OVERLAY_CURRENT, "only in the current file");
        assert_eq!(px(&out, 1), OVERLAY_SAME, "in both");
        assert_eq!(px(&out, 2), OVERLAY_CHOSEN, "only in the chosen file");
        assert_eq!(px(&out, 3), [255, 255, 255], "paper stays white");
        assert!(out.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn the_host_picks_the_colours_and_a_strength_that_fades_the_ink() {
        let (cur, chosen) = (strip(&[0, 1]), strip(&[1, 2]));
        let style = OverlayStyle { current: [0, 200, 0], chosen: [200, 0, 200], strength: 1.0 };
        let mut out = Vec::new();
        overlay_page_styled_into(&mut out, (&cur, 4, 1), (&chosen, 4, 1), 4, 1, &style);
        assert_eq!((px(&out, 0), px(&out, 1), px(&out, 2)), ([0, 200, 0], OVERLAY_SAME, [200, 0, 200]));
        // No style means the viewer's colours.
        assert_eq!(overlay_page((&cur, 4, 1), (&chosen, 4, 1), 4, 1), {
            overlay_page_styled_into(&mut out, (&cur, 4, 1), (&chosen, 4, 1), 4, 1, &OverlayStyle::default());
            out.clone()
        });
        // Half strength: halfway between paper and the colour; none: paper.
        overlay_page_styled_into(&mut out, (&cur, 4, 1), (&chosen, 4, 1), 4, 1, &OverlayStyle { strength: 0.5, ..style });
        assert_eq!(px(&out, 0), [128, 228, 128]);
        overlay_page_styled_into(&mut out, (&cur, 4, 1), (&chosen, 4, 1), 4, 1, &OverlayStyle { strength: 0.0, ..style });
        assert!((0..4).all(|i| px(&out, i) == [255, 255, 255]));
    }

    #[test]
    fn a_lighter_ink_blends_toward_white_and_paper_tints_vanish() {
        // 50% gray ink in the current file; a faint tint (below the floor) in the chosen one.
        let cur = vec![128, 128, 128, 255, 255, 255, 255, 255];
        let chosen = vec![255, 255, 255, 255, 240, 240, 240, 255];
        let out = overlay_page((&cur, 2, 1), (&chosen, 2, 1), 2, 1);
        let p = px(&out, 0);
        assert!(p[0] == 255 && p[1] > 100 && p[1] < 160 && p[1] == p[2], "half-dark red: {p:?}");
        assert_eq!(px(&out, 1), [255, 255, 255]);
    }

    #[test]
    fn different_sizes_align_at_the_corner_and_the_canvas_covers_both() {
        let small = strip(&[0]);
        let big = vec![0, 0, 0, 255].repeat(8);
        let out = overlay_page((&small, 4, 1), (&big, 4, 2), 4, 2);
        assert_eq!(out.len(), 4 * 2 * 4);
        assert_eq!(px(&out, 0), OVERLAY_SAME);
        assert_eq!(px(&out, 1), OVERLAY_CHOSEN);
        assert_eq!(px(&out, 4), OVERLAY_CHOSEN, "second row: only the taller file has ink");
    }

    #[test]
    fn the_overlay_keeps_the_page_size_and_pairs_pages_up_to_the_shorter_file() {
        let mut s = Session::new();
        let pdf = |t: &str| s.create_from_text("t", t).unwrap();
        let (pa, pb) = (pdf("Delivery within five days."), pdf("Delivery within three days."));
        let a = s.open("a.pdf", None, pa, None).unwrap();
        let b = s.open("b.pdf", None, pb, None).unwrap();
        let o = s.compare_overlay(a, b, 96.0).unwrap();
        assert_eq!((o.current_pages, o.chosen_pages), (1, 1));
        let c = s.open("c.pdf", None, o.pdf.clone(), None).unwrap();
        let (src, dst) = (&s.get(a).unwrap().info.pages[0], &s.get(c).unwrap().info.pages[0]);
        assert!((src.width - dst.width).abs() < 1.0 && (src.height - dst.height).abs() < 1.0, "{} x {} vs {} x {}", src.width, src.height, dst.width, dst.height);
    }
}
