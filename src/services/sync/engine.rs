// The engine: who is on the network, who is trusted, and the loop that keeps
// two workspaces converged.
//
// Threads (all plain std, all started by `start`):
//   the HTTP server   — the inbound endpoints (`super::server`)
//   the announcer     — UDP broadcast every few seconds (`super::transport`)
//   the listener      — peers heard on the wire → `Job::Discovered`
//   the worker        — runs one pull-merge-push cycle per `Cmd::SyncWith`
//
// Everything that touches the workspace (an Rc, UI-thread property) crosses
// the `Job` channel to `install`'s Slint timer instead — the same shape the
// flush service would use if it needed the document and did not have the
// repository instead.

use serde::{Deserialize, Serialize};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::model::SyncSnapshot;
use super::server::SyncServer;
use super::{transport, SYNC_PORT};

/// How this device announces itself. `id` is stable across runs (a settings
/// row), so a rename or a DHCP change does not orphan a pairing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// "windows" / "android" / "linux" / "macos" / "other"
    pub kind: String,
    pub port: u16,
}

impl DeviceInfo {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Result<DeviceInfo, String> {
        serde_json::from_str(s).map_err(|e| format!("bad device info: {e}"))
    }

    pub fn kind() -> String {
        if cfg!(target_os = "android") {
            "android".into()
        } else if cfg!(target_os = "windows") {
            "windows".into()
        } else if cfg!(target_os = "macos") {
            "macos".into()
        } else if cfg!(target_os = "linux") {
            "linux".into()
        } else {
            "other".into()
        }
    }

    /// A readable default: the machine's name, or a kind + short id.
    pub fn default_name(_id: &str) -> String {
        let host = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default();
        if !host.is_empty() && host != "localhost" {
            return host;
        }
        format!("Quire ({})", DeviceInfo::kind())
    }
}

/// One line of the peers table, persisted as a settings row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerRecord {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub ip: String,
    pub port: u16,
    pub paired: bool,
    /// Unix seconds of the last announcement seen (0 = never).
    pub last_seen: u64,
    /// RFC 3339 of the last successful sync ("" = never).
    pub last_sync: String,
}

/// One line of the sync log, newest last, capped by the writer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub at: String,
    pub peer: String,
    pub ok: bool,
    pub message: String,
}

/// A job for the UI thread. Every variant carries its own reply channel so
/// the background side can block until the workspace answered.
pub enum Job {
    /// The full snapshot of this device's workspace.
    ExportSnapshot {
        reply: Sender<SyncSnapshot>,
    },
    /// The ids of the attachments this device already stores.
    ListLocalAttachments {
        reply: Sender<Vec<u64>>,
    },
    /// Merge a peer's snapshot into the session (fetching the attachment
    /// bytes it brought along) and answer the merged snapshot.
    ApplyRemote {
        /// Who the snapshot came from — the shadow it merges against is keyed
        /// by this id, so it has to travel with the rows.
        peer: PeerRecord,
        snapshot: SyncSnapshot,
        bytes: Vec<(u64, Vec<u8>)>,
        reply: Sender<Result<SyncSnapshot, String>>,
    },
    /// The raw bytes of one of this device's attachments.
    AttachmentBytes {
        id: u64,
        reply: Sender<Vec<u8>>,
    },
    /// A device asked to pair; true accepts it.
    InboundPair {
        device: DeviceInfo,
        ip: String,
        reply: Sender<bool>,
    },
    /// A device was heard on the wire (or probed by hand).
    Discovered {
        device: DeviceInfo,
        ip: String,
    },
    /// A pairing just completed (this side asked, or answered a request):
    /// the peer becomes trusted and is written to the peers table.
    Paired {
        device: DeviceInfo,
        ip: String,
    },
    /// A sync attempt finished; the caller writes the peer row and the log.
    SyncDone {
        peer_id: String,
        ok: bool,
        message: String,
    },
    /// The periodic tick; the pump decides whether anyone is due.
    AutoTick,
}

/// A command from the UI thread to the worker.
pub enum Cmd {
    /// Run a full pull-merge-push against one peer.
    SyncWith(PeerRecord),
    /// Ask a discovered device to pair (a POST to its /sync/pair).
    PairWith(PeerRecord),
    /// A device typed in by hand: ask it who it is and pair on the answer.
    ProbeAdd { ip: String, port: u16 },
}

