// The client halves: a blocking HTTP/1.0 request good enough for the four
// endpoints the sync server speaks, and the UDP discovery pair (an announcer
// that broadcasts this device's info, a listener that reports the peers it
// hears). Same dependency discipline as `services::lan_server`: std::net and
// nothing else, so the Android `.so` links it without a TLS story.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use super::engine::DeviceInfo;
use super::DISCOVERY_PORT;

fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    let addr = (host, port);
    let stream = TcpStream::connect_timeout(
        &std::net::ToSocketAddrs::to_socket_addrs(&addr)
            .map_err(|e| e.to_string())?
            .next()
            .ok_or_else(|| format!("{host}:{port} resolved to nothing"))?,
        timeout,
    )
    .map_err(|e| format!("connect {host}:{port}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .ok();
    Ok(stream)
}

/// One GET. The sync server always answers with a Content-Length body, so
/// the response is read to that length rather than to EOF.
pub fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String> {
    let mut stream = connect(host, port, timeout)?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    read_response(stream)
}

/// One POST with a UTF-8 body; the answer is read as text.
pub fn http_post(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let mut stream = connect(host, port, timeout)?;
    let req = format!(
        "POST {path} HTTP/1.0\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let (status, bytes) = read_response(stream)?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

fn read_response(stream: TcpStream) -> Result<(u16, Vec<u8>), String> {
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| format!("read status: {e}"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line:?}"))?;
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("read header: {e}"))?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = v.trim().parse().ok();
        }
    }
    let mut body = Vec::new();
    match content_length {
        Some(len) => {
            reader
                .take(len as u64)
                .read_to_end(&mut body)
                .map_err(|e| format!("read body: {e}"))
        }
        None => reader
            .read_to_end(&mut body)
            .map_err(|e| format!("read body: {e}")),
    }?;
    Ok((status, body))
}

/// Broadcast this device's info every `interval` until the process ends.
/// A datagram that fails (no Wi-Fi, airplane mode) just skips a beat — the
/// loop is deliberately dumb so it can run forever.
pub fn spawn_announcer(info: DeviceInfo, interval: Duration) {
    std::thread::Builder::new()
        .name("sync-announce".into())
        .spawn(move || {
            let socket = match UdpSocket::bind(("0.0.0.0", 0)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("quire: sync discovery announce unavailable ({e})");
                    return;
                }
            };
            socket.set_broadcast(true).ok();
            let payload = info.to_json();
            loop {
                let _ = socket.send_to(
                    payload.as_bytes(),
                    ("255.255.255.255", DISCOVERY_PORT),
                );
                std::thread::sleep(interval);
            }
        })
        .ok();
}

/// Listen for announcements and hand every one that is not this device to
/// `on_seen` (device, ip). One thread for the life of the process.
pub fn spawn_listener(self_id: String, on_seen: impl Fn(DeviceInfo, String) + Send + 'static) {
    std::thread::Builder::new()
        .name("sync-listen".into())
        .spawn(move || {
            let socket = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("quire: sync discovery listen unavailable ({e})");
                    return;
                }
            };
            let mut buf = [0u8; 2048];
            loop {
                let (n, from) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(_) => {
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                let Ok(text) = std::str::from_utf8(&buf[..n]) else {
                    continue;
                };
                let Ok(device) = DeviceInfo::from_json(text) else {
                    continue;
                };
                if device.id == self_id {
                    continue;
                }
                on_seen(device, from.ip().to_string());
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The response reader against a hand-rolled HTTP/1.0 reply — the same
    /// bytes `services::lan_server`-style servers put on the wire. The
    /// server half reads the request first and drains the socket after
    /// answering, so the reply is delivered rather than RST away.
    #[test]
    fn reads_a_content_length_body() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
            );
            let _ = stream.flush();
            // keep the connection open until the client is done reading
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
        });
        let (status, body) = http_get("127.0.0.1", port, "/", Duration::from_secs(5)).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
    }
}
