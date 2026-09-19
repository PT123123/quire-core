// LAN pull client (M8): fetches a framed workspace from another Quire's
// share endpoint over plain HTTP/1.0 and splits it back into pages.
// Dependency-free like the server: one TcpStream, one GET, read to EOF.

use std::io::{Read, Write};
use std::net::TcpStream;

use crate::services::lan_server::unframe_workspace;

/// GET `url` (http://host:port/api/export) and return (title, markdown)
/// pairs. `url` missing a path gets /api/export appended.
pub fn pull_workspace(url: &str) -> Result<Vec<(String, String)>, String> {
    let body = http_get(url)?;
    Ok(unframe_workspace(&body))
}

/// Minimal HTTP/1.0 GET: send request, read status + headers + body to EOF
/// (the server closes per request). Returns the body on 200, else an error
/// carrying the status line.
pub fn http_get(url: &str) -> Result<String, String> {
    let (host, port, path) = parse_url(url)?;
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nUser-Agent: quire-lan\r\nAccept: text/plain\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read: {e}"))?;

    let mut lines = response.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body_start = response
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .ok_or("malformed response (no header terminator)")?;
    let body = response[body_start..].to_string();
    if status != 200 {
        let snippet: String = body.chars().take(80).collect();
        return Err(format!("HTTP {status}: {snippet}"));
    }
    Ok(body)
}

/// Split "http://host:port/path" (host may be an IP or hostname; port
/// defaults to 5877 when absent).
fn parse_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("only http:// URLs are supported for LAN pulls")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/api/export"),
    };
    let (host, port) = match hostport.rfind(':') {
        Some(i) => (
            &hostport[..i],
            hostport[i + 1..]
                .parse::<u16>()
                .map_err(|_| "bad port in URL")?,
        ),
        None => (hostport, crate::services::lan_server::DEFAULT_PORT),
    };
    if host.is_empty() {
        return Err("empty host in URL".into());
    }
    Ok((host.to_string(), port, path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_forms() {
        let (h, p, path) = parse_url("http://192.168.1.5:5877/api/export").unwrap();
        assert_eq!((h.as_str(), p, path.as_str()), ("192.168.1.5", 5877, "/api/export"));
        let (h, p, path) = parse_url("http://my-pc.local").unwrap();
        assert_eq!((h.as_str(), p, path.as_str()), ("my-pc.local", 5877, "/api/export"));
        assert!(parse_url("ftp://x").is_err());
        assert!(parse_url("http://host:99999").is_err());
    }

    #[test]
    fn unframe_round_trips_titles_and_bodies() {
        let body = "<<<QUIRE PAGE: Top>>>\n# Top\n\n- a\n<<<QUIRE END>>>\n\
                    <<<QUIRE PAGE: >> Child>>>\nsome text\n<<<QUIRE END>>>\n";
        let pages = unframe_workspace(body);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].0, "Top");
        assert!(pages[0].1.contains("- a"));
        assert_eq!(pages[1].0, ">> Child"); // '>' sanitized away at frame time
        assert_eq!(pages[1].1, "some text");
    }
}
