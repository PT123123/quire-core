// The shape of a date atom's payload (SPEC §四十, ADR-0050).
//
// `@date` stores **ISO calendar text** — `YYYY-MM-DD`, ten bytes, no time and
// no zone — and this module is the one place that decides what that means.
// Two callers have to agree exactly: the importer, which decides whether a
// `@[…]` with no target is a date or prose that happens to start with `@[`,
// and the picker, which writes the atom's first value. A format that lived in
// two string literals would drift the first time either side was touched, and
// the failure would be silent — the characters stay on screen and the atom
// quietly becomes text.
//
// Why no time and no zone: this is a single-user, offline, local-first
// document. A timestamp would record the instant of *pressing the key*, which
// is not what the writer meant by "the 22nd", and a zone would make the same
// file read differently on another machine. The ten digits are what survive ten
// years and what any other renderer prints back unchanged.

use std::time::{SystemTime, UNIX_EPOCH};

/// `YYYY-MM-DD` and nothing else. Byte-wise on purpose: a multi-byte character
/// can never be an ASCII digit, so `len() == 10` plus the two dashes is the
/// entire rule and no char boundary can be split.
pub fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Days since 1970-01-01 → (year, month 1-12, day), Hinnant's
/// `civil_from_days`. Valid for every date a document will hold, and exact at
/// leap days and century rules — which is where a hand-rolled version fails.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// One calendar date as the payload text. Zero-padded, so the bytes sort the
/// way the dates do and `is_iso_date` accepts the result for every year
/// (year 999 and below would not pad to four digits; nothing else can).
pub fn to_iso(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today, UTC, as the atom's payload. UTC rather than local time on purpose:
/// the alternative needs the OS zone, and a notebook's "today" is the same
/// string as its file timestamps' day. The writer can always edit the atom's
/// text to say another day — the atom is a span, not a locked field.
pub fn today_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    to_iso(secs.div_euclid(86_400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_format_is_exact() {
        assert!(is_iso_date("2026-09-22"));
        assert!(is_iso_date("0000-01-01"));
        assert!(!is_iso_date("2026-9-22"));
        assert!(!is_iso_date("2026-09-22T00:00"));
        assert!(!is_iso_date("2026-09-2２")); // full-width digit
        assert!(!is_iso_date("Project Atlas"));
        assert!(!is_iso_date(""));
        assert!(!is_iso_date("2026/09/22"));
    }

    #[test]
    fn epoch_conversion_survives_the_usual_traps() {
        assert_eq!(to_iso(0), "1970-01-01");
        // a leap day, and the day after it
        assert_eq!(to_iso(19_782), "2024-02-29");
        assert_eq!(to_iso(19_783), "2024-03-01");
        // the 1900 century rule (not a leap year) and 2000 (one)
        assert_eq!(to_iso(-25_567), "1900-01-01");
        assert_eq!(to_iso(11_016), "2000-02-29");
        // negative days, i.e. before the epoch
        assert_eq!(to_iso(-1), "1969-12-31");
    }

    #[test]
    fn today_is_what_the_format_says_it_is() {
        let t = today_iso();
        assert!(is_iso_date(&t), "{t} is not the format the importer accepts");
        assert!(t.starts_with("20"), "{t} is far from any clock this app sees");
    }
}
