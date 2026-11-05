//! A builder (`mode: "builder"`) edits a PDF that already carries annotations with `/NM`. The
//! server keeps a file's annotations by that name (`onConflictDoNothing` on `annotId`), so a save
//! that renamed one would add it again on the next render. Each case edits, saves, reopens the
//! saved bytes and lists the names per page.

use std::sync::Arc;

use pdfcraft_cos::Document;
use pdfcraft_engine::{DocId, Edit, EditScope, Ownership, Session};
use pdfcraft_xfdf::list_annotations;

/// Three pages. Page 1: Square a1, Text a2 and its reply a3, Ink without a name. Page 2:
/// Highlight b1. Page 3: Square c1.
fn sample() -> Arc<Vec<u8>> {
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Annots [6 0 R 7 0 R 8 0 R 9 0 R] >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Annots [10 0 R] >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Annots [11 0 R] >>",
        "<< /Type /Annot /Subtype /Square /Rect [200 200 300 260] /C [1 0 0] /T (Carol) /NM (a1) /F 4 >>",
        "<< /Type /Annot /Subtype /Text /Rect [100 700 120 720] /Contents (note) /T (Alice) /NM (a2) /Name /Comment /F 4 >>",
        "<< /Type /Annot /Subtype /Text /Rect [100 700 120 720] /Contents (reply) /T (Bob) /NM (a3) /IRT 7 0 R /RT /R /F 4 /Name /Comment >>",
        "<< /Type /Annot /Subtype /Ink /Rect [10 10 50 50] /InkList [[10 10 50 50]] /T (Eve) /F 4 >>",
        "<< /Type /Annot /Subtype /Highlight /Rect [50 500 200 520] /QuadPoints [50 520 200 520 50 500 200 500] /C [1 1 0] /T (Dave) /NM (b1) /F 4 >>",
        "<< /Type /Annot /Subtype /Square /Rect [20 20 80 80] /C [0 0 1] /T (Frank) /NM (c1) /F 4 >>",
    ];
    let mut out = String::from("%PDF-1.7\n");
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out += &format!("{} 0 obj\n{o}\nendobj\n", i + 1);
    }
    let xref = out.len();
    out += &format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
    for o in &offs {
        out += &format!("{o:010} 00000 n \n");
    }
    out += &format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1);
    Arc::new(out.into_bytes())
}

fn open(actor: Option<&str>) -> (Session, DocId) {
    let mut s = Session::new().with_clock(|| 1_700_000_000);
    s.set_open_ownership(Ownership { actor: actor.map(str::to_string), scope: EditScope::All, ..Default::default() });
    let id = s.open("a.pdf", None, sample(), None).expect("opens");
    (s, id)
}

/// `page:subtype:name` of every annotation in the saved bytes, in order.
fn names(s: &Session, id: DocId) -> Vec<String> {
    let bytes = s.save_bytes(id).expect("saves");
    let doc = Document::open(bytes).expect("reopens");
    list_annotations(&doc).into_iter().map(|e| format!("{}:{}:{}", e.page + 1, e.subtype, e.nm.unwrap_or_else(|| "-".into()))).collect()
}

fn all_names(label: &str, actor: Option<&str>, edit: Edit) -> Vec<String> {
    let (mut s, id) = open(actor);
    s.apply(id, edit).unwrap_or_else(|e| panic!("{label}: {e}"));
    names(&s, id)
}

fn untouched() -> Vec<String> {
    vec!["1:Square:a1", "1:Text:a2", "1:Text:a3", "1:Ink:-", "2:Highlight:b1", "3:Square:c1"].into_iter().map(String::from).collect()
}

#[test]
fn editing_or_deleting_a_file_comment_keeps_the_names_of_the_rest() {
    let contents = Edit::SetAnnotationContents { page: 0, index: 1, text: "changed".into() };
    assert_eq!(all_names("contents", None, contents), untouched());
    let moved = Edit::MoveAnnotation { page: 0, index: 0, dx: 5.0, dy: 5.0 };
    assert_eq!(all_names("move", None, moved), untouched());
    let gone = all_names("delete", None, Edit::DeleteAnnotation { page: 0, index: 0 });
    assert_eq!(gone, untouched()[1..].to_vec(), "a deleted comment is gone from the saved bytes");
}

