//! PdfCraft in the browser. Build: `cd apps/pdfcraft-web && trunk build --release`
//! (trunk generates the JS loader; no handwritten JS — plan/execution-plan.md §1).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast;
    use pdfcraft_ui_egui::PdfCraftApp;

    let options = eframe::WebOptions { renderer: eframe::Renderer::Glow, ..Default::default() };
    wasm_bindgen_futures::spawn_local(async move {
        let Some(canvas) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("pdfcraft"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        else {
            return;
        };
        let started = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(|cc| {
                    let mut app = PdfCraftApp::new();
                    // The browser has no system clock for wasm32-unknown-unknown: without one a comment gets
                    // no /M or /CreationDate and Fill & Sign dates read 1 Jan 1970. This is the only Session.
                    app.session = std::mem::take(&mut app.session)
                        // New comment ids (/NM) must differ between sessions: wasm32 has no random source of its own.
                        .with_entropy(|| (js_sys::Math::random() * 9_007_199_254_740_992.0) as u64)
                        .with_wall_clock(
                            || (js_sys::Date::now() / 1000.0) as i64,
                            // getTimezoneOffset is minutes *west* of UTC, for today's date (daylight saving).
                            || -(js_sys::Date::new_0().get_timezone_offset() as i64) * 60,
                        );
                    // "printcraft": settings saved under the app's former name, PrintCraft.
                    if let Some(json) = cc.storage.and_then(|s| s.get_string("pdfcraft").or_else(|| s.get_string("printcraft"))) {
                        app.restore(&json);
                    }
                    // `?file=<url>` opens a PDF from a URL (same-origin or CORS-enabled).
                    // A host-lock build (PPM) opens only what the host sends, so it never fetches one.
                    if let Some(url) = query_param("file").filter(|_| !pdfcraft_ui_egui::HOST_LOCK) {
                        let inbox = app.startup_inbox.clone();
                        let failed = app.failed_inbox.clone();
                        let ctx = cc.egui_ctx.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            let name = url.rsplit('/').next().unwrap_or("document.pdf").split('?').next().unwrap_or("document.pdf").to_string();
                            match fetch_bytes(&url).await {
                                Ok(bytes) => {
                                    if let Ok(mut q) = inbox.lock() {
                                        q.push((name, bytes));
                                    }
                                }
                                Err(e) => {
                                    eframe::web_sys::console::error_1(&format!("PdfCraft: could not fetch {url}: {e}").into());
                                    // Shown in the app too, not only in the console (#173).
                                    if let Ok(mut q) = failed.lock() {
                                        q.push((name, e));
                                    }
                                }
                            }
                            ctx.request_repaint();
                        });
                    }
                    // eframe never raises a close request on a browser reload or tab close, so `guard_quit` never
                    // prompts; ask the browser to confirm instead while a document has unsaved work (#812).
                    warn_before_unload(app.unsaved_flag.clone());
                    #[cfg(feature = "host-lock")]
                    block_browser_find();
                    // `?host=parent`: a same-origin page embedding PdfCraft in an iframe (such
                    // as the Nextcloud app) passes files in and takes saves back (`host` below).
                    if query_param("host").as_deref() == Some("parent")
                        && let Some(bridge) = host::Bridge::new()
                    {
                        // `?author=`: the host's name for the user signs their comments, whatever name the
                        // shared settings hold; it is neither editable here nor saved.
                        if let Some(author) = query_param("author") {
                            app.set_host_author(&author);
                        }
                        // `?authorSep=&authorIdx=`: show only that piece of an author's name on screen (`userId#name#team`
                        // with `#` and 1 shows the name). Display only; the saved `/T` and every event keep the name whole.
                        if let Some(sep) = query_param("authorSep") {
                            let index = query_param("authorIdx").and_then(|i| i.parse::<usize>().ok()).unwrap_or(0);
                            pdfcraft_ui_egui::author_display::set(&sep, index);
                        }
                        // A host-driven frame starts clean: the previous frame's open side panel
                        // (compare, ...) and egui memory in localStorage are neither read nor saved (#26).
                        app.start_fresh_for_host(&cc.egui_ctx);
                        bridge.listen(
                            app.startup_inbox.clone(),
                            app.stamp_inbox.clone(),
                            app.compare_inbox.clone(),
                            app.ownership_inbox.clone(),
                            app.annot_inbox.clone(),
                            app.bytes_inbox.clone(),
                            cc.egui_ctx.clone(),
                        );
                        bridge.listen_view(app.view_inbox.clone(), cc.egui_ctx.clone());
                        app.host_view = Some(bridge.view_fn());
                        app.host_stamp = Some(bridge.stamp_fn());
                        app.host_annots = Some(bridge.annots_fn());
                        app.host_compare = Some(bridge.compare_fn());
                        app.host_save = Some(bridge.save_fn());
                        app.host_dirty = Some(bridge.dirty_fn());
                        app.host_bytes = Some(bridge.bytes_fn());
                        bridge.post_ready();
                    } else if query_param("host").as_deref() == Some("parent") {
                        eframe::web_sys::console::warn_1(
                            &"PdfCraft: ?host=parent needs a framing page on the same origin; saves download instead".into(),
                        );
                    }
                    // Without the bridge above `host_save` stays unset, and a host-lock build then
                    // shows only its "open it in PPM" page and opens nothing.
                    Ok(Box::new(app))
                }),
            )
            .await;
        if let Err(e) = started {
            eframe::web_sys::console::error_1(&e);
            if query_param("host").as_deref() == Some("parent")
                && let Some(bridge) = host::Bridge::new()
            {
                bridge.post_failed(&e.as_string().unwrap_or_else(|| format!("{e:?}")));
            }
        }
    });
}

/// Whether a keydown is one the editor uses itself (Cmd/Ctrl+F find, Cmd/Ctrl+G and Shift+G next/previous)
/// and eframe leaves to the browser, which would open its own find bar next to the editor's. Copy, paste
/// and the rest are not blocked.
#[cfg(any(target_arch = "wasm32", test))]
fn blocks_browser_default(ctrl: bool, meta: bool, alt: bool, key: &str, code: &str) -> bool {
    // `key` follows the layout (a Korean layout reports "ㄹ" or "Process"), `code` is the physical key.
    // Ctrl+Cmd together is left alone: Chrome on a Mac uses Ctrl+Cmd+F for full screen.
    (ctrl != meta) && !alt && (key.eq_ignore_ascii_case("f") || key.eq_ignore_ascii_case("g") || code == "KeyF" || code == "KeyG")
}

/// Listens for keydown in the capture phase and cancels only the browser's default for those keys.
/// It never stops propagation, so egui still receives the key.
#[cfg(all(target_arch = "wasm32", feature = "host-lock"))]
fn block_browser_find() {
    use eframe::wasm_bindgen::{JsCast, closure::Closure};
    let Some(window) = web_sys::window() else { return };
    let handler = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |event: web_sys::KeyboardEvent| {
        if blocks_browser_default(event.ctrl_key(), event.meta_key(), event.alt_key(), &event.key(), &event.code()) {
            event.prevent_default();
        }
    });
    let options = web_sys::AddEventListenerOptions::new();
    options.set_capture(true);
    if window.add_event_listener_with_callback_and_add_event_listener_options("keydown", handler.as_ref().unchecked_ref(), &options).is_ok() {
        handler.forget();
    }
}

#[cfg(target_arch = "wasm32")]
fn warn_before_unload(unsaved: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use eframe::wasm_bindgen::{JsCast, closure::Closure};
    let Some(window) = web_sys::window() else { return };
    let handler = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        if unsaved.load(std::sync::atomic::Ordering::Relaxed) {
            event.prevent_default();
        }
    });
    if window.add_event_listener_with_callback("beforeunload", handler.as_ref().unchecked_ref()).is_ok() {
        handler.forget();
    }
}

#[cfg(target_arch = "wasm32")]
fn query_param(key: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search).ok()?.get(key)
}

