// LAN sync between Quire installs (desktop ↔ Android), modelled on
// aw-server-plus's aw-sync: UDP discovery, a trust-on-first-use pairing,
// full-snapshot exchange over HTTP, and a three-way merge whose loser is
// logged rather than dropped. The transport is the same dependency-free
// std::net HTTP the LAN share (`services::lan_server`) uses — no async
// runtime, no new heavy crates, one code path every shell compiles.
//
// This is the crate's half of the feature, and it is the whole protocol: the
// merge's rules, the wire format, the sockets and the worker thread. What it
// deliberately does **not** contain is the session glue — reading a snapshot
// out of a live workspace and walking a merged one back in touches a shell's
// `Rc`-bound session (`app::state`, `app::workspace`), which lives on that
// shell's side of ADR-0093's boundary and stays there. Both the desktop shell
// and the Android shell therefore depend on this module instead of carrying a
// copy each.
//
// Ownership map:
//   model     — the wire structs and their conversions to/from `core` rows
//   merge     — the pure three-way merge (unit-tested, no app types)
//   transport — the HTTP client and the UDP discovery halves
//   server    — the inbound HTTP endpoints
//   engine    — threads and the job/command channels back to the UI thread
//
// A shell's session is `Rc`-bound to its UI thread, so every job that reads
// or writes the workspace travels through `engine`'s channel to a timer on
// that thread; background threads only ever move bytes and JSON.

pub mod engine;
pub mod merge;
pub mod model;
pub mod server;
pub mod transport;

/// The TCP port the sync server listens on. The read-only LAN share keeps
/// 5877; sync sits next to it.
pub const SYNC_PORT: u16 = 5878;
/// The UDP port discovery announcements go out on.
pub const DISCOVERY_PORT: u16 = 5879;
