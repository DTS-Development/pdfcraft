//! PdfCraft patch: a limit on the stream bytes one thread may decode, for callers that read
//! files they don't trust. Every stream decode passes through [`crate::object::Stream`]'s common
//! decoder, which takes what it produced (or, when it failed, the most it could have produced)
//! off the limit, so forms, shadings, functions, fonts and anything added later share one budget
//! and none has to be counted by hand. The limit only runs down. By default there is none,
//! and viewing and rendering are unchanged. Without the `std` feature there are no thread
//! locals, and so no limit.

#[cfg(feature = "std")]
std::thread_local! {
    static BYTES_LEFT: core::cell::Cell<Option<usize>> = const { core::cell::Cell::new(None) };
}

/// Limit the stream bytes this thread may decode from now on (`None` lifts the limit).
pub fn set_content_byte_budget(bytes: Option<usize>) {
    #[cfg(feature = "std")]
    BYTES_LEFT.with(|b| b.set(bytes));
    #[cfg(not(feature = "std"))]
    let _ = bytes;
}

/// What is left of the limit set by [`set_content_byte_budget`] (`None`: no limit).
pub fn content_byte_budget_left() -> Option<usize> {
    #[cfg(feature = "std")]
    return BYTES_LEFT.with(core::cell::Cell::get);
    #[cfg(not(feature = "std"))]
    None
}

/// Take `len` bytes off the limit, if there is one.
pub fn spend_content_bytes(len: usize) {
    #[cfg(feature = "std")]
    BYTES_LEFT.with(|b| b.set(b.get().map(|left| left.saturating_sub(len))));
    #[cfg(not(feature = "std"))]
    let _ = len;
}