#[cfg(target_arch = "wasm32")]
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    use eframe::wasm_bindgen::JsCast;
    let window = web_sys::window().ok_or("no window")?;
    let resp = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp.dyn_into().map_err(|_| "not a response")?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let buf = wasm_bindgen_futures::JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

/// The `postMessage` bridge to a same-origin parent page. Messages are plain objects with a
/// `type`:
///
/// - `pdfcraft:ready` (to the parent): the app is listening; send files now.
/// - `pdfcraft:open` (from the parent): `{ name, bytes, actor?, readOnly?, mode?, policy?, replace? }`, where `bytes` is an
///   `ArrayBuffer` or `Uint8Array` holding a PDF. It opens in a tab, like a `?file=` URL; a second
///   open adds a tab, and the one showing may stay the earlier document. `replace: true` (a boolean,
///   a wrong type refuses the open) first closes every open document without asking about unsaved
///   changes and shows this one; without it nothing changes.
///   `actor` (a string of 1 to 200 characters, the user's id) makes every comment the app creates
///   the user's, and lets only that user change or delete comments; comments without an owner are
///   locked. `readOnly` (a boolean) refuses every edit that changes the document. Both default
///   to off, and each open sets them anew. A wrong type or length refuses the open with
///   `pdfcraft:failed` instead of opening without the checks. `mode` (`"viewer"` or `"builder"`)
///   is how much editing a host-lock build allows: `"builder"` everything it doesn't lock for
///   every host, anything else, or nothing, only comments. A build without the lock ignores it.
///   `maxAnnotationXfdfLength` (a whole number from 200000 to 8000000, UTF-16 units of one annotation's XFDF as it goes out
///   to the page, counted after `&quot;` escaping) raises the size an annotation may have to be saved; out of range refuses the open.
///   `annotSync` (a boolean, needs `actor`) turns on sharing the annotations through the page: edits
///   to annotations no longer make the file dirty, and the user's changes go out as
///   `pdfcraft:annots-changed` instead of `pdfcraft:save`. `commentsPanel: false` hides this
///   interface's own comments panel and the buttons that open it.
/// - `pdfcraft:annots-reset` (from the parent): `{ items, seq }`, the server's whole list, item =
///   `{ annotId, authorId, xfdf }` (one annotation as XFDF, `name` = `/NM`). It makes the document
///   match the list; `seq` is the last `annots-changed` number the page had received when it
///   fetched the list, and the actor's annotations changed in later messages (or not yet sent) are
///   left as they are. The actor's own annotations change in the working document and in the undo
///   states that hold the same version, so undoing an edit of theirs still works. `pdfcraft:annots-apply`
///   `{ upserts, deletes }` changes some (upserts are items, same `annotId` replaces; deletes are
///   annotIds). `pdfcraft:annots-reject` `{ items: [{ annotId, server }] }` puts back what the
///   server holds after it refused a change: `server` is an item, or `null` when the server has
///   none. All three skip undo and the dirty flag, and are not echoed in `annots-changed`. They
///   may come right behind `pdfcraft:open`: they wait for the document, are dropped when the open
///   fails, and a new `pdfcraft:open` drops those still waiting.
/// - `pdfcraft:annots-select` (from the parent): `{ annotId }` goes to the annotation's page and
///   selects it (nothing is selected when the id is unknown).
/// - `pdfcraft:annots-changed` (to the parent): `{ seq, upserts: [{ annotId, page, type, xfdf }], deletes: [annotId] }`
///   after the user applies, undoes or redoes an edit: the difference, by `/NM`, of the annotations
///   the `actor` owns (`seq` counts the messages from 1 for each document opened; `page` counts from 1 like the server's, `type` is the annotation's subject or XFDF tag, at most 100 characters). Stamps count too.
///   `pdfcraft:annots-selected` `{ annotId | null }` when the selection on the canvas changes.
///   `pdfcraft:sign-box-selected` `{ annotId, role, page, x, y, width, height, source, signer? }` (`source` is `"user"`
///   when the user clicked or keyed the box and `"api"` when the program selected it: a box just added with
///   `sign-box-add`, or `annots-select`; `signer` only when
///   the box has a `/PpmSigner`, sent as stored, never trimmed or left out; XFDF carries it as `ppmSigner`, and an
///   imported `ppmSigner` that is not a string is ignored. Whether an XFDF signer is valid is for the server to check) follows it when the
///   selected annotation is a sign box (a FreeText with `/PpmSignRole`): `page` counts from 0 and the
///   rectangle is in PDF points on the page as shown (turned by its `/Rotate`, origin top-left), the
///   space `stamp-place` takes. Not sent when the selection is cleared or is anything else; like
///   `annots-selected` it needs `annotSync`. When the user changes the selected sign box (moves it, undoes) it is sent
///   again alone, without `annots-selected`.
/// - `pdfcraft:save` (to the parent): `{ name, saveAs, pagesChanged, bytes }` when the user saves;
///   `bytes` is a transferred `ArrayBuffer`. `pagesChanged` is `true` when the pages differ from
///   the file as `pdfcraft:open` gave it (a page was deleted, moved, inserted, copied, replaced or
///   rotated and not put back; saving doesn't reset it), so comments the parent keeps by page
///   number may no longer fit. The parent stores the file (and reports its own errors).
/// - `pdfcraft:request-bytes` (from the parent): `{ id, purpose }`, `purpose` one of `save`, `check`,
///   `preview`: the active document as it is now (unsaved edits included). `pdfcraft:bytes` (to the
///   parent) answers `{ id, purpose, name, bytes[, pagesChanged: true] }`, `bytes` a transferred
///   `ArrayBuffer`; `pagesChanged` is there only when true and only for `mode: "builder"`, with
///   the meaning it has in `pdfcraft:save`. With `save` the document counts as saved. `pdfcraft:bytes-failed`
///   `{ id, purpose, reason }`, `reason` `not-ready` (no document is open), `edit-pending` or an
///   error text. A request with a wrong `id` or `purpose` gets no answer.
/// - `pdfcraft:stamp-add` (from the parent): `{ id, name, bytes, width, height, meta }` arms one
///   placement of a PNG stamp, `width` and `height` in PDF points, `meta` the text stored as the
///   stamp's `/PpmStamp`. `pdfcraft:stamp-cancel` `{ id }` withdraws it.
/// - `pdfcraft:stamp-place` (from the parent): `{ id, page, x, y, width?, height?, name, bytes, meta }`
///   stamps a PNG at once, with no click and nothing armed (an armed stamp still waiting is
///   cancelled first). `page` counts from 0; `x`, `y` are the stamp's top-left corner in PDF points on
///   the page as shown (turned by its `/Rotate`, origin top-left) and must lie inside the page.
///   `width` and `height` come together or not at all; without them the PNG's pixels are points
///   and the longer side is cut to 200 pt, the ratio kept. `meta` is stored as `/PpmStamp`. The
///   stamp is one undoable edit. Reported with `pdfcraft:stamp-placed` `{ id, page }`, or
///   `pdfcraft:stamp-failed` `{ id, error }` when it is refused (no document, comments not
///   allowed, page out of range, bad numbers, start outside the page, or the `stamp-add` checks).
/// - `pdfcraft:sign-in-box` (from the parent): `{ id, annotId, page, x, y, width?, height?, name, bytes, meta }`
///   signs in the sign box `annotId` (its `/NM`; it must be a FreeText with `/PpmSignRole` on `page`, and
///   not locked): the same as `stamp-place`, but in one edit the sign box is removed and the stamp put
///   in its place with `/F` Locked (128) and LockedContents (512). A sealed stamp can't be moved, resized,
///   deleted, unlocked or edited by the user or by `annots-apply`/`annots-reset` (`annots-reject` still
///   works), whoever signed it, also after the file is saved and opened again. The edit starts the undo
///   history over, so undo and redo can't bring the box back. Reported like `stamp-place` with `pdfcraft:stamp-placed`
///   `{ id, page }` or `pdfcraft:stamp-failed` `{ id, error }` (no such `annotId`, not a sign box, on another
///   page, locked, plus the `stamp-place` checks). With `annotSync` the box's delete and the stamp's upsert
///   come in `annots-changed`. Nothing is selected afterwards.
/// - `pdfcraft:sign-box-add` (from the parent): `{ id, role, label, signer? }` puts a 180 x 60 pt FreeText
///   sign box (text `label`, dashed black border, `/PpmSignRole` = `role`, and `/PpmSigner` = `signer`
///   when it is a string of 1 to 200 characters (UTF-16) with no space at either end and no control, bidi or
///   zero-width character, kept exactly as given; an invalid one is refused with `stamp-failed`, an empty string or
///   null means none; without it the box is as before) in the middle of the
///   page being read at once, selected; reported with `pdfcraft:stamp-placed` `{ id, page }`, or
///   `pdfcraft:stamp-failed` when it is refused. It is never armed, so there is nothing to cancel.
///   Optional `width` and `height` (points, both or neither, each at least 10 and not more than the page) replace
///   the 180 x 60 size; `page` (from 0) replaces the page being read; `x` and `y` (both or neither) put the box's
///   top-left corner there in points on the page as shown (as in `stamp-place`), otherwise it is centred. A box
///   that does not lie inside the page is refused.
/// - `pdfcraft:watermark` (from the parent): `{ text }` shows a diagonal watermark on each page
///   on the screen, under its annotations (WebViewer's `setWatermark({ diagonal })`: one 48 pt line
///   in the middle of the page along its diagonal, `rgba(120, 120, 120, 0.3)`). Like the host's own
///   viewer, which has no print or image export, no output takes it: not the saved file, not an
///   image export, not Print as Image. A blank or
///   missing `text` takes it away. It stays until changed, over documents opened later too.
///   An optional `style` `{ fontSize?, color?, opacity?, rotation? }` changes the look: `fontSize` in points (8 to
///   400, default 48; the line height follows it), `color` `#rgb` or `#rrggbb` (default `#787878`), `opacity` 0 to 1
///   (default 0.3), `rotation` in degrees clockwise as shown (-360 to 360) or `"diagonal"` (default, the page's own
///   diagonal). A `style` that can't be read (unknown field, wrong type, out of range) takes the watermark away.
/// - `pdfcraft:annots-visible` (from the parent): `{ visible, keepSignatures? }` hides (`false`) or shows the comments
///   on the screen of every document, also those opened later, until it is sent again. It draws only: the document,
///   undo, `annots-changed` and what is synced stay as they are, and while hidden the comments can't be picked or
///   drawn (the same as the menu's Hide all comments, which still works per document afterwards). Form fields and
///   links stay. With `keepSignatures: true` signatures (sealed comments) and sign boxes stay drawn too.
/// - `pdfcraft:annotation-defaults` (from the parent): `{ customData }`, an object of strings (at most 64 keys, keys 1 to
///   100 characters, values up to 2000, no key starting with `ppm`), is put in each comment, answer, status and mark the
///   user creates from now on (`/PpmCustomData`, so it is saved and goes out as `<trn-custom-data>` in `annots-changed`),
///   until the next message; `null` or `{}` stops. Not on sign boxes, signatures, copied pages' comments, or what the
///   server applies. A value the comment already has is kept. Needs a user (`actor`), like marking comments as theirs.
/// - `pdfcraft:annotation-popup-actions` (from the parent): `{ actions }`, a list of `{ id, label, icon, closeOnClick? }` (at most 8;
///   `id` 1 to 64 characters of letters, digits and `-_.:`, no two alike; `label` 1 to 20 characters, the button's tooltip and name; `icon`
///   the name of one of the embedded icons; `closeOnClick: true` deselects the annotation right after the click is reported, so
///   the bar goes away; an editor without it refuses the whole list as an unknown field), puts one icon button each on a bar that floats next to the selected annotation of a
///   document shared with the page (`annotSync`), for every document, also those opened later, until it is sent again. `null`,
///   missing or `[]` takes the bar away; a list that can't be used does too and is logged. Sign boxes and replies have no bar;
///   signatures do. It is there in viewer and `readOnly` documents too. A new list shows in an open bar at once. A click goes
///   to the parent as `pdfcraft:annotation-popup-action` `{ id, annotId, subject?, page, rect }` (`subject` is the annotation's
///   `/Subj`, else its XFDF tag name such as `square`, cut to 100 UTF-16 units: the same value as an `annots-changed` upsert's
///   `subtype`; absent for a kind with no XFDF form; `page` from 0, `rect` `{ x, y, width,
///   height }` in points on the page as shown, top-left origin, like `sign-box-selected`).
/// - `pdfcraft:stamp-armed` `{ id }`, `pdfcraft:stamp-placed` `{ id, page }`,
///   `pdfcraft:stamp-cancelled` `{ id }` and `pdfcraft:stamp-failed` `{ id, error }` (to the
///   parent) report how a stamp request went.
/// - `pdfcraft:compare` (from the parent): `{ id, old: { name, bytes }, new: { name, bytes }, mode?, policy?, overlay?, side?, view?, lockView? }`
///   opens the old file in a background tab and the new file as the active one, then runs the
///   Compare Files comparison. `overlay` is `{ colorA?, colorB?, opacity?, opacityA?, opacityB? }`: the ink of the old and the new
///   file in the overlay (`#rgb`/`#rrggbb`) and how strong it is (0 to 1; `opacityA` / `opacityB` for one file win over `opacity`; ink in
///   both files is as strong as the stronger one); a value that can't be read fails the compare. `side` is `{ colorA?, colorB? }`: the
///   shading of the old and the new file's words in the side-by-side view (`#rgb`/`#rrggbb`; the viewer's colours when missing).
///   `view` (`"side"` default, `"overlay"`, or `"overlay-side"`, the old file beside the overlay) is the view the comparison opens on (an overlay reports `compare-view` with `building`
///   as usual, its `view` being one of the three); `lockView: true` takes the switch among them away from the user (an overlay that fails to build goes back to side by side). A window compares once; `pdfcraft:compare-done`
///   `{ id, replaced, inserted, deleted, visual }` or `pdfcraft:compare-failed` `{ id, error }`
///   (to the parent) report the outcome. In this mode the Compare panel has no Mark as
///   comments or Report.
/// - `pdfcraft:compare-preview` (from the parent): `{ id, old: { name, bytes }, new: { name, bytes }, mode?, policy? }` opens the two files side by
///   side (the new one on the left, scrolled and zoomed together) without comparing: no shading, no change list, no view switch. It counts as the
///   window's one comparison. Answered with `pdfcraft:compare-preview-done` `{ id }` or `pdfcraft:compare-preview-failed` `{ id, error }`.
/// - `pdfcraft:compare-run` (from the parent): `{ id, overlay?, side?, view?, lockView? }` compares the two files a preview opened, as `pdfcraft:compare`
///   does (same options, same answers: `compare-done`, `compare-failed`, `compare-view`). Without an open preview, or a second time, it fails with `compare-failed`.
/// - `pdfcraft:goto` (from the parent): `{ id, page, rect? }` scrolls to the page (from 0, as in `stamp-place`), or to
///   `rect` `{ x, y, width, height }` on it (points on the page as shown, top-left origin; its centre goes to the middle
///   of the window at the zoom as it is). Answered with `pdfcraft:goto-done` `{ id }` or `pdfcraft:goto-failed`
///   `{ id, error }` (no document, page out of range, a bad rect).
/// - `pdfcraft:highlight` (from the parent): `{ id, page, rects, color?, transient }` marks `rects` (a list of the same
///   `{ x, y, width, height }`) on the page. `transient` must be `true`: the mark is a screen overlay, no annotation, so it
///   is not saved, not undoable and not in `annots-changed`. `color` is `#rgb`, `#rrggbb` (drawn at 40% opacity)
///   or `#rrggbbaa`, yellow if missing. It does not scroll (use `goto`). The next `highlight` or `pdfcraft:highlight-clear`
///   `{}` takes it away. Answered with `pdfcraft:highlight-done` `{ id }` or `pdfcraft:highlight-failed` `{ id, error }`.
/// - `pdfcraft:page-info` (from the parent): `{ id }` is answered with `pdfcraft:page-info-done`
///   `{ id, current, pages: [{ width, height, rotation }] }` (`current` from 0 like every page here; `width` and
///   `height` in points on the page as shown, turned by `rotation`, the page's `/Rotate`, and upright like the
///   rects of `goto`) or `pdfcraft:page-info-failed` `{ id, error }` (no document).
///   `pdfcraft:page-changed` `{ page }` (to the parent, page from 0) follows the page being read: once when the
///   first page of a document is drawn, then whenever it changes or another document comes to the front.
/// - `pdfcraft:opened` (to the parent): once for each `pdfcraft:open`, when a page of that document was first drawn
///   (`ready` only says the app listens).
/// - `pdfcraft:dirty` (to the parent): `{ dirty }` whenever unsaved work appears or goes away.
/// - `pdfcraft:failed` (to the parent): `{ error }` when the app couldn't start.
///
/// Only the parent window on the app's own origin is heard or answered. With a parent on
/// another origin there is no bridge: the app runs as if unframed and saves download.
#[cfg(target_arch = "wasm32")]
mod host {
    use eframe::wasm_bindgen::{JsCast, JsValue, closure::Closure};

