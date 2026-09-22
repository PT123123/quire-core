// quire-core: everything Quire knows how to store, and nothing about how it
// looks. `core/` is the document and database model, `storage/` is SQLite
// behind it, `services/` is the work that runs without a window — import,
// export, search, attachments, settings, and the LAN framing that a sync
// module will grow out of.
//
// The rule this crate exists to enforce: nothing under `src/` here may name
// Slint, a file dialog, a clipboard or a platform API. The desktop shell and
// the Android shell are two consumers of one model, so a dependency in this
// direction ends the port rather than being noticed later.

pub mod core;
pub mod services;
pub mod storage;
// Scratch-directory guard for tests; see the module header for why it is not
// `#[cfg(test)]`.
pub mod testing;
