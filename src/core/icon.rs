//! A page's own icon (SPEC §三十八 "图标与封面").
//!
//! One list and one function. The list is the picker's whole content: a flat,
//! curated set of emoji, twelve rows of eight, chosen from what every Windows
//! install draws through `Segoe UI Emoji`. There are no categories and no
//! search — that is this slice's limit, written down rather than implied.
//!
//! The entries are written as escapes on purpose: several of them are a base
//! code point plus a variation selector, and a source file that survives one
//! editor's "tidy up the invisible characters" pass would quietly turn a cell
//! of the grid into a different-looking one.
//!
//! What a page *stores* is the emoji itself and not an index into this list, so
//! adding or reordering an entry here cannot rewrite anybody's page.

/// The picker's grid, in row-major order.
pub const PICKER: [&str; 96] = [
    // faces
    "\u{1F600}", "\u{1F604}", "\u{1F601}", "\u{1F642}", "\u{1F609}", "\u{1F60D}", "\u{1F618}", "\u{1F60E}",
    "\u{1F914}", "\u{1F610}", "\u{1F634}", "\u{1F92F}", "\u{1F62D}", "\u{1F624}", "\u{1F973}", "\u{1F631}",
    // hands
    "\u{1F44D}", "\u{1F44E}", "\u{1F44C}", "\u{270C}\u{FE0F}", "\u{1F91E}", "\u{1F64F}", "\u{1F4AA}", "\u{1F44F}",
    // hearts
    "\u{2764}\u{FE0F}", "\u{1F9E1}", "\u{1F49B}", "\u{1F49A}", "\u{1F499}", "\u{1F49C}", "\u{1F5A4}", "\u{1F4AF}",
    // marks
    "\u{2705}", "\u{274C}", "\u{26A0}\u{FE0F}", "\u{2757}", "\u{1F4A1}", "\u{1F525}", "\u{2728}", "\u{2B50}",
    // sky
    "\u{1F31E}", "\u{1F308}", "\u{2600}\u{FE0F}", "\u{1F319}", "\u{26C5}", "\u{2744}\u{FE0F}", "\u{1F30A}", "\u{1F331}",
    // plants
    "\u{1F332}", "\u{1F333}", "\u{1F335}", "\u{1F338}", "\u{1F33B}", "\u{1F340}", "\u{1F341}", "\u{1F33E}",
    // desk
    "\u{1F4DD}", "\u{1F4CC}", "\u{1F4CE}", "\u{1F4D0}", "\u{1F4DA}", "\u{1F4D6}", "\u{1F516}", "\u{1F5C2}\u{FE0F}",
    // machine
    "\u{1F4BB}", "\u{1F5A5}\u{FE0F}", "\u{2328}\u{FE0F}", "\u{1F5A8}\u{FE0F}", "\u{1F4F1}", "\u{260E}\u{FE0F}", "\u{1F4F7}", "\u{1F50B}",
    // time and space
    "\u{23F0}", "\u{23F3}", "\u{1F4C5}", "\u{1F5D2}\u{FE0F}", "\u{1F30D}", "\u{1F680}", "\u{1F6F8}", "\u{2604}\u{FE0F}",
    // travel
    "\u{2708}\u{FE0F}", "\u{1F697}", "\u{1F695}", "\u{1F699}", "\u{1F6B2}", "\u{26F5}", "\u{1F6A6}", "\u{1F5FA}\u{FE0F}",
    // play
    "\u{26BD}", "\u{1F3C0}", "\u{1F3BE}", "\u{1F3AF}", "\u{1F3B2}", "\u{1F3B8}", "\u{1F3AC}", "\u{1F3A8}",
];

/// How many emoji sit on one row of the picker. The .slint side repeats this
/// many per line, so the two have to agree — hence it lives here.
pub const PER_ROW: usize = 8;

/// The placeholder an iconless page shows: the title's first character, so a
/// page never has an empty icon slot and the slot still says something about
/// *this* page. Empty title, no placeholder.
///
/// First scalar value, not first grapheme: a title opening on a ZWJ emoji
/// sequence would split, which for a 16px slot is a cosmetic risk this slice
/// takes rather than a dependency.
pub fn initial(title: &str) -> String {
    title
        .chars()
        .find(|c| !c.is_whitespace())
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// What an icon slot shows: the page's own emoji, or that placeholder when it
/// has none. The rule lives here rather than in a binding because the two
/// halves are decided in different places — the sidebar substitutes, the
/// editor above the title does not — and a slot that silently grew a third
/// caller is the bug this function exists to prevent.
pub fn slot(stored: &str, title: &str) -> String {
    if stored.is_empty() {
        initial(title)
    } else {
        stored.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picker_is_a_whole_number_of_rows_with_no_duplicates() {
        assert_eq!(PICKER.len() % PER_ROW, 0);
        for (index, emoji) in PICKER.iter().enumerate() {
            for other in &PICKER[index + 1..] {
                assert_ne!(emoji, other, "{emoji} appears twice at {index}");
            }
        }
        // every entry is drawable text, not an empty cell in the grid
        for emoji in PICKER {
            assert!(!emoji.is_empty(), "an empty picker cell cannot be clicked");
            assert!(emoji.chars().count() <= 2, "{emoji} is not one emoji");
        }
    }

    #[test]
    fn an_iconsless_page_falls_back_to_its_title() {
        assert_eq!(initial("Getting Started"), "G");
        assert_eq!(initial("写作与中文测试"), "写");
        assert_eq!(initial("   Indented"), "I");
        assert_eq!(initial(""), "");
        assert_eq!(initial("   "), "");
    }

    #[test]
    fn a_stored_icon_beats_the_placeholder_and_an_empty_one_does_not() {
        assert_eq!(slot("\u{1F680}", "Project Atlas"), "\u{1F680}");
        assert_eq!(slot("", "Project Atlas"), "P");
        // an icon set on an untitled page still shows, and an iconless
        // untitled page shows nothing rather than a stray box
        assert_eq!(slot("\u{1F389}", ""), "\u{1F389}");
        assert_eq!(slot("", ""), "");
    }
}
