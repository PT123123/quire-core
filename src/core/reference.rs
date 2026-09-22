// References between things inside one library (SPEC §四十, ADR-0026).
//
// A reference is always stored as **the id of what it points at**, never as
// its title or its address: a Page block keeps `blocks.page_ref`, and an
// inline `@page mention` keeps the same address inside its mark's payload.
// The title a reader sees is looked up when the page is drawn, which is the
// whole mechanism behind SPEC §四十's "页面别名" — renaming a page moves every
// chip that points at it without a single write, and there is no second copy
// that could fall out of step.
//
// This module is the one place that knows the *spelling* of that address. The
// export / import channels and the projection both go through it, so a form
// that changes here changes everywhere at once rather than in four string
// literals.

use crate::core::types::PageId;

/// `quire://page/<id>` — the address a page reference is written as. It is
/// also a link a reader can follow, which is why the mention chip's click and
/// a `Link to page` mark end up on the same navigation path
/// (`controller::on_open_link`), and why a hand-written `[Atlas](quire://page/12)`
/// in an exported file comes back as a real reference.
pub const PAGE_SCHEME: &str = "quire://page/";

/// The address for one page.
pub fn page_uri(id: PageId) -> String {
    format!("{PAGE_SCHEME}{}", id.as_u64())
}

/// The page an address names, or `None` when the string is not this kind of
/// address at all. A reference whose id names no page in this library is
/// **not** an error here — the caller decides what a dangling reference looks
/// like (ADR-0029): loading must not fail because a page is gone.
pub fn page_of(uri: &str) -> Option<PageId> {
    let rest = uri.strip_prefix(PAGE_SCHEME)?;
    rest.parse::<u64>().ok().map(PageId)
}

/// The byte offset of the `@` that currently wants a picker, or `None`.
///
/// The rule is the one every mention UI settles on, written down so the
/// trigger and the picker cannot drift:
///
/// * the **last** `@` in the text is the candidate (the caret is at the end of
///   the line in this editor, so the last one is the live one);
/// * nothing after it may be whitespace — a space is the writer saying "that
///   was just an at sign", and it is also what closes the popup;
/// * it must start a word, so `me@example.com` does not fire;
/// * `\@` is an escape: the writer means the character.
///
/// An `@` that fails any of these is ordinary prose, and the caller keeps the
/// characters as they are — nothing here consumes them.
pub fn trigger_at(text: &str) -> Option<usize> {
    let at = text.rfind('@')?;
    if text[at + 1..].chars().any(char::is_whitespace) {
        return None;
    }
    if at > 0 && text[..at].ends_with('\\') {
        return None;
    }
    match text[..at].chars().next_back() {
        None => Some(at),
        Some(c) if c.is_whitespace() || !c.is_alphanumeric() => Some(at),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_round_trips_and_only_this_kind_of_address_parses() {
        assert_eq!(page_uri(PageId(12)), "quire://page/12");
        assert_eq!(page_of("quire://page/12"), Some(PageId(12)));
        assert_eq!(page_of("quire://block/12"), None);
        assert_eq!(page_of("https://example.com/quire://page/12"), None);
        assert_eq!(page_of("quire://page/"), None);
        assert_eq!(page_of("quire://page/twelve"), None);
        assert_eq!(page_of(""), None);
    }

    #[test]
    fn the_trigger_is_the_last_at_that_starts_a_word() {
        assert_eq!(trigger_at("@"), Some(0));
        assert_eq!(trigger_at("@Pro"), Some(0));
        assert_eq!(trigger_at("see @Pro"), Some(4));
        // two triggers: the live one is the later
        assert_eq!(trigger_at("@one @two"), Some(5));
        // a space ends it — the writer meant the character
        assert_eq!(trigger_at("@Pro Atlas"), None);
        assert_eq!(trigger_at("see @ "), None);
        // an address is not a mention
        assert_eq!(trigger_at("me@example.com"), None);
        assert_eq!(trigger_at("\\@escaped"), None);
        assert_eq!(trigger_at("nothing here"), None);
        assert_eq!(trigger_at(""), None);
    }
}
