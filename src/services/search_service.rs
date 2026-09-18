// Search use-case service (SPEC §二十, M7). Turns the raw FTS5 match list
// into one ranked hit per page with a snippet, and offers a non-blocking
// entry point: the UI thread must never wait on SQLite (docs/ARCHITECTURE.md
// hard rule 1), so `search_async` hands back a pollable handle instead of
// running the query inline.
//
// The query API lives here and on the concrete repository, *not* on
// `core::persistence::Repository` (ADR-0014): search is a capability of the
// SQLite backend, and the change contract stays exactly as written.

use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use crate::core::persistence::StorageError;
use crate::core::types::{BlockId, PageId};
use crate::storage::search_index::{Match, SearchRequest};
use crate::storage::SqliteRepository;

/// Snippet width in characters, roughly one sidebar line.
pub const SNIPPET_CHARS: usize = 80;

/// One page in the result list.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub page: PageId,
    pub title: String,
    /// Excerpt of the matching text, centred on the match.
    pub snippet: String,
    /// Block to scroll to when the hit is opened; `None` for a title hit.
    pub block: Option<BlockId>,
    /// Best bm25 among this page's matches (lower is better).
    pub score: f64,
}

pub struct SearchService {
    repo: Arc<SqliteRepository>,
}

impl SearchService {
    pub fn new(repo: Arc<SqliteRepository>) -> Self {
        SearchService { repo }
    }

    /// Arc constructor for the app layer (Track A wiring).
    pub fn new_arc(repo: Arc<SqliteRepository>) -> Arc<Self> {
        Arc::new(SearchService::new(repo))
    }

    /// Whole-workspace query by plain text (blocking; for tests and tools).
    pub fn query(&self, text: &str) -> Result<Vec<Hit>, StorageError> {
        self.search(&SearchRequest::new(text))
    }

    /// In-page find: hits restricted to one page.
    pub fn query_in_page(&self, text: &str, page: PageId) -> Result<Vec<Hit>, StorageError> {
        self.search(&SearchRequest::new(text).in_page(page))
    }

    pub fn search(&self, req: &SearchRequest) -> Result<Vec<Hit>, StorageError> {
        let matches = self.repo.search(req)?;
        Ok(rank(&matches, &terms(&req.query), req.limit))
    }

    /// Hand the query to a worker thread and return immediately. `poll`
    /// yields `None` while it runs, so the UI keeps painting. Typing a new
    /// query simply drops the old handle; the worker's answer goes unread,
    /// which costs one wasted query and no blocking.
    pub fn search_async(&self, req: SearchRequest) -> PendingSearch {
        let repo = self.repo.clone();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("quire-search".into())
            .spawn(move || {
                let result = repo
                    .search(&req)
                    .map(|matches| rank(&matches, &terms(&req.query), req.limit));
                let _ = tx.send(result);
            });
        PendingSearch {
            rx: spawned.ok().map(|_| rx),
        }
    }
}

/// Result of `SearchService::search_async` that may still be running.
pub struct PendingSearch {
    rx: Option<Receiver<Result<Vec<Hit>, StorageError>>>,
}

impl PendingSearch {
    /// Non-blocking: `None` while pending, `Some(Ok(hits))` when done.
    /// `Some(Err)` means the query failed or the worker never started.
    pub fn poll(&self) -> Option<Result<Vec<Hit>, StorageError>> {
        let rx = self.rx.as_ref()?;
        match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(StorageError::Sql("search worker died".into())))
            }
        }
    }

    /// Block until the worker answers. Only for tests and CLI tools.
    pub fn wait(self) -> Result<Vec<Hit>, StorageError> {
        let rx = self.rx.ok_or_else(|| StorageError::Sql("search worker died".into()))?;
        rx.recv().map_err(|_| StorageError::Sql("search worker died".into()))?
    }
}

/// Query words as the user typed them (unsegmented), for snippet centring.
/// CJK characters count as alphanumeric to `char`, so they survive the trim.
fn terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_string()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// One page's aggregated match.
struct Agg {
    page: PageId,
    title: String,
    score: f64,
    /// Score of the match the snippet text came from.
    chosen: f64,
    block: Option<BlockId>,
    text: String,
}