    #[derive(Clone)]
    pub struct Bridge {
        window: web_sys::Window,
        parent: web_sys::Window,
        origin: String,
    }

    impl Bridge {
        /// `None` when the page isn't framed (there is no parent to talk to), or when the parent
        /// is on another origin: messages only go to this origin, so the browser would drop
        /// every save without an error while the document showed as saved (#2167).
        pub fn new() -> Option<Self> {
            let window = web_sys::window()?;
            let parent = window.parent().ok().flatten()?;
            if js_sys::Object::is(&parent, &window) {
                return None;
            }
            let origin = window.location().origin().ok()?;
            // Reading another origin's location throws, which is `None` too.
            if parent.location().origin().ok()? != origin {
                return None;
            }
            Some(Self { window, parent, origin })
        }

        fn post(&self, message: &js_sys::Object, transfer: Option<&JsValue>) -> Result<(), String> {
            let sent = match transfer {
                Some(t) => self.parent.post_message_with_transfer(message, &self.origin, &js_sys::Array::of1(t)),
                None => self.parent.post_message(message, &self.origin),
            };
            sent.map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
        }

        pub fn post_ready(&self) {
            if let Some(m) = message(&[("type", "pdfcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]) {
                let _ = self.post(&m, None);
            }
        }

        /// The app couldn't start (no WebGL, for example): let the parent say so.
        pub fn post_failed(&self, error: &str) {
            if let Some(m) = message(&[("type", "pdfcraft:failed".into()), ("error", error.into())]) {
                let _ = self.post(&m, None);
            }
        }

        /// Hear `pdfcraft:open` from the parent and queue the file like a `?file=` download.
        pub fn listen(
            &self,
            inbox: pdfcraft_ui_egui::Inbox,
            stamps: pdfcraft_ui_egui::StampInbox,
            compares: pdfcraft_ui_egui::CompareInbox,
            ownership: pdfcraft_ui_egui::OwnershipInbox,
            annots: pdfcraft_ui_egui::AnnotInbox,
            bytes: pdfcraft_ui_egui::BytesInbox,
            ctx: egui::Context,
        ) {
            let bridge = self.clone();
            let parent = self.parent.clone();
            let origin = self.origin.clone();
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                    return;
                }
                let data = event.data();
                match string(&data, "type").as_deref() {
                    Some("pdfcraft:open") => {
                        // Messages left over from an earlier open belong to that open.
                        if let Ok(mut q) = annots.lock() {
                            q.clear();
                        }
                    }
                    Some(kind) if pdfcraft_ui_egui::is_annot_sync_message(kind) => {
                        match annots_command(kind, &data) {
                            Ok(command) => {
                                if let Ok(mut q) = annots.lock() {
                                    q.push(command);
                                }
                                ctx.request_repaint();
                            }
                            Err(error) => eframe::web_sys::console::warn_1(&format!("PdfCraft: {kind} refused: {error}").into()),
                        }
                        return;
                    }
                    Some("pdfcraft:request-bytes") => {
                        // A request with a missing or odd id or purpose gets no answer, as with the other apps.
                        let id = string(&data, "id").filter(|id| !id.is_empty() && id.chars().count() <= 128);
                        let purpose = string(&data, "purpose").and_then(|p| pdfcraft_ui_egui::BytesPurpose::parse(&p));
                        match (id, purpose) {
                            (Some(id), Some(purpose)) => {
                                if let Ok(mut q) = bytes.lock() {
                                    q.push(pdfcraft_ui_egui::BytesRequest { id, purpose });
                                }
                                ctx.request_repaint();
                            }
                            _ => eframe::web_sys::console::warn_1(&"PdfCraft: pdfcraft:request-bytes refused: bad id or purpose".into()),
                        }
                        return;
                    }
                    Some("pdfcraft:stamp-add") => {
                        let id = string(&data, "id").unwrap_or_default();
                        match stamp_add(&data) {
                            Ok(command) => {
                                if let Ok(mut q) = stamps.lock() {
                                    q.push(command);
                                }
                                ctx.request_repaint();
                            }
                            Err(error) => bridge.post_stamp(&pdfcraft_ui_egui::StampEvent::Failed { id, error }),
                        }
                        return;
                    }
                    Some(kind @ ("pdfcraft:compare" | "pdfcraft:compare-preview" | "pdfcraft:compare-run")) => {
                        use pdfcraft_ui_egui::{CompareEvent, CompareOp};
                        let op = match kind {
                            "pdfcraft:compare-preview" => CompareOp::Preview,
                            "pdfcraft:compare-run" => CompareOp::Run,
                            _ => CompareOp::Compare,
                        };
                        let id = string(&data, "id").unwrap_or_default();
                        match compare_request(&data, op) {
                            Ok(command) => {
                                if let Ok(mut q) = compares.lock() {
                                    q.push(command);
                                }
                                ctx.request_repaint();
                            }
                            Err(error) => {
                                let id = id.chars().take(128).collect();
                                bridge.post_compare(&if op == CompareOp::Preview { CompareEvent::PreviewFailed { id, error } } else { CompareEvent::Failed { id, error } })
                            }
                        }
                        return;
                    }
                    Some("pdfcraft:stamp-place") => {
                        let id = string(&data, "id").unwrap_or_default();
                        match stamp_place(&data) {
                            Ok(command) => {
                                if let Ok(mut q) = stamps.lock() {
                                    q.push(command);
                                }
                                ctx.request_repaint();
                            }
                            Err(error) => bridge.post_stamp(&pdfcraft_ui_egui::StampEvent::Failed { id, error }),
                        }
                        return;
                    }
                    Some("pdfcraft:sign-in-box") => {
                        let id = string(&data, "id").unwrap_or_default();
                        match sign_in_box(&data) {
                            Ok(command) => {
                                if let Ok(mut q) = stamps.lock() {
                                    q.push(command);
                                }
                                ctx.request_repaint();
                            }
                            Err(error) => bridge.post_stamp(&pdfcraft_ui_egui::StampEvent::Failed { id, error }),
                        }
                        return;
                    }
                    Some("pdfcraft:sign-box-add") => {
                        let id = string(&data, "id").unwrap_or_default();
                        match sign_box_add(&data) {
                            Ok(command) => {
                                if let Ok(mut q) = stamps.lock() {
                                    q.push(command);
                                }
                                ctx.request_repaint();
                            }
                            Err(error) => bridge.post_stamp(&pdfcraft_ui_egui::StampEvent::Failed { id, error }),
                        }
                        return;
                    }
                    Some("pdfcraft:watermark") => {
                        // A text of the wrong type counts as none: the watermark goes, never a stale one stays.
                        let text = string(&data, "text");
                        // A `style` that can't be read counts as no watermark too, with the reason in the console.
                        let style = js_sys::Reflect::get(&data, &JsValue::from_str("style")).unwrap_or(JsValue::UNDEFINED);
                        let (text, style) = if style.is_undefined() || style.is_null() {
                            (text, None)
                        } else {
                            let parsed = js_sys::JSON::stringify(&style)
                                .ok()
                                .and_then(|s| s.as_string())
                                .ok_or_else(|| "style is not plain data".to_string())
                                .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).map_err(|e| e.to_string()))
                                .and_then(|value| pdfcraft_ui_egui::WatermarkStyle::from_json(&value));
                            match parsed {
                                Ok(style) => (text, Some(style)),
                                Err(error) => {
                                    eframe::web_sys::console::warn_1(&format!("PdfCraft: pdfcraft:watermark style refused: {error}").into());
                                    (None, None)
                                }
                            }
                        };
                        if let Ok(mut q) = stamps.lock() {
                            q.push(pdfcraft_ui_egui::StampCommand::Watermark { text, style });
                        }
                        ctx.request_repaint();
                        return;
                    }
                    Some("pdfcraft:annots-visible") => {
                        // `visible` must be a boolean; anything else leaves things as they are (the host's call is refused by its own checks).
                        let flag = |key: &str| js_sys::Reflect::get(&data, &JsValue::from_str(key)).ok().and_then(|v| v.as_bool());
                        if let Some(visible) = flag("visible") {
                            if let Ok(mut q) = stamps.lock() {
                                q.push(pdfcraft_ui_egui::StampCommand::AnnotsVisible {
                                    visible,
                                    keep_signatures: flag("keepSignatures").unwrap_or(false),
                                });
                            }
                            ctx.request_repaint();
                        } else {
                            eframe::web_sys::console::warn_1(&"PdfCraft: pdfcraft:annots-visible refused: visible must be a boolean".into());
                        }
                        return;
                    }
                    Some("pdfcraft:annotation-defaults") => {
                        // `customData` is an object of strings, or null/missing to stop. Anything else stops it too (never a stale value).
                        let value = js_sys::Reflect::get(&data, &JsValue::from_str("customData")).unwrap_or(JsValue::UNDEFINED);
                        let pairs = if value.is_undefined() || value.is_null() {
                            Vec::new()
                        } else {
                            js_sys::JSON::stringify(&value)
                                .ok()
                                .and_then(|s| s.as_string())
                                .and_then(|json| serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(&json).ok())
                                .and_then(|m| m.into_iter().map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect::<Option<Vec<_>>>())
                                .unwrap_or_else(|| {
                                    eframe::web_sys::console::warn_1(&"PdfCraft: pdfcraft:annotation-defaults refused: customData must be an object of strings".into());
                                    Vec::new()
                                })
                        };
                        if let Ok(mut q) = stamps.lock() {
                            q.push(pdfcraft_ui_egui::StampCommand::AnnotationDefaults { custom_data: pairs });
                        }
                        ctx.request_repaint();
                        return;
                    }
                    Some("pdfcraft:annotation-popup-actions") => {
                        // `actions` is a list of `{ id, label, icon }` (at most 8) or null/missing/[] to take them away. A list that can't be used
                        // takes them away too (never a stale one) and says why in the console.
                        let value = js_sys::Reflect::get(&data, &JsValue::from_str("actions")).unwrap_or(JsValue::UNDEFINED);
                        let json = if value.is_undefined() || value.is_null() { None } else { js_sys::JSON::stringify(&value).ok().and_then(|s| s.as_string()) };
                        let parsed = match json.as_deref().map(serde_json::from_str::<serde_json::Value>) {
                            None => pdfcraft_ui_egui::parse_popup_actions(None),
                            Some(Ok(v)) => pdfcraft_ui_egui::parse_popup_actions(Some(&v)),
                            Some(Err(e)) => Err(e.to_string()),
                        };
                        let actions = parsed.unwrap_or_else(|error| {
                            eframe::web_sys::console::warn_1(&format!("PdfCraft: pdfcraft:annotation-popup-actions refused: {error}").into());
                            Vec::new()
                        });
                        if let Ok(mut q) = stamps.lock() {
                            q.push(pdfcraft_ui_egui::StampCommand::AnnotationPopupActions { actions });
                        }
                        ctx.request_repaint();
                        return;
                    }
                    Some("pdfcraft:stamp-cancel") => {
                        if let Some(id) = string(&data, "id") {
                            if let Ok(mut q) = stamps.lock() {
                                q.push(pdfcraft_ui_egui::StampCommand::Cancel { id });
                            }
                            ctx.request_repaint();
                        }
                        return;
                    }
                    _ => return,
                }
                let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "document.pdf".to_string());
                let who = match open_ownership(&data) {
                    Ok(who) => who,
                    Err(error) => {
                        bridge.post_failed(&error);
                        return;
                    }
                };
                let Ok(bytes) = js_sys::Reflect::get(&data, &JsValue::from_str("bytes")) else { return };
                let bytes = if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
                    array.to_vec()
                } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
                    js_sys::Uint8Array::new(&bytes).to_vec()
                } else {
                    return;
                };
                // The rules go first, so the file is never opened without them.
                if let Ok(mut o) = ownership.lock() {
                    o.push(who);
                }
                if let Ok(mut q) = inbox.lock() {
                    q.push((name, bytes));
                }
                ctx.request_repaint();
            });
            if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
                // The listener lives as long as the page.
                on_message.forget();
            }
        }

        /// Tell the parent how a stamp request went.
        pub fn post_stamp(&self, event: &pdfcraft_ui_egui::StampEvent) {
            use pdfcraft_ui_egui::StampEvent as E;
            let fields = match event {
                E::Armed { id } => vec![("type", "pdfcraft:stamp-armed".into()), ("id", id.as_str().into())],
                E::Placed { id, page } => {
                    vec![("type", "pdfcraft:stamp-placed".into()), ("id", id.as_str().into()), ("page", JsValue::from_f64(*page as f64))]
                }
                E::Cancelled { id } => vec![("type", "pdfcraft:stamp-cancelled".into()), ("id", id.as_str().into())],
                E::Failed { id, error } => {
                    vec![("type", "pdfcraft:stamp-failed".into()), ("id", id.as_str().into()), ("error", error.as_str().into())]
                }
            };
            if let Some(m) = message(&fields) {
                let _ = self.post(&m, None);
            }
        }

        /// Tell the parent how a compare request went.
        pub fn post_compare(&self, event: &pdfcraft_ui_egui::CompareEvent) {
            use pdfcraft_ui_egui::CompareEvent as E;
            let count = |n: usize| JsValue::from_f64(n as f64);
            let fields = match event {
                E::Done { id, replaced, inserted, deleted, visual } => vec![
                    ("type", "pdfcraft:compare-done".into()),
                    ("id", id.as_str().into()),
                    ("replaced", count(*replaced)),
                    ("inserted", count(*inserted)),
                    ("deleted", count(*deleted)),
                    ("visual", count(*visual)),
                ],
                E::Failed { id, error } => {
                    vec![("type", "pdfcraft:compare-failed".into()), ("id", id.as_str().into()), ("error", error.as_str().into())]
                }
                E::PreviewDone { id } => vec![("type", "pdfcraft:compare-preview-done".into()), ("id", id.as_str().into())],
                E::PreviewFailed { id, error } => {
                    vec![("type", "pdfcraft:compare-preview-failed".into()), ("id", id.as_str().into()), ("error", error.as_str().into())]
                }
                E::View { id, view, building } => vec![
                    ("type", "pdfcraft:compare-view".into()),
                    ("id", id.as_str().into()),
                    (
                        "view",
                        match view {
                            pdfcraft_ui_egui::CompareView::Side => "side",
                            pdfcraft_ui_egui::CompareView::Overlay => "overlay",
                            pdfcraft_ui_egui::CompareView::OverlaySide => "overlay-side",
                        }
                        .into(),
                    ),
                    ("building", JsValue::from_bool(*building)),
                ],
            };
            if let Some(m) = message(&fields) {
                let _ = self.post(&m, None);
            }
        }

        pub fn compare_fn(&self) -> pdfcraft_ui_egui::HostCompareFn {
            let bridge = self.clone();
            Box::new(move |event| bridge.post_compare(event))
        }

        /// Tell the parent what the user changed or selected (`annotSync`).
        pub fn post_annots(&self, event: &pdfcraft_ui_egui::AnnotEvent) {
            // A big change goes out in several messages, which the page's size limits ask for.
            for (kind, body) in event.to_messages() {
                let Ok(json) = serde_json::to_string(&body) else { continue };
                let Ok(value) = js_sys::JSON::parse(&json) else { continue };
                if let Some(m) = message(&[("type", kind.into())]) {
                    // The fields of the body sit beside `type`, like every other message.
                    if let Some(fields) = value.dyn_ref::<js_sys::Object>() {
                        let _ = js_sys::Object::assign(&m, fields);
                    }
                    let _ = self.post(&m, None);
                }
            }
        }

        pub fn annots_fn(&self) -> pdfcraft_ui_egui::HostAnnotFn {
            let bridge = self.clone();
            Box::new(move |event| bridge.post_annots(event))
        }

        pub fn view_fn(&self) -> pdfcraft_ui_egui::HostViewFn {
            let bridge = self.clone();
            Box::new(move |event| bridge.post_view(event))
        }

        /// Hear `pdfcraft:goto`, `pdfcraft:highlight`, `pdfcraft:highlight-clear` and `pdfcraft:page-info` from the parent. A request that
        /// can't be read is answered with its `-failed` message when it has an `id`.
        pub fn listen_view(&self, views: pdfcraft_ui_egui::ViewInbox, ctx: egui::Context) {
            let bridge = self.clone();
            let parent = self.parent.clone();
            let origin = self.origin.clone();
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                    return;
                }
                let data = event.data();
                let Some(kind) = string(&data, "type") else { return };
                let Some(name) = kind.strip_prefix("pdfcraft:").filter(|n| matches!(*n, "goto" | "highlight" | "highlight-clear" | "page-info"))
                else {
                    return;
                };
                let parsed = js_sys::JSON::stringify(&data)
                    .ok()
                    .and_then(|s| s.as_string())
                    .ok_or_else(|| "not plain data".to_string())
                    .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).map_err(|e| e.to_string()))
                    .and_then(|value| pdfcraft_ui_egui::ViewCommand::from_json(name, &value));
                match parsed {
                    Ok(command) => {
                        if let Ok(mut q) = views.lock() {
                            q.push(command);
                        }
                        ctx.request_repaint();
                    }
                    Err(error) => {
                        let id = string(&data, "id").unwrap_or_default().chars().take(128).collect();
                        match name {
                            "goto" => bridge.post_view(&pdfcraft_ui_egui::ViewEvent::GotoFailed { id, error }),
                            "highlight" => bridge.post_view(&pdfcraft_ui_egui::ViewEvent::HighlightFailed { id, error }),
                            "page-info" => bridge.post_view(&pdfcraft_ui_egui::ViewEvent::PageInfoFailed { id, error }),
                            _ => eframe::web_sys::console::warn_1(&format!("PdfCraft: {kind} refused: {error}").into()),
                        }
                    }
                }
            });
            if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
                // The listener lives as long as the page.
                on_message.forget();
            }
        }

        /// Tell the parent about the view: `pdfcraft:opened`, `goto-done`, `goto-failed`, `highlight-done`, `highlight-failed`, `page-info-done`, `page-info-failed`, `page-changed`.
        pub fn post_view(&self, event: &pdfcraft_ui_egui::ViewEvent) {
            let (name, fields) = event.to_message();
            let mut pairs: Vec<(&str, JsValue)> = vec![("type", format!("pdfcraft:{name}").into())];
            for key in ["id", "error"] {
                if let Some(text) = fields.get(key).and_then(|v| v.as_str()) {
                    pairs.push((key, text.into()));
                }
            }
            // Numbers (`current`, `page`) and the page list of `page-info-done`.
            for key in ["current", "page"] {
                if let Some(n) = fields.get(key).and_then(|v| v.as_f64()) {
                    pairs.push((key, n.into()));
                }
            }
            if let Some(list) = fields.get("pages").and_then(|v| v.as_array()) {
                let array = js_sys::Array::new();
                for p in list {
                    let item = js_sys::Object::new();
                    for key in ["width", "height", "rotation"] {
                        if let Some(n) = p.get(key).and_then(|v| v.as_f64()) {
                            let _ = js_sys::Reflect::set(&item, &JsValue::from_str(key), &JsValue::from_f64(n));
                        }
                    }
                    array.push(&item);
                }
                pairs.push(("pages", array.into()));
            }
            if let Some(m) = message(&pairs) {
                let _ = self.post(&m, None);
            }
        }

        pub fn stamp_fn(&self) -> pdfcraft_ui_egui::HostStampFn {
            let bridge = self.clone();
            Box::new(move |event| bridge.post_stamp(event))
        }

        pub fn save_fn(&self) -> pdfcraft_ui_egui::HostSaveFn {
            let bridge = self.clone();
            Box::new(move |name: &str, bytes: &[u8], save_as: bool, pages_changed: bool| {
                let buffer = js_sys::Uint8Array::from(bytes).buffer();
                let m = message(&[
                    ("type", "pdfcraft:save".into()),
                    ("name", name.into()),
                    ("saveAs", save_as.into()),
                    ("pagesChanged", pages_changed.into()),
                    ("bytes", buffer.clone().into()),
                ])
                .ok_or_else(|| "couldn't build the message".to_string())?;
                bridge.post(&m, Some(&buffer))
            })
        }

        /// Answers `pdfcraft:request-bytes`: `pdfcraft:bytes` (`bytes` a transferred `ArrayBuffer`;
        /// `pagesChanged: true` only when it is true) or `pdfcraft:bytes-failed`.
        pub fn bytes_fn(&self) -> pdfcraft_ui_egui::HostBytesFn {
            use pdfcraft_ui_egui::BytesReply as R;
            let bridge = self.clone();
            Box::new(move |reply: &R| match reply {
                R::Bytes { id, purpose, name, bytes, pages_changed } => {
                    let buffer = js_sys::Uint8Array::from(bytes.as_slice()).buffer();
                    let mut fields = vec![
                        ("type", "pdfcraft:bytes".into()),
                        ("id", id.as_str().into()),
                        ("purpose", purpose.as_str().into()),
                        ("name", name.as_str().into()),
                        ("bytes", buffer.clone().into()),
                    ];
                    if *pages_changed {
                        fields.push(("pagesChanged", true.into()));
                    }
                    let m = message(&fields).ok_or_else(|| "couldn't build the message".to_string())?;
                    bridge.post(&m, Some(&buffer))
                }
                R::Failed { id, purpose, reason } => {
                    let m = message(&[
                        ("type", "pdfcraft:bytes-failed".into()),
                        ("id", id.as_str().into()),
                        ("purpose", purpose.as_str().into()),
                        ("reason", reason.as_str().into()),
                    ])
                    .ok_or_else(|| "couldn't build the message".to_string())?;
                    bridge.post(&m, None)
                }
            })
        }

        pub fn dirty_fn(&self) -> pdfcraft_ui_egui::HostDirtyFn {
            let bridge = self.clone();
            let mut last = false;
            Box::new(move |dirty: bool| {
                if dirty != last {
                    last = dirty;
                    if let Some(m) = message(&[("type", "pdfcraft:dirty".into()), ("dirty", dirty.into())]) {
                        let _ = bridge.post(&m, None);
                    }
                }
            })
        }
    }

    fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
        let m = js_sys::Object::new();
        for (key, value) in fields {
            js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
        }
        Some(m)
    }

    /// Read one `{ name, bytes }` file of a `pdfcraft:compare` message.
    fn compare_file(data: &JsValue, key: &str) -> Result<(String, Vec<u8>), String> {
        let file = js_sys::Reflect::get(data, &JsValue::from_str(key)).map_err(|_| format!("{key} is missing"))?;
        if !file.is_object() {
            return Err(format!("{key} must be an object"));
        }
        let name = string(&file, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "document.pdf".to_string());
        let bytes = js_sys::Reflect::get(&file, &JsValue::from_str("bytes")).unwrap_or(JsValue::UNDEFINED);
        let array = if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
            array.clone()
        } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
            js_sys::Uint8Array::new(&bytes)
        } else {
            return Err(format!("{key}.bytes must be a Uint8Array or ArrayBuffer"));
        };
        // Look at the length before copying anything into wasm memory.
        if array.length() as usize > pdfcraft_ui_egui::MAX_COMPARE_BYTES {
            return Err(format!("{key} is too large (at most {} MB)", pdfcraft_ui_egui::MAX_COMPARE_BYTES >> 20));
        }
        Ok((name.chars().take(200).collect(), array.to_vec()))
    }

    /// Read a `pdfcraft:compare`, `-preview` or `-run` message; the error is what the parent is told. A preview has no looks
    /// to read (nothing is compared yet) and a run has no files (the preview opened them).
    fn compare_request(data: &JsValue, op: pdfcraft_ui_egui::CompareOp) -> Result<pdfcraft_ui_egui::CompareCommand, String> {
        use pdfcraft_ui_egui::CompareOp;
        let id = string(data, "id").ok_or_else(|| "id must be a string".to_string())?;
        // The same rules as an open: `mode` and `policy`, and the narrowest without them.
        let scope = pdfcraft_ui_egui::open_scope(string(data, "mode").as_deref());
        let ownership = pdfcraft_ui_egui::with_policy(pdfcraft_ui_egui::Ownership { scope, ..Default::default() }, policy_overrides(data));
        let json_of = |key: &str| -> Result<Option<serde_json::Value>, String> {
            let v = js_sys::Reflect::get(data, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED);
            if op == CompareOp::Preview || v.is_undefined() || v.is_null() {
                return Ok(None);
            }
            let json = js_sys::JSON::stringify(&v).ok().and_then(|s| s.as_string()).ok_or_else(|| format!("{key} is not plain data"))?;
            serde_json::from_str::<serde_json::Value>(&json).map(Some).map_err(|e| e.to_string())
        };
        let overlay = js_sys::Reflect::get(data, &JsValue::from_str("overlay")).unwrap_or(JsValue::UNDEFINED);
        let overlay = if op == CompareOp::Preview || overlay.is_undefined() || overlay.is_null() {
            None
        } else {
            let json = js_sys::JSON::stringify(&overlay).ok().and_then(|s| s.as_string()).ok_or_else(|| "overlay is not plain data".to_string())?;
            let value = serde_json::from_str::<serde_json::Value>(&json).map_err(|e| e.to_string())?;
            Some(pdfcraft_ui_egui::overlay_style_from_json(&value)?)
        };
        let side = match json_of("side")? {
            Some(value) => pdfcraft_ui_egui::side_colors_from_json(&value)?,
            None => Default::default(),
        };
        // `view` is "side", "overlay" or "overlay-side" (anything else fails the compare); `lockView` must be a boolean.
        let view = match js_sys::Reflect::get(data, &JsValue::from_str("view")).unwrap_or(JsValue::UNDEFINED) {
            v if v.is_undefined() || v.is_null() || op == CompareOp::Preview => pdfcraft_ui_egui::CompareView::Side,
            v => match v.as_string().as_deref() {
                Some("overlay") => pdfcraft_ui_egui::CompareView::Overlay,
                Some("overlay-side") => pdfcraft_ui_egui::CompareView::OverlaySide,
                Some("side") => pdfcraft_ui_egui::CompareView::Side,
                _ => return Err("view must be \"side\", \"overlay\" or \"overlay-side\"".into()),
            },
        };
        let lock_view = match js_sys::Reflect::get(data, &JsValue::from_str("lockView")).unwrap_or(JsValue::UNDEFINED) {
            v if v.is_undefined() || v.is_null() || op == CompareOp::Preview => false,
            v => v.as_bool().ok_or("lockView must be a boolean")?,
        };
        // The files: a run has none; a compare or a preview needs both.
        let (old, new) = if op == CompareOp::Run { (Default::default(), Default::default()) } else { (compare_file(data, "old")?, compare_file(data, "new")?) };
        Ok(pdfcraft_ui_egui::CompareCommand {
            id,
            op,
            old,
            new,
            ownership,
            overlay,
            side,
            view,
            lock_view,
        })
    }

    /// Read a `pdfcraft:stamp-add` message; the error is what the parent is told.
    fn stamp_add(data: &JsValue) -> Result<pdfcraft_ui_egui::StampCommand, String> {
        let get = |key: &str| js_sys::Reflect::get(data, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED);
        let text = |key: &str| get(key).as_string().ok_or_else(|| format!("{key} must be a string"));
        let number = |key: &str| get(key).as_f64().ok_or_else(|| format!("{key} must be a number"));
        let bytes = get("bytes");
        let array = if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
            array.clone()
        } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
            js_sys::Uint8Array::new(&bytes)
        } else {
            return Err("bytes must be a Uint8Array or ArrayBuffer".into());
        };
        // Look at the length before copying anything into wasm memory.
        if array.length() as usize > pdfcraft_ui_egui::MAX_PNG_BYTES {
            return Err(format!("the stamp is too large (at most {} MB)", pdfcraft_ui_egui::MAX_PNG_BYTES >> 20));
        }
        let bytes = array.to_vec();
        Ok(pdfcraft_ui_egui::StampCommand::Add {
            id: text("id")?,
            name: text("name")?,
            bytes,
            width: number("width")?,
            height: number("height")?,
            meta: text("meta")?,
        })
    }

    /// Read a `pdfcraft:stamp-place` message; the error is what the parent is told.
    fn stamp_place(data: &JsValue) -> Result<pdfcraft_ui_egui::StampCommand, String> {
        let get = |key: &str| js_sys::Reflect::get(data, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED);
        let text = |key: &str| get(key).as_string().ok_or_else(|| format!("{key} must be a string"));
        let number = |key: &str| get(key).as_f64().ok_or_else(|| format!("{key} must be a number"));
        let given = |key: &str| !get(key).is_undefined() && !get(key).is_null();
        // A size is both sides or neither.
        let size = match (given("width"), given("height")) {
            (true, true) => Some((number("width")?, number("height")?)),
            (false, false) => None,
            _ => return Err("width and height must be given together".into()),
        };
        let page = number("page")?;
        if !(page.is_finite() && page >= 0.0 && page.fract() == 0.0 && page <= f64::from(u32::MAX)) {
            return Err("page must be a whole number from 0".into());
        }
        let bytes = get("bytes");
        let array = if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
            array.clone()
        } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
            js_sys::Uint8Array::new(&bytes)
        } else {
            return Err("bytes must be a Uint8Array or ArrayBuffer".into());
        };
        // Look at the length before copying anything into wasm memory.
        if array.length() as usize > pdfcraft_ui_egui::MAX_PNG_BYTES {
            return Err(format!("the stamp is too large (at most {} MB)", pdfcraft_ui_egui::MAX_PNG_BYTES >> 20));
        }
        Ok(pdfcraft_ui_egui::StampCommand::Place {
            id: text("id")?,
            page: page as usize,
            x: number("x")?,
            y: number("y")?,
            size,
            name: text("name")?,
            bytes: array.to_vec(),
            meta: text("meta")?,
        })
    }

    /// Read a `pdfcraft:sign-in-box` message: a `stamp-place` plus the `annotId` of the sign box it signs.
    fn sign_in_box(data: &JsValue) -> Result<pdfcraft_ui_egui::StampCommand, String> {
        let annot_id =
            js_sys::Reflect::get(data, &JsValue::from_str("annotId")).ok().and_then(|v| v.as_string()).ok_or("annotId must be a string")?;
        if annot_id.is_empty() {
            return Err("annotId must not be empty".into());
        }
        match stamp_place(data)? {
            pdfcraft_ui_egui::StampCommand::Place { id, page, x, y, size, name, bytes, meta } => {
                Ok(pdfcraft_ui_egui::StampCommand::SignIn { id, annot_id, page, x, y, size, name, bytes, meta })
            }
            _ => Err("not a sign-in message".into()),
        }
    }

    /// Read a `pdfcraft:sign-box-add` message; the error is what the parent is told.
    fn sign_box_add(data: &JsValue) -> Result<pdfcraft_ui_egui::StampCommand, String> {
        let text = |key: &str| {
            js_sys::Reflect::get(data, &JsValue::from_str(key)).ok().and_then(|v| v.as_string()).ok_or_else(|| format!("{key} must be a string"))
        };
        // `signer` is optional: missing, null or an empty string means none; any other type is an error.
        let signer = match js_sys::Reflect::get(data, &JsValue::from_str("signer")).unwrap_or(JsValue::UNDEFINED) {
            v if v.is_undefined() || v.is_null() => None,
            v => Some(v.as_string().ok_or("signer must be a string")?).filter(|s| !s.is_empty()),
        };
        // `width`/`height` come together, `x`/`y` come together; missing or null means the default. Pages count from 0.
        let get = |key: &str| js_sys::Reflect::get(data, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED);
        let given = |key: &str| !get(key).is_undefined() && !get(key).is_null();
        let number = |key: &str| get(key).as_f64().ok_or_else(|| format!("{key} must be a number"));
        let pair = |a: &str, b: &str| match (given(a), given(b)) {
            (true, true) => Ok(Some((number(a)?, number(b)?))),
            (false, false) => Ok(None),
            _ => Err(format!("{a} and {b} must be given together")),
        };
        let size = pair("width", "height")?;
        let at = pair("x", "y")?;
        let page = if given("page") {
            let p = number("page")?;
            if !(p.is_finite() && p >= 0.0 && p.fract() == 0.0 && p <= f64::from(u32::MAX)) {
                return Err("page must be a whole number from 0".into());
            }
            Some(p as usize)
        } else {
            None
        };
        Ok(pdfcraft_ui_egui::StampCommand::AddSignBox { id: text("id")?, role: text("role")?, signer, label: text("label")?, size, page, at })
    }

    /// Read a `pdfcraft:annots-*` message; the error says what was wrong with it.
    fn annots_command(kind: &str, data: &JsValue) -> Result<pdfcraft_ui_egui::AnnotCommand, String> {
        let json = js_sys::JSON::stringify(data).ok().and_then(|s| s.as_string()).ok_or("not plain data")?;
        let value = serde_json::from_str::<serde_json::Value>(&json).map_err(|e| e.to_string())?;
        pdfcraft_ui_egui::AnnotCommand::from_json(kind.trim_start_matches("pdfcraft:annots-"), &value)
    }

    /// Read `actor`, `readOnly`, `mode` and `policy.overrides` of a `pdfcraft:open` message. Missing means off; a wrong type or
    /// length is an error (the open is refused), never a way around the checks.
    fn open_ownership(data: &JsValue) -> Result<pdfcraft_ui_egui::Ownership, String> {
        let get = |key: &str| js_sys::Reflect::get(data, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED);
        let (actor, read_only) = (get("actor"), get("readOnly"));
        let actor = if actor.is_undefined() || actor.is_null() {
            None
        } else {
            let a = actor.as_string().filter(|a| (1..=200).contains(&a.encode_utf16().count()));
            Some(a.ok_or_else(|| "actor must be a string of 1 to 200 characters".to_string())?)
        };
        let read_only = if read_only.is_undefined() || read_only.is_null() {
            false
        } else {
            read_only.as_bool().ok_or_else(|| "readOnly must be a boolean".to_string())?
        };
        // Any other type than a string is "no mode", the narrowest.
        let scope = pdfcraft_ui_egui::open_scope(string(data, "mode").as_deref());
        // `annotSync` shares the annotations through the page, which needs to know whose they are.
        let flag = |key: &str| -> Result<bool, String> {
            let v = get(key);
            if v.is_undefined() || v.is_null() { Ok(false) } else { v.as_bool().ok_or_else(|| format!("{key} must be a boolean")) }
        };
        let annot_sync = flag("annotSync")?;
        // `replace: true` closes every open document (no questions) before this one opens.
        let replace = flag("replace")?;
        if annot_sync && actor.is_none() {
            return Err("annotSync needs an actor".into());
        }
        // `commentsPanel: false` hides this interface's own panel; absent means shown.
        let get_panel = get("commentsPanel");
        let hide_comments_panel = if get_panel.is_undefined() || get_panel.is_null() {
            false
        } else {
            !get_panel.as_bool().ok_or_else(|| "commentsPanel must be a boolean".to_string())?
        };
        // `maxAnnotationXfdfLength`: a whole number of UTF-16 units from the default up to the ceiling; anything else refuses the open.
        let max = get("maxAnnotationXfdfLength");
        let max_xfdf_len = if max.is_undefined() || max.is_null() {
            0
        } else {
            let (lo, hi) = (pdfcraft_ui_egui::XFDF_LEN_MIN, pdfcraft_ui_egui::XFDF_LEN_MAX);
            let n = max.as_f64().filter(|n| n.fract() == 0.0 && *n >= lo as f64 && *n <= hi as f64);
            n.ok_or_else(|| format!("maxAnnotationXfdfLength must be a whole number from {lo} to {hi}"))? as usize
        };
        let own = pdfcraft_ui_egui::Ownership { actor, read_only, scope, annot_sync, hide_comments_panel, replace, max_xfdf_len, ..Default::default() };
        Ok(pdfcraft_ui_egui::with_policy(own, policy_overrides(data)))
    }

    /// `policy.overrides` of a `pdfcraft:open` or `pdfcraft:compare` message: command id to true
    /// or false. The reading is `pdfcraft_ui_egui::policy_overrides`, on the policy as JSON.
    fn policy_overrides(data: &JsValue) -> Vec<(String, bool)> {
        let policy = js_sys::Reflect::get(data, &JsValue::from_str("policy")).unwrap_or(JsValue::UNDEFINED);
        let json = js_sys::JSON::stringify(&policy).ok().and_then(|s| s.as_string());
        let value = json.and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok());
        value.map(|v| pdfcraft_ui_egui::policy_overrides(&v)).unwrap_or_default()
    }

    fn string(data: &JsValue, key: &str) -> Option<String> {
        js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("pdfcraft-web targets wasm32: run `trunk serve` or `trunk build --release` in apps/pdfcraft-web");
}

