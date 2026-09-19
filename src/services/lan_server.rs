// LAN sharing server (M8): a dependency-free HTTP/1.1 endpoint that lets
// another Quire (or any browser) on the same network pull the workspace as
// Markdown. Design notes:
//
// - std::net::TcpListener, one thread per connection, HTTP/1.0 close-per-
//   request semantics — no async runtime, no new crates (SPEC dependency
//   discipline).
// - Read-only by construction: there is no write endpoint. A pull is driven
//   by the OTHER side's client, which imports into its own database.
// - Bound to 0.0.0.0 only while sharing is enabled (off by default); the
//   endpoint serves whatever is committed to SQLite at request time.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use crate::core::types::PageId;use crate::core::persistence::Repository;
use crate::storage::SqliteRepository;

pub const DEFAULT_PORT: u16 = 5877;

/// What the server needs: read-only access to the persisted state.
pub struct LanServer {
    repo: Arc<SqliteRepository>,
    port: u16,
}

impl LanServer {
    pub fn new(repo: Arc<SqliteRepository>, port: u16) -> Self {
        LanServer { repo, port }
    }

    /// Bind 0.0.0.0:<port>. Tests bind port 0 and read the real port via
    /// `listener.local_addr()`.
    pub fn bind(&self) -> Result<TcpListener, String> {
        TcpListener::bind(("0.0.0.0", self.port)).map_err(|e| e.to_string())
    }

    /// Serve forever on an already-bound listener (caller spawns a thread).
    pub fn serve(&self, listener: TcpListener) {
        eprintln!("quire: LAN sharing on {}", listener.local_addr().unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap()));
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let repo = self.repo.clone();
            std::thread::spawn(move || {
                let _ = handle_connection(stream, &repo);
            });
        }
    }
}

fn handle_connection(stream: TcpStream, repo: &Arc<SqliteRepository>) -> std::io::Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
    }

    if method != "GET" {
        return respond(&stream, 405, "method not allowed (read-only share)");
    }

    let path = path.split('?').next().unwrap_or(path);
    match path {
        "/" | "/api" => respond(
            &stream,
            200,
            "Quire LAN share\n\nGET /api/pages         page list (id<TAB>title)\nGET /api/page/<id>.md single page as Markdown\nGET /api/export      whole workspace (QUIRE PAGE framing)\n",
        ),
        "/api/pages" => {
            let Ok(state) = repo.load() else {
                return respond(&stream, 500, "workspace load failed");
            };
            let ids: Vec<i64> = state.pages.iter().map(|p| p.id.0 as i64).collect();
            let mut out = String::new();
            for p in &state.pages {
                let parent_note = match p.parent {
                    Some(pid) if ids.contains(&(pid.0 as i64)) => {
                        format!("\tunder #{}", pid.0)
                    }
                    _ => String::new(),
                };
                out.push_str(&format!(
                    "{}\t{}{}\n",
                    p.id.0,
                    p.title.replace(['\n', '\t'], " "),
                    parent_note
                ));
            }
            respond(&stream, 200, &out)
        }
        p if p.starts_with("/api/page/") => {
            let id_part = p
                .strip_prefix("/api/page/")
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<i64>().ok());
            let Some(id) = id_part else {
                return respond(&stream, 400, "bad page id");
            };
            let Ok(state) = repo.load() else {
                return respond(&stream, 500, "workspace load failed");
            };
            let pid = PageId(id as u64);
            let Some(page) = state.pages.iter().find(|p| p.id == pid) else {
                return respond(&stream, 404, "no such page");
            };
            let blocks: Vec<crate::core::Block> = state
                .blocks
                .iter()
                .filter(|b| b.page == pid)
                .cloned()
                .collect();
            let md = crate::services::export_service::export_page(&blocks);
            let body = format!("# {}\n\n{}", page.title, md);
            respond(&stream, 200, &body)
        }
        "/api/export" => {
            let Ok(state) = repo.load() else {
                return respond(&stream, 500, "workspace load failed");
            };
            respond(&stream, 200, &frame_workspace(&state))
        }
        _ => respond(&stream, 404, "not found"),
    }
}

/// Whole-workspace framing: one boundary line per page, Markdown body after.
/// Titles are sanitized so they can never contain the boundary marker.
pub fn frame_workspace(state: &crate::core::PersistedState) -> String {
    let mut out = String::new();
    fn emit(
        state: &crate::core::PersistedState,
        parent: Option<PageId>,
        depth: usize,
        out: &mut String,
    ) {
        let mut kids: Vec<&crate::core::Page> = state
            .pages
            .iter()
            .filter(|p| p.parent == parent)
            .collect();
        kids.sort_by_key(|p| p.order);
        for p in kids {
            let prefix: String = "> ".repeat(depth.min(3));
            let title = format!("{}{}", prefix, p.title.replace(['\n', '>'], " "));
            let blocks: Vec<crate::core::Block> = state
                .blocks
                .iter()
                .filter(|b| b.page == p.id)
                .cloned()
                .collect();
            let md = crate::services::export_service::export_page(&blocks);
            out.push_str(&format!("<<<QUIRE PAGE: {}>>>\n", title));
            out.push_str(&md);
            if !md.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("<<<QUIRE END>>>\n");
            emit(state, Some(p.id), depth + 1, out);
        }
    }
    emit(state, None, 0, &mut out);
    out
}

