// What an embed card can say about a link without touching the network
// (SPEC §三十七 批次 C, ADR-0040).
//
// SPEC §二 and §三十三 rule out a WebView and a JS runtime, so an embed is a
// *card*: who the link belongs to, the address, and one button that hands the
// address to the system browser. Everything here is a string function over the
// stored url — no fetch, no favicon, no oEmbed, nothing that has to be kept
// alive or invalidated, which is what keeps a card cheaper than the iframe it
// stands in for.

/// The card's headline: the provider the host belongs to, or the host itself,
/// or "Link" when there is neither. Never empty, because the row has to read as
/// a card even before anything is typed into it.
pub fn describe(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return "Embed".into();
    }
    if trimmed.to_ascii_lowercase().starts_with("mailto:") {
        return "Email".into();
    }
    let (host, path) = split(trimmed);
    if host.is_empty() {
        return "Link".into();
    }
    // google.com is one host with several products behind it, and the product
    // is spelled either as the subdomain (maps.google.com) or as the first path
    // segment (google.com/maps) — both are real urls people paste.
    if ends_at(&host, "google.com") {
        let sub = host.strip_suffix(".google.com").unwrap_or("");
        let product = if sub.is_empty() {
            path.split('/').next().unwrap_or("")
        } else {
            sub.split('.').next().unwrap_or("")
        };
        let name = match product {
            "maps" => "Google Maps",
            "drive" => "Google Drive",
            "docs" => "Google Docs",
            "calendar" => "Google Calendar",
            "photos" => "Google Photos",
            _ => "Google",
        };
        return name.into();
    }
    for (suffix, name) in PROVIDERS {
        if ends_at(&host, suffix) {
            return (*name).into();
        }
    }
    host
}

/// The address as the system should be asked to open it: a bare domain gets the
/// scheme the link dialog would have given it, so "example.com/a" opens the
/// page rather than failing as a relative path.
pub fn with_scheme(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.contains("://") || trimmed.starts_with("mailto:") {
        return trimmed.to_string();
    }
    format!("https://{trimmed}")
}