#[test]
fn page_changes_keep_the_names_and_move_the_page_numbers() {
    assert_eq!(all_names("delete page", None, Edit::DeletePages { pages: vec![0] }), ["1:Highlight:b1", "2:Square:c1"]);
    assert_eq!(
        all_names("move page", None, Edit::MovePages { pages: vec![2], to: 0 }),
        ["1:Square:c1", "2:Square:a1", "2:Text:a2", "2:Text:a3", "2:Ink:-", "3:Highlight:b1"]
    );
    assert_eq!(all_names("rotate", None, Edit::RotatePages { pages: vec![0], degrees: 90 }), untouched());
    assert_eq!(
        all_names("blank page", None, Edit::InsertBlankPage { at: 1, width: 600.0, height: 800.0 }),
        ["1:Square:a1", "1:Text:a2", "1:Text:a3", "1:Ink:-", "3:Highlight:b1", "4:Square:c1"]
    );
}

#[test]
fn a_copied_page_repeats_the_names_of_its_original() {
    // Without an actor nothing renames a copy's comments, so the server's import (one row per
    // name) keeps the original's and drops the copy's.
    let names = all_names("duplicate", None, Edit::DuplicatePages { pages: vec![0], at: None });
    assert_eq!(names.iter().filter(|n| n.ends_with(":a1")).count(), 2);
}

#[test]
fn a_new_comment_gets_its_own_name() {
    use pdfcraft_engine::{NewAnnotation, Shape, Style};
    let new = Edit::AddAnnotation(NewAnnotation {
        page: 1,
        shape: Shape::Rectangle { rect: [10.0, 10.0, 60.0, 60.0] },
        style: Style { color: [1.0, 0.0, 0.0], opacity: 1.0, width: 2.0, fill: None },
        contents: "new".into(),
        author: "Reviewer".into(),
    });
    let names = all_names("add", None, new);
    let added: Vec<_> = names.iter().filter(|n| n.starts_with("2:Square:")).collect();
    assert_eq!(added.len(), 1);
    assert!(!added[0].ends_with(":-") && !untouched().contains(added[0]), "{added:?}");
}

#[test]
fn with_an_actor_a_file_comment_is_someone_elses_and_page_edits_are_refused() {
    // So a builder document is opened without an actor.
    let (mut s, id) = open(Some("u-a"));
    assert!(s.apply(id, Edit::DeletePages { pages: vec![0] }).is_err());
    assert!(s.apply(id, Edit::SetAnnotationContents { page: 0, index: 1, text: "x".into() }).is_err());
}

#[test]
fn pages_changed_tells_a_page_structure_change_from_a_comment_edit() {
    let (mut s, id) = open(None);
    assert!(!s.pages_changed(id));
    s.apply(id, Edit::SetAnnotationContents { page: 0, index: 1, text: "changed".into() }).unwrap();
    s.apply(id, Edit::DeleteAnnotation { page: 0, index: 0 }).unwrap();
    assert!(!s.pages_changed(id), "comments are no page change");
    for (label, edit) in [
        ("delete", Edit::DeletePages { pages: vec![0] }),
        ("move", Edit::MovePages { pages: vec![2], to: 0 }),
        ("rotate", Edit::RotatePages { pages: vec![0], degrees: 90 }),
        ("blank", Edit::InsertBlankPage { at: 1, width: 600.0, height: 800.0 }),
        ("duplicate", Edit::DuplicatePages { pages: vec![0], at: None }),
    ] {
        let (mut s, id) = open(None);
        s.apply(id, edit).unwrap();
        assert!(s.pages_changed(id), "{label}");
        // Put back, it is the file as opened again.
        s.undo(id).unwrap();
        assert!(!s.pages_changed(id), "{label} undone");
    }
    // A crop shows another area: comments by place no longer fit either.
    let crop = Edit::SetPageBox { pages: vec![0], which: pdfcraft_engine::PageBox::Crop, spec: pdfcraft_engine::BoxSpec::Margins([10.0, 10.0, 10.0, 10.0]) };
    s.apply(id, crop).unwrap();
    assert!(s.pages_changed(id), "crop");
    // Stored, the pages are measured from that file: false until they change again.
    s.apply(id, Edit::DeletePages { pages: vec![2] }).unwrap();
    assert!(s.pages_changed(id));
    let bytes = s.save_bytes(id).unwrap();
    s.mark_saved(id, bytes, None).unwrap();
    assert!(!s.pages_changed(id));
    s.apply(id, Edit::RotatePages { pages: vec![0], degrees: 90 }).unwrap();
    assert!(s.pages_changed(id));
}