pub struct Engine {
    pub self_info: DeviceInfo,
    pub jobs: Sender<Job>,
    pub cmds: Sender<Cmd>,
}

/// Timestamp helpers shared by the pump.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_rfc3339() -> String {
    // Readable without a date crate: a civil-from-days conversion of the
    // unix second into UTC ("2026-09-23 14:05"). The peers table shows this
    // string, so the format is for a reader, not for a parser.
    let secs = now_unix() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {h:02}:{m:02}")
}

impl Engine {
    /// Start the four threads. Called once per run from the controller with
    /// the channel ends the pump will drain. Returns the command channel the
    /// pump uses to ask the worker for a sync cycle.
    pub fn start(self_info: DeviceInfo, jobs: Sender<Job>) -> Sender<Cmd> {
        // inbound HTTP
        let server = super::server::SyncServer {
            self_info: self_info.clone(),
            tx: jobs.clone(),
        };
        std::thread::Builder::new()
            .name("sync-server".into())
            .spawn(move || match SyncServer::bind(SYNC_PORT) {
                Ok(listener) => server.serve(listener),
                Err(e) => eprintln!("quire: sync server not started: {e}"),
            })
            .ok();

        // discovery: announce + listen
        transport::spawn_announcer(self_info.clone(), Duration::from_secs(4));
        let jobs_listen = jobs.clone();
        transport::spawn_listener(self_info.id.clone(), move |device, ip| {
            let _ = jobs_listen.send(Job::Discovered { device, ip });
        });

        // the worker: one sync cycle at a time, in request order
        let (cmd_tx, cmd_rx): (Sender<Cmd>, Receiver<Cmd>) = std::sync::mpsc::channel();
        let jobs_worker = jobs.clone();
        let me = self_info.clone();
        std::thread::Builder::new()
            .name("sync-worker".into())
            .spawn(move || {
                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        Cmd::SyncWith(peer) => sync_with(&peer, &jobs_worker),
                        Cmd::PairWith(peer) => pair_with(&me, &peer, &jobs_worker),
                        Cmd::ProbeAdd { ip, port } => probe_add(&ip, port, &jobs_worker),
                    }
                }
            })
            .ok();
        cmd_tx
    }
}

/// Ask a discovered peer to pair: one POST with this device's own info. Both
/// sides end up with the other in their peers table — that is the whole
/// handshake (trust on the first exchange; a code would only matter on a
/// network the user does not already control).
fn pair_with(me: &DeviceInfo, peer: &PeerRecord, jobs: &Sender<Job>) {
    match transport::http_post(
        &peer.ip,
        peer.port,
        "/sync/pair",
        &me.to_json(),
        Duration::from_secs(5),
    ) {
        Ok((200, _)) => {
            let _ = jobs.send(Job::Paired {
                device: DeviceInfo {
                    id: peer.id.clone(),
                    name: peer.name.clone(),
                    kind: peer.kind.clone(),
                    port: peer.port,
                },
                ip: peer.ip.clone(),
            });
        }
        Ok((status, body)) => {
            let _ = jobs.send(Job::SyncDone {
                peer_id: peer.id.clone(),
                ok: false,
                message: format!("{}: pairing answered {status}: {body}", peer.name),
            });
        }
        Err(e) => {
            let _ = jobs.send(Job::SyncDone {
                peer_id: peer.id.clone(),
                ok: false,
                message: format!("{}: {e}", peer.name),
            });
        }
    }
}

/// A device typed in by hand: ask who it is, then pair on the answer. This is
/// the door for a network where the UDP announcement cannot get through
/// (Android without a multicast lock, or a router that filters broadcasts).
fn probe_add(ip: &str, port: u16, jobs: &Sender<Job>) {
    let fail = |msg: String| {
        let _ = jobs.send(Job::SyncDone {
            peer_id: ip.to_string(),
            ok: false,
            message: msg,
        });
    };
    match transport::http_get(ip, port, "/sync/info", Duration::from_secs(4)) {
        Ok((200, body)) => {
            let text = String::from_utf8_lossy(&body).into_owned();
            match DeviceInfo::from_json(&text) {
                Ok(device) => {
                    let _ = jobs.send(Job::Paired {
                        device,
                        ip: ip.to_string(),
                    });
                }
                Err(e) => fail(format!("{ip}:{port}: {e}")),
            }
        }
        Ok((status, _)) => fail(format!("{ip}:{port}: info answered {status}")),
        Err(e) => fail(format!("{ip}:{port}: {e}")),
    }
}