/// Split a framed workspace back into (title, markdown) pairs.
pub fn unframe_workspace(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut title: Option<String> = None;
    let mut buf = String::new();
    for line in body.lines() {
        if let Some(t) = line
            .strip_prefix("<<<QUIRE PAGE: ")
            .and_then(|s| s.strip_suffix(">>>"))
        {
            if let Some(done) = title.take() {
                out.push((done, buf.trim().to_string()));
                buf.clear();
            }
            title = Some(t.to_string());
        } else if line == "<<<QUIRE END>>>" {
            if let Some(done) = title.take() {
                out.push((done, buf.trim().to_string()));
                buf.clear();
            }
        } else if title.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(done) = title.take() {
        out.push((done, buf.trim().to_string()));
    }
    out
}

fn respond(mut stream: &TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.0 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{BlockId, MarkKind};
    use crate::core::persistence::Repository;
    use crate::services::lan_client::pull_workspace;
    use std::io::Read;
    use crate::core::types::{Block, BlockKind, ColorKind, Mark, OrderKey};
    use std::collections::HashMap;

    fn seeded_repo() -> Arc<SqliteRepository> {
        let repo = SqliteRepository::in_memory().unwrap();
        let mut state = crate::core::PersistedState::default();
        let page = crate::core::Page {
            id: PageId(7),
            title: "Shared page".into(),
            parent: None,
            order: OrderKey::FIRST,
            favorite: false,
            expanded: false,
        };
        state.pages.push(page);
        state.blocks.push(Block {
            id: BlockId(1),
            page: PageId(7),
            parent: None,
            order: OrderKey::FIRST,
            kind: BlockKind::Paragraph,
            text: "hello from the lan".into(),
            checked: false,
            marks: vec![Mark { start: 0, end: 5, kind: MarkKind::Bold, url: String::new() }],
            color: ColorKind::Default,
            background: ColorKind::Default,
            page_ref: None,
        });
        state.settings.insert("theme".into(), "dark".into());
        repo.replace_all(&state).unwrap();
        Arc::new(repo)
    }

    fn spawn_server() -> (u16, Arc<SqliteRepository>) {
        let repo = seeded_repo();
        let server = LanServer::new(repo.clone(), 0);
        let listener = server.bind().unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || server.serve(listener));
        (port, repo)
    }

    fn get(port: u16, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.0\r\nHost: t\r\n\r\n").as_bytes())
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status: u16 = response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = response
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or("")
            .to_string();
        (status, body)
    }

    #[test]
    fn pages_list_and_page_markdown_are_served() {
        let (port, _repo) = spawn_server();
        let (status, body) = get(port, "/api/pages");
        assert_eq!(status, 200);
        assert!(body.contains("7\tShared page"), "list line: {body}");

        let (status, body) = get(port, "/api/page/7.md");
        assert_eq!(status, 200);
        assert!(body.contains("# Shared page"));
        // the seeded bold mark renders as **hello** — assert format-agnostically
        assert!(body.contains("hello") && body.contains("from the lan"));

        let (status, _) = get(port, "/api/page/999.md");
        assert_eq!(status, 404);
    }

    #[test]
    fn export_framing_round_trips_through_the_client() {
        let (port, _) = spawn_server();
        let (status, body) = get(port, "/api/export");
        assert_eq!(status, 200);
        let pages = unframe_workspace(&body);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].0, "Shared page");
        assert!(pages[0].1.contains("hello") && pages[0].1.contains("from the lan"));
    }

    #[test]
    fn client_pull_parses_the_framed_export() {
        let (port, _) = spawn_server();
        let pages = pull_workspace(&format!("http://127.0.0.1:{port}")).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].0, "Shared page");
        assert!(pages[0].1.contains("hello"));
    }

    #[test]
    fn non_get_is_rejected() {
        let (port, _) = spawn_server();
        let (status, _) = get(port, "/api/pages");
        let _ = status;
        // POST via raw socket
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"POST /api/pages HTTP/1.0\r\nHost: t\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.0 405"));
    }

    // keep the unused-import surface honest
    #[allow(dead_code)]
    fn _types_used(m: &HashMap<u8, u8>, mk: &MarkKind) {
        let _ = m;
        let _ = matches!(mk, MarkKind::Bold);
    }
}
