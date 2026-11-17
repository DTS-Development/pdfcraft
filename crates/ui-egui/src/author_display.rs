//! How an author's name is shown (`?authorSep=&authorIdx=` of a host-driven frame): the host keeps a
//! structured `/T` such as `userId#name#team` and wants only one piece on screen. Display only: the
//! document, the XFDF, `annots-changed` and the events keep the `/T` as it is.

use std::borrow::Cow;
use std::sync::Mutex;

/// Longest separator taken; a longer one is an input error and is ignored.
pub const MAX_SEPARATOR_CHARS: usize = 8;

static RULE: Mutex<Option<(String, usize)>> = Mutex::new(None);

/// Set the rule: show the `index`th (0-based) piece of the `/T` split at `separator`. An empty or too
/// long separator clears it.
pub fn set(separator: &str, index: usize) {
    let valid = !separator.is_empty() && separator.chars().count() <= MAX_SEPARATOR_CHARS;
    if let Ok(mut r) = RULE.lock() {
        *r = valid.then(|| (separator.to_string(), index));
    }
}

/// The name to show for `author`: the piece the rule picks, or all of it when there is no rule, no
/// such piece, or the piece is empty.
pub fn shown(author: &str) -> Cow<'_, str> {
    let Ok(rule) = RULE.lock() else { return Cow::Borrowed(author) };
    let Some((sep, index)) = rule.as_ref() else { return Cow::Borrowed(author) };
    match author.split(sep.as_str()).nth(*index).map(str::trim) {
        Some(piece) if !piece.is_empty() => Cow::Owned(piece.to_string()),
        _ => Cow::Borrowed(author),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test: the rule is process-wide, so parallel tests would see each other's.
    #[test]
    fn picks_the_piece_or_falls_back_to_the_whole_name() {
        set("", 1);
        assert_eq!(shown("u1#Kim#Team A"), "u1#Kim#Team A", "no rule");
        set("#", 1);
        assert_eq!(shown("u1#Kim#Team A"), "Kim");
        assert_eq!(shown("font-test"), "font-test", "no separator in it");
        assert_eq!(shown("u1##Team"), "u1##Team", "empty piece");
        assert_eq!(shown("u1"), "u1", "no such piece");
        set("#", 2);
        assert_eq!(shown("u1#Kim#Team A"), "Team A");
        set("123456789", 0);
        assert_eq!(shown("a#b"), "a#b", "separator too long is ignored");
        set("", 0);
    }
}