/// One page: Square "keep" with a name, Text and its reply (IRT) without one, and a Popup.
fn unnamed_sample() -> Arc<Vec<u8>> {
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Annots [4 0 R 5 0 R 6 0 R 7 0 R] >>",
        "<< /Type /Annot /Subtype /Square /Rect [200 200 300 260] /C [1 0 0] /NM (keep) /F 4 >>",
        "<< /Type /Annot /Subtype /Text /Rect [100 700 120 720] /Contents (note) /Name /Comment /F 4 >>",
        "<< /Type /Annot /Subtype /Text /Rect [100 700 120 720] /Contents (reply) /IRT 5 0 R /RT /R /F 4 /Name /Comment >>",
        "<< /Type /Annot /Subtype /Popup /Rect [300 700 400 760] /Parent 5 0 R >>",
    ];
    let mut out = String::from("%PDF-1.7\n");
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out += &format!("{} 0 obj\n{o}\nendobj\n", i + 1);
    }
    let xref = out.len();
    out += &format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
    for o in &offs {
        out += &format!("{o:010} 00000 n \n");
    }
    out += &format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1);
    Arc::new(out.into_bytes())
}

#[test]
fn a_builder_save_names_the_unnamed_comments_once_and_changes_nothing_else() {
    let mut s = Session::new().with_clock(|| 1_700_000_000);
    s.set_open_ownership(Ownership { actor: None, scope: EditScope::All, ..Default::default() });
    let id = s.open("a.pdf", None, unnamed_sample(), None).expect("opens");
    // The plain save leaves them as they are.
    assert_eq!(names(&s, id), ["1:Square:keep", "1:Text:-", "1:Text:-", "1:Popup:-"]);
    let first = s.save_bytes_named(id).unwrap();
    let listed: Vec<_> = list_annotations(&Document::open(first.clone()).unwrap()).into_iter().map(|e| (e.subtype, e.nm, e.irt)).collect();
    assert_eq!(listed[0].1.as_deref(), Some("keep"), "a name it has stays");
    let (note, reply) = (listed[1].1.clone().expect("named"), listed[2].1.clone().expect("named"));
    assert!(note != reply && note != "keep" && reply != "keep", "{note} {reply}");
    assert_eq!(listed[2].2, Some((0, 1)), "the reply still points at its comment");
    assert_eq!(listed[3].1, None, "a popup is no comment of its own");
    // Not an edit: nothing unsaved, no page change, and the working document is untouched.
    assert!(!s.get(id).unwrap().dirty && !s.pages_changed(id));
    assert_eq!(names(&s, id), ["1:Square:keep", "1:Text:-", "1:Text:-", "1:Popup:-"]);
    // Once stored the names are the file's own: the next save repeats them.
    s.mark_saved(id, first, None).unwrap();
    let second = s.save_bytes_named(id).unwrap();
    let again: Vec<_> = list_annotations(&Document::open(second).unwrap()).into_iter().map(|e| e.nm).collect();
    assert_eq!(again, [Some("keep".into()), Some(note), Some(reply), None]);
}