/// Collapse per-source matches into per-page hits, best first. A block match
/// always outranks a bare title match for the snippet slot, because jumping
/// to a block is what the search panel does.
fn rank(matches: &[Match], terms: &[String], limit: usize) -> Vec<Hit> {
    let mut aggs: Vec<Agg> = Vec::new();
    for m in matches {
        let index = aggs.iter().position(|a| a.page == m.page);
        let agg = match index {
            Some(i) => &mut aggs[i],
            None => {
                aggs.push(Agg {
                    page: m.page,
                    title: m.page_title.clone(),
                    score: m.score,
                    chosen: f64::MAX,
                    block: None,
                    text: String::new(),
                });
                aggs.last_mut().unwrap()
            }
        };
        agg.score = agg.score.min(m.score);
        let take = match (agg.block.is_some(), m.block.is_some()) {
            (false, true) => true,
            (true, false) => false,
            _ => m.score < agg.chosen,
        };
        if take {
            agg.chosen = m.score;
            agg.block = m.block;
            agg.text = m.text.clone();
        }
    }
    aggs.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.page.cmp(&b.page))
    });
    aggs.truncate(limit);
    aggs.into_iter()
        .map(|a| Hit {
            page: a.page,
            snippet: snippet(if a.text.is_empty() { &a.title } else { &a.text }, terms),
            title: a.title,
            block: a.block,
            score: a.score,
        })
        .collect()
}

/// Window `text` around the first query word that occurs in it.
pub fn snippet(text: &str, terms: &[String]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let center = terms
        .iter()
        .filter_map(|term| find(&chars, &term.chars().collect::<Vec<_>>()))
        .min()
        .unwrap_or(0);
    let start = center.saturating_sub(SNIPPET_CHARS / 3);
    let end = (start + SNIPPET_CHARS).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// Case-insensitive char-window search; `None` when the term is absent.
fn find(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| {
        hay[i..i + needle.len()]
            .iter()
            .zip(needle)
            .all(|(h, n)| h == n || h.to_ascii_lowercase() == n.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(page: u64, block: Option<u64>, score: f64, text: &str) -> Match {
        Match {
            page: PageId(page),
            page_title: format!("Page {page}"),
            is_title: block.is_none(),
            block: block.map(BlockId),
            text: text.into(),
            score,
        }
    }

    #[test]
    fn snippet_centers_on_the_term() {
        let text = "a".repeat(40) + " 关键词出现在这里 " + &"b".repeat(200);
        let s = snippet(&text, &["关键词".into()]);
        assert!(s.starts_with('…'), "front not elided: {s}");
        assert!(s.ends_with('…'), "back not elided: {s}");
        assert!(s.contains("关键词"), "snippet lost the term: {s}");
        assert!(s.chars().count() <= SNIPPET_CHARS + 2);
    }

    #[test]
    fn snippet_of_short_text_is_the_whole_text() {
        assert_eq!(snippet("short note", &["note".into()]), "short note");
        assert_eq!(snippet("", &["x".into()]), "");
    }

    #[test]
    fn terms_keep_cjk_and_drop_punctuation() {
        assert_eq!(terms("  quick!  中文 "), ["quick", "中文"]);
        assert_eq!(terms("—— —"), Vec::<String>::new());
    }

    #[test]
    fn ranking_is_per_page_best_first_and_block_preferring() {
        let hits = rank(
            &[
                m(1, None, -1.0, "Page 1"),
                m(2, Some(21), -2.5, "gamma"),
                m(1, Some(10), -2.0, "alpha beta"),
                m(1, Some(11), -3.0, "alpha alpha alpha"),
            ],
            &["alpha".into()],
            10,
        );
        // page 1 leads with its best block (-3.0) and the snippet follows it
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].page.0, hits[0].block), (1, Some(BlockId(11))));
        assert_eq!(hits[0].score, -3.0);
        assert!(hits[0].snippet.contains("alpha alpha"));
        assert_eq!((hits[1].page.0, hits[1].block), (2, Some(BlockId(21))));
    }

    #[test]
    fn title_only_page_snippets_its_title() {
        let hits = rank(&[m(7, None, -1.0, "Getting Started")], &["getting".into()], 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].block, None);
        assert_eq!(hits[0].snippet, "Getting Started");
    }

    #[test]
    fn limit_caps_the_page_list() {
        let all: Vec<Match> = (1..=5).map(|i| m(i, Some(i * 10), -1.0, "x")).collect();
        assert_eq!(rank(&all, &["x".into()], 3).len(), 3);
    }
}