#[cfg(test)]
mod tests {
    use super::blocks_browser_default;

    #[test]
    fn blocks_find_and_find_next() {
        assert!(blocks_browser_default(true, false, false, "f", ""));
        assert!(blocks_browser_default(true, false, false, "F", ""));
        assert!(blocks_browser_default(true, false, false, "g", ""));
        assert!(blocks_browser_default(true, false, false, "G", ""));
    }

    #[test]
    fn blocks_find_on_korean_layout_by_physical_key() {
        assert!(blocks_browser_default(true, false, false, "ㄹ", "KeyF"));
        assert!(blocks_browser_default(true, false, false, "Process", "KeyF"));
        assert!(blocks_browser_default(true, false, false, "ㅎ", "KeyG"));
        assert!(!blocks_browser_default(true, false, false, "ㅊ", "KeyC"));
    }

    #[test]
    fn blocks_either_ctrl_or_meta_but_not_both() {
        assert!(blocks_browser_default(false, true, false, "f", "KeyF"));
        assert!(blocks_browser_default(true, false, false, "g", "KeyG"));
        assert!(!blocks_browser_default(true, true, false, "f", "KeyF"));
    }

    #[test]
    fn leaves_other_keys_alone() {
        assert!(!blocks_browser_default(false, false, false, "f", "KeyF"));
        assert!(!blocks_browser_default(true, false, false, "c", "KeyC"));
        assert!(!blocks_browser_default(true, false, false, "v", "KeyV"));
        assert!(!blocks_browser_default(true, false, false, "x", "KeyX"));
        assert!(!blocks_browser_default(true, false, true, "f", "KeyF"));
    }
}