/// `host.ends_at("youtube.com")` is true for the host itself and for its
/// subdomains — and false for `evilyoutube.com`, which a plain suffix test
/// would happily call YouTube.
fn ends_at(host: &str, suffix: &str) -> bool {
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

/// The authority's host (no userinfo, no port, no `www.`) and the first path
/// segments, lowercased. Whatever cannot be read as a host comes back empty.
fn split(url: &str) -> (String, String) {
    let after_scheme = url.splitn(2, "://").nth(1).unwrap_or(url);
    let before_query = after_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let mut bits = before_query.splitn(2, '/');
    let authority = bits.next().unwrap_or("");
    let path = bits.next().unwrap_or("");
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    // A space is never legal in a host, and it is what tells a line of prose
    // from an address the user typed without a scheme — the card must not
    // promote "some site" into a provider name.
    if host.split_whitespace().count() != 1 {
        return (String::new(), path.to_ascii_lowercase());
    }
    // lowercased *before* the prefix is dropped — a `WWW.` in the case a user
    // actually types is still the same host.
    (
        host.to_ascii_lowercase()
            .trim_start_matches("www.")
            .to_string(),
        path.to_ascii_lowercase(),
    )
}

/// The only addresses the app will hand to the system. A link's target is data
/// that arrives from a file — an import, a paste, a LAN pull — and the shell
/// will *run* a path as readily as it will open a url, so anything that is not
/// an address a browser understands stays in the document and off the command
/// line. `quire://` targets are resolved earlier, by the app itself.
pub fn is_openable(url: &str) -> bool {
    let t = url.trim().to_ascii_lowercase();
    t.starts_with("http://") || t.starts_with("https://") || t.starts_with("mailto:")
}

/// Host suffix to card headline. Only the names a link actually carries in a
/// note — the fallback is the host itself, so an unlisted site still gets a
/// card that says where it points.
const PROVIDERS: &[(&str, &str)] = &[
    ("youtube.com", "YouTube"),
    ("youtu.be", "YouTube"),
    ("bilibili.com", "Bilibili"),
    ("vimeo.com", "Vimeo"),
    ("dailymotion.com", "Dailymotion"),
    ("figma.com", "Figma"),
    ("canva.com", "Canva"),
    ("codepen.io", "CodePen"),
    ("github.com", "GitHub"),
    ("gitlab.com", "GitLab"),
    ("notion.so", "Notion"),
    ("notion.site", "Notion"),
    ("airtable.com", "Airtable"),
    ("trello.com", "Trello"),
    ("miro.com", "Miro"),
    ("x.com", "X"),
    ("twitter.com", "X"),
    ("linkedin.com", "LinkedIn"),
    ("spotify.com", "Spotify"),
    ("wikipedia.org", "Wikipedia"),
    ("amazon.com", "Amazon"),
];

#[cfg(test)]
mod tests {
    use super::{describe, is_openable, split, with_scheme};

    #[test]
    fn known_hosts_name_their_provider() {
        assert_eq!(describe("https://www.youtube.com/watch?v=abc"), "YouTube");
        assert_eq!(describe("https://youtu.be/abc"), "YouTube");
        assert_eq!(describe("https://figma.com/file/1234"), "Figma");
        assert_eq!(describe("https://x.com/user/status/1"), "X");
        assert_eq!(describe("https://github.com/a/b"), "GitHub");
    }

    #[test]
    fn a_subdomain_is_the_same_provider_and_a_lookalike_is_not() {
        assert_eq!(describe("https://player.bilibili.com/x"), "Bilibili");
        // the whole-label test, which is the one that matters: a suffix match
        // alone would let this card claim to be YouTube.
        assert_eq!(describe("https://evilyoutube.com/x"), "evilyoutube.com");
        assert_eq!(describe("https://notyoutube.com/x"), "notyoutube.com");
    }

    #[test]
    fn google_is_its_products() {
        assert_eq!(describe("https://maps.google.com/?q=x"), "Google Maps");
        assert_eq!(describe("https://google.com/maps/place/x"), "Google Maps");
        assert_eq!(
            describe("https://www.google.com/maps/place/x"),
            "Google Maps"
        );
        assert_eq!(
            describe("https://drive.google.com/file/d/1"),
            "Google Drive"
        );
        assert_eq!(
            describe("https://docs.google.com/document/d/1"),
            "Google Docs"
        );
        assert_eq!(describe("https://google.com/search?q=x"), "Google");
        assert_eq!(describe("https://www.google.com/x"), "Google");
    }

    #[test]
    fn an_unlisted_host_is_itself() {
        assert_eq!(describe("https://example.org/a/b"), "example.org");
        assert_eq!(describe("http://localhost:8080/x"), "localhost");
        assert_eq!(describe("https://user:pw@site.com/x"), "site.com");
    }

    #[test]
    fn nothing_to_show_still_reads_as_a_card() {
        assert_eq!(describe(""), "Embed");
        assert_eq!(describe("   "), "Embed");
        // prose is not a host, and the card says so rather than inventing one
        assert_eq!(describe("a line of prose"), "Link");
        assert_eq!(describe("mailto:someone@example.com"), "Email");
    }

    #[test]
    fn the_split_drops_everything_the_card_does_not_show() {
        assert_eq!(
            split("HTTPS://WWW.Site.COM:8443/Path/?Q=1#frag"),
            ("site.com".into(), "path/".into())
        );
        assert_eq!(split("example.com"), ("example.com".into(), "".into()));
    }

    #[test]
    fn a_bare_domain_gets_the_scheme_the_dialog_would_give_it() {
        assert_eq!(with_scheme("example.com/a"), "https://example.com/a");
        assert_eq!(with_scheme("https://example.com"), "https://example.com");
        assert_eq!(with_scheme("http://example.com"), "http://example.com");
        assert_eq!(with_scheme("mailto:a@b.c"), "mailto:a@b.c");
        assert_eq!(with_scheme("  "), "");
    }

    #[test]
    fn only_an_address_a_browser_understands_leaves_the_app() {
        assert!(is_openable("https://example.com"));
        assert!(is_openable("HTTP://EXAMPLE.COM/x"));
        assert!(is_openable("mailto:a@b.c"));
        // the shapes a document can carry that the shell would *run* rather
        // than open: a local path, a share, an installed protocol
        assert!(!is_openable("C:\\Windows\\System32\\calc.exe"));
        assert!(!is_openable("\\\\share\\folder\\thing.exe"));
        assert!(!is_openable("file:///C:/Users/ted/notes.md"));
        assert!(!is_openable("javascript:alert(1)"));
        assert!(!is_openable(""));
        assert!(!is_openable("example.com/a"));
    }
}
