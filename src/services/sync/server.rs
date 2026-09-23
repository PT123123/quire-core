// The inbound half of the sync protocol: a std::net HTTP server with five
// endpoints, mounted on a thread for the whole run while sync is enabled.
// Every endpoint that touches the workspace answers through the engine's
// job channel — the workspace itself is UI-thread property, so the socket
// threads never hold more than bytes.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Sender;

use super::engine::{DeviceInfo, Job, PeerRecord};

pub struct SyncServer {
    pub self_info: DeviceInfo,
    pub tx: Sender<Job>,
}

impl SyncServer {
    pub fn bind(port: u16) -> Result<TcpListener, String> {
        TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("sync bind: {e}"))
    }

    /// Serve forever on an already-bound listener (the caller spawns the
    /// thread; `services::lan_server` has the same shape).
    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let server = SyncServer {
                self_info: self.self_info.clone(),
                tx: self.tx.clone(),
            };
            std::thread::spawn(move || {
                let _ = server.handle(stream);
            });
        }
    }

    fn handle(&self, stream: TcpStream) -> std::io::Result<()> {
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line
                .strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
            {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        let body = String::from_utf8_lossy(&body).into_owned();

        let path = path.split('?').next().unwrap_or("").to_string();
        let reply = match (&method[..], &path[..]) {
            ("GET", "/sync/info") => {
                respond_text(&stream, 200, &self.self_info.to_json())
            }
            ("GET", "/sync/snapshot") => match ask(&self.tx, |reply| Job::ExportSnapshot { reply }) {
                Some(snap) => respond_text(&stream, 200, &snap.to_json()),
                None => respond_text(&stream, 503, "the app is shutting down"),
            },
            ("POST", "/sync/snapshot") => {
                let ip = stream
                    .peer_addr()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_default();
                let (status, text) = match SyncSnapshotIn::parse(&body) {
                    Ok(snap) => {
                        // the sender names itself in the snapshot; that is what
                        // the shadow for this merge is keyed by
                        let peer = PeerRecord {
                            id: snap.device_id.clone(),
                            name: snap.device.clone(),
                            kind: String::new(),
                            ip: ip.clone(),
                            port: stream
                                .peer_addr()
                                .map(|a| a.port())
                                .unwrap_or(crate::services::sync::SYNC_PORT),
                            paired: true,
                            last_seen: super::engine::now_unix(),
                            last_sync: String::new(),
                        };
                        match ask(&self.tx, |reply| Job::ApplyRemote {
                            peer,
                            snapshot: snap,
                            bytes: Vec::new(),
                            reply,
                        }) {
                            Some(Ok(merged)) => (200, merged.to_json()),
                            Some(Err(e)) => (409, e),
                            None => (503, "the app is shutting down".into()),
                        }
                    }
                    Err(e) => (400, e),
                };
                respond_text(&stream, status, &text)
            }
            ("GET", p) if p.starts_with("/sync/attachment/") => {
                let id = p
                    .strip_prefix("/sync/attachment/")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                match ask(&self.tx, |reply| Job::AttachmentBytes { id, reply }) {
                    Some(bytes) => respond_bytes(&stream, 200, &bytes),
                    None => respond_text(&stream, 404, "no such attachment"),
                }
            }
            ("POST", "/sync/pair") => {
                let (status, text) = match DeviceInfo::from_json(&body) {
                    Ok(device) => {
                        let ip = stream
                            .peer_addr()
                            .map(|a| a.ip().to_string())
                            .unwrap_or_default();
                        match ask(&self.tx, |reply| Job::InboundPair { device, ip, reply }) {
                            Some(true) => (200, "ok".into()),
                            _ => (503, "pairing refused".into()),
                        }
                    }
                    Err(e) => (400, e),
                };
                respond_text(&stream, status, &text)
            }
            ("GET", "/") | ("GET", "/sync") => respond_text(
                &stream,
                200,
                "Quire LAN sync\n\nGET  /sync/info             this device\nGET  /sync/snapshot       the workspace snapshot\nPOST /sync/snapshot       merge a snapshot and answer the merged one\nGET  /sync/attachment/<id> one attachment's bytes\nPOST /sync/pair           pair with the sending device\n",
            ),
            _ => respond_text(&stream, 404, "not found"),
        };
        reply
    }
}

/// The request body of the snapshot endpoints — the same struct the pull
/// reads, parsed where the response is written so the error text can travel.
struct SyncSnapshotIn;

impl SyncSnapshotIn {
    fn parse(body: &str) -> Result<super::model::SyncSnapshot, String> {
        super::model::SyncSnapshot::from_json(body)
    }
}

/// Ask the UI thread and block on the answer. `make` builds the job around a
/// fresh reply channel; `None` means the app went away mid-request.
fn ask<T>(tx: &Sender<Job>, make: impl FnOnce(mpsc::Sender<T>) -> Job) -> Option<T> {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    tx.send(make(reply_tx)).ok()?;
    reply_rx.recv().ok()
}

use std::sync::mpsc;

fn respond_text(stream: &TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    respond_bytes(stream, status, body.as_bytes())
}

fn respond_bytes(stream: &TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.0 {status} {reason}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut s = stream;
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()
}
