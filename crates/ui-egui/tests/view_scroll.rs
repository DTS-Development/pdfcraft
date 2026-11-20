//! Wheel scrolling after the View commands, whether they come from the palette or run directly.

use egui_kittest::Harness;
use pdfcraft_ui_egui::PdfCraftApp;

/// `n` pages of 200×300 with a line of text each.
fn fixture(n: usize) -> Vec<u8> {
    let mut objs: Vec<String> = vec!["<< /Type /Catalog /Pages 2 0 R >>".into()];
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    objs.push(format!("<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 200 300] >>", kids.join(" ")));
    objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    for i in 0..n {
        objs.push(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>", 5 + 2 * i));
        let body = format!("BT /F1 18 Tf 20 150 Td (Page {}) Tj ET", i + 1);
        objs.push(format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()));
    }
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let x = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offs {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn harness() -> Harness<'static, PdfCraftApp> {
    harness_dt(0.25)
}

fn harness_dt(dt: f32) -> Harness<'static, PdfCraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).with_step_dt(dt).build_eframe(|_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.open_bytes("a.pdf", None, fixture(6)).unwrap();
        app.set_option("zoom", "200").unwrap();
        app.set_option("left", "closed").unwrap();
        app.set_option("panel", "none").unwrap();
        app
    });
    for _ in 0..3000 {
        h.run_steps(2);
        if !h.state().render_pending() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.run_steps(6);
    h
}

/// Run `command` the way the palette does: open it, type the label, press Enter.
fn via_palette(h: &mut Harness<'static, PdfCraftApp>, label: &str) {
    h.state_mut().palette_open = true;
    h.state_mut().palette_query = label.to_string();
    h.run_steps(3);
    h.key_press(egui::Key::Enter);
    h.run_steps(4);
    assert!(!h.state().palette_open, "the palette closed");
}

const CASES: [(&str, &str); 7] = [
    ("view.layout.single", "Single page"),
    ("view.layout.two_up", "Two-page view"),
    ("view.continuous", "Continuous"),
    ("view.fit_width_scrolling", "Fit width"),
    ("view.fit_one_page", "Fit one full page"),
    ("view.fit_visible", "Fit visible"),
    ("view.marquee_zoom", "Marquee zoom"),
];

/// Browser wheel input: `Point` units, phase always `Move` (eframe web never reports phases).
fn web_notch(h: &mut Harness<'static, PdfCraftApp>, move_pointer: bool) -> (f32, bool) {
    let i = h.state().active.unwrap();
    let cur = h.state().views[i].current;
    let top = |h: &Harness<'static, PdfCraftApp>| h.state().views[i].page_screen_rect(h.state().views[i].current).map(|r| r.min.y);
    let before = top(h);
    if move_pointer {
        h.hover_at(h.state().views[i].viewport_rect().center());
        h.run_steps(2);
    }
    h.event(egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -100.0), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE });
    h.run_steps(30);
    let turned = h.state().views[i].current != cur;
    (match (before, top(h)) { (Some(a), Some(b)) => a - b, _ => f32::NAN }, turned)
}

/// After a View command (chosen from the palette or run directly, with the window focused or not,
/// the pointer left under where the palette was or not) a wheel notch still scrolls the page or
/// turns it. Each case below was checked to do one of the two; none leaves the view dead.
#[test]
fn the_wheel_still_works_after_each_view_command() {
    for (id, label) in CASES {
        for how in 0..4 {
            let mut h = harness();
            let i = h.state().active.unwrap();
            h.hover_at(h.state().views[i].viewport_rect().center());
            h.run_steps(2);
            if how == 0 {
                h.state_mut().execute(id);
                h.run_steps(4);
            } else {
                via_palette(&mut h, label);
            }
            if how == 2 {
                h.event(egui::Event::WindowFocused(false));
                h.run_steps(3);
            }
            let (moved, turned) = web_notch(&mut h, how == 3);
            assert!(turned || moved.abs() > 1.0, "{id} (variant {how}): the wheel did nothing ({moved}, turned {turned})");
        }
    }
}

/// The browser reports no touch phases, so a swipe made soon after the last one (its momentum
/// tail runs on, less than 0.15 s apart) must still turn a page of its own.
#[test]
fn quick_successive_swipes_each_turn_a_page_in_fit_one_page() {
    for gap_frames in [6usize, 18, 60] {
        let mut h = harness_dt(1.0 / 60.0);
        via_palette(&mut h, "Fit one full page");
        let i = h.state().active.unwrap();
        h.hover_at(h.state().views[i].viewport_rect().center());
        h.run_steps(2);
        for s in 1..=4 {
            for f in 0..40 {
                let d = 60.0 * 0.93f32.powi(f);
                h.event(egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -d), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE });
                h.run_steps(1);
            }
            h.run_steps(gap_frames);
            assert_eq!(h.state().views[i].current, s, "swipe {s} after gaps of {gap_frames} frames");
        }
    }
}