/// One pull-merge-push cycle against a peer, run on the worker thread. The
/// workspace steps happen on the peer's own UI thread through `Job`s; this
/// thread only moves bytes.
fn sync_with(peer: &PeerRecord, jobs: &Sender<Job>) {
    let fail = |msg: String| {
        let _ = jobs.send(Job::SyncDone {
            peer_id: peer.id.clone(),
            ok: false,
            message: msg,
        });
    };

    // ① the peer is alive and is who we paired with
    let (status, info_body) =
        match transport::http_get(&peer.ip, peer.port, "/sync/info", Duration::from_secs(5)) {
            Ok(v) => v,
            Err(e) => return fail(format!("{}: {e}", peer.name)),
        };
    if status != 200 {
        return fail(format!("{}: info answered {status}", peer.name));
    }
    let info_body = String::from_utf8_lossy(&info_body).into_owned();
    let info = match DeviceInfo::from_json(&info_body) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if info.id != peer.id {
        return fail(format!(
            "{}: answered as {} — the address moved to another device",
            peer.name, info.name
        ));
    }

    // ② which attachments live here already
    let (tx, rx) = std::sync::mpsc::channel();
    if jobs.send(Job::ListLocalAttachments { reply: tx }).is_err() {
        return fail("the app is shutting down".into());
    }
    let local_ids = match rx.recv() {
        Ok(v) => v,
        Err(_) => return fail("the app is shutting down".into()),
    };

    // ③ the peer's snapshot
    let (status, snap_body) =
        match transport::http_get(&peer.ip, peer.port, "/sync/snapshot", Duration::from_secs(60)) {
            Ok(v) => v,
            Err(e) => return fail(format!("{}: {e}", peer.name)),
        };
    if status != 200 {
        return fail(format!(
            "{name}: snapshot answered {status}: {body}",
            name = peer.name,
            status = status,
            body = String::from_utf8_lossy(&snap_body)
        ));
    }
    let remote = match SyncSnapshot::from_json(&String::from_utf8_lossy(&snap_body)) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };

    // ④ the bytes of attachments we lack
    let mut bytes: Vec<(u64, Vec<u8>)> = Vec::new();
    for att in &remote.attachments {
        if local_ids.contains(&att.id) {
            continue;
        }
        match transport::http_get(
            &peer.ip,
            peer.port,
            &format!("/sync/attachment/{}", att.id),
            Duration::from_secs(120),
        ) {
            Ok((200, data)) => bytes.push((att.id, data)),
            Ok((status, _)) => {
                return fail(format!(
                    "{name}: attachment {} answered {status}",
                    att.id,
                    name = peer.name
                ))
            }
            Err(e) => return fail(format!("{}: {e}", peer.name)),
        }
    }

    // ⑤ merge + apply on the UI thread; the answer is our merged snapshot
    let (tx, rx) = std::sync::mpsc::channel();
    if jobs
        .send(Job::ApplyRemote {
            peer: peer.clone(),
            snapshot: remote,
            bytes,
            reply: tx,
        })
        .is_err()
    {
        return fail("the app is shutting down".into());
    }
    let merged = match rx.recv() {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return fail(format!("{}: {e}", peer.name)),
        Err(_) => return fail("the app is shutting down".into()),
    };

    // ⑥ push the merged snapshot; the peer merges it and answers its own
    //    final state (whose row count goes in the log — the loop closes on
    //    its next pull, since our merged already carries both sides' rows)
    let (status, answer) = match transport::http_post(
        &peer.ip,
        peer.port,
        "/sync/snapshot",
        &merged.to_json(),
        Duration::from_secs(120),
    ) {
        Ok(v) => v,
        Err(e) => return fail(format!("{}: {e}", peer.name)),
    };
    if status != 200 {
        return fail(format!(
            "{name}: push answered {status}: {answer}",
            name = peer.name
        ));
    }

    let _ = jobs.send(Job::SyncDone {
        peer_id: peer.id.clone(),
        ok: true,
        message: format!(
            "Synced with {name}: {pages} pages, {blocks} blocks, {dbs} databases in the merged workspace",
            name = peer.name,
            pages = merged.pages.len(),
            blocks = merged.blocks.len(),
            dbs = merged.databases.len()
        ),
    });
}
