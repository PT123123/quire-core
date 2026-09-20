// Test support, compiled into the lib so `tests/integration/**` can reach it
// too — a `#[cfg(test)]` helper here would be invisible to them, because the
// lib is built without that flag for an integration test crate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory under `%TEMP%` that deletes itself.
///
/// Uniqueness carries three ingredients on purpose. Windows recycles process
/// ids, so `pid` alone can name a directory a finished run left behind — and a
/// "fresh fixture" test then reads that stale content instead of its own. The
/// counter separates calls inside one process; the clock separates processes.
///
/// The self-deletion is what makes that ordering unnecessary. Every fixture
/// helper used to create a directory and walk away, and `%TEMP%` accumulated
/// thousands of them.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "quire-{label}-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            nanos(),
        ));
        // Insurance, not the mechanism: `create_dir_all` succeeds on a
        // directory that already exists, so a name that somehow collided
        // would hand the test someone else's content.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("could not create a scratch directory");
        ScratchDir { path: dir }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for ScratchDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // A file still held open by the OS makes this fail on Windows. Nothing
        // a test can do about it, and a leftover is not a test failure, so the
        // error is dropped rather than turning cleanup into an assertion.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the type: the directory is there for the test and gone
    /// after it. Asserted from outside the `Drop` impl, since a guard checking
    /// its own work would pass even when the removal failed.
    #[test]
    fn a_scratch_directory_exists_while_bound_and_not_afterwards() {
        let path = {
            let scratch = ScratchDir::new("guard");
            assert!(scratch.exists(), "{} vanished", scratch.display());
            assert!(scratch.join("child").parent().is_some());
            scratch.path().to_path_buf()
        };
        assert!(!path.exists(), "{path:?} survived its guard");
    }

    #[test]
    fn two_scratch_directories_never_share_a_name() {
        let a = ScratchDir::new("same-label");
        let b = ScratchDir::new("same-label");
        assert_ne!(a.path(), b.path());
    }
}
