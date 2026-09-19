// Rolling backups and startup recovery (SPEC §二十五). WAL + `synchronous=FULL`
// already guarantees that a committed batch survives a kill or a power loss;
// what it cannot survive is the main file itself going bad (a failing disk, a
// truncated write, an antimalware poke). So every successful open writes a
// compact snapshot of the database into a rotating family of `.bak<N>` files,
// and an open that fails with `Corrupt` walks that family from newest to
// oldest until one of them opens cleanly.
//
// The snapshots are made with `VACUUM INTO`, not a file copy: it reads through
// the live connection, so it sees committed-into-WAL pages too, and it writes
// the destination as one self-contained file (no `-wal` to carry along).
//
// Retention (D10) is two windows, whichever closes first: the newest `KEEP`
// generations, and `MAX_AGE`. Count alone keeps a five-year-old `.bak5` alive
// for a user who opens the app once a week; age alone keeps five copies of a
// workspace the size of scene D. Age is measured from the file's own modified
// time, which is when the snapshot was taken.
//
// Loss window: `.bak1` is the database as of the most recent snapshot, and a
// long session now takes one every `PersistenceService`'s period (default ten
// minutes, services/persistence.rs) instead of only at open — so a corruption
// that arrives mid-session costs the edits made since the last tick, not since
// startup. See ADR-0015 for the open-time design and ADR-0019 for this change.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::core::StorageError;

use super::database::Database;

/// How many snapshots to keep. Five answers "the last four opens were all
/// bad" while the age window below keeps the family from being five copies of
/// the workspace forever.
pub const KEEP: usize = 5;

/// A snapshot older than this is dropped even when the family is not full.
/// Seven days is one working week: the point is to be able to roll back past
/// "the bad edit I made this morning", not to be a time machine.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Generations scanned past `KEEP`, so slots left by an older, larger `KEEP`
/// cannot survive unbounded: `recover` only walks `1..=KEEP`, so anything
/// outside that window is unreadable weight.
const STALE_SLOTS: usize = 2;

/// What an open actually did, so the app can tell the user instead of the
/// fact dying in a log line (M8_FEEDBACK #4). Both fields describe this open:
/// a clean startup leaves `recovered_from` empty and `backup_failed` false.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// The snapshot that was moved into the main path, because the file that
    /// was there could not be read. Anything non-`None` means the edits made
    /// after the snapshot was taken are gone.
    pub recovered_from: Option<PathBuf>,
    /// The snapshot this open should have written but could not (read-only or
    /// full directory, a locked file). Startup continues — the database itself
    /// opened — but this session has no insurance behind it.
    pub backup_failed: bool,
}

impl OpenReport {
    /// Nothing to tell: the database opened as-is and the snapshot was written.
    pub fn is_clean(&self) -> bool {
        self.recovered_from.is_none() && !self.backup_failed
    }

    /// Print the two facts the user would want to know at startup, in the
    /// `eprintln!` convention the rest of startup already uses. A UI warning
    /// can be built from the same fields (M8_FEEDBACK #4).
    pub fn log(&self) {
        if let Some(from) = &self.recovered_from {
            eprintln!(
                "quire: the database was unreadable and was restored from {from:?} — \
                 edits made since then are lost"
            );
        }
        if self.backup_failed {
            eprintln!("quire: could not write the startup snapshot; this session is not backed up");
        }
    }
}

/// `<path>.bak<N>`, 1 = newest. The suffix is appended whole so the backup
/// keeps the main file's extension (`workspace.db.bak1`), which keeps file
/// pickers and `sqlite3` usable on them.
pub fn slot(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".bak{index}"));
    PathBuf::from(name)
}

/// WAL sidecars belonging to `path`, which must move or die with it.
fn sidecars(path: &Path) -> Vec<PathBuf> {
    ["-wal", "-shm"]
        .iter()
        .map(|suffix| {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            PathBuf::from(name)
        })
        .collect()
}

fn io(e: std::io::Error, what: &str) -> StorageError {
    StorageError::Open(format!("{what}: {e}"))
}

/// Shift the family by one and write the current database into `.bak1`, then
/// drop what the retention policy no longer wants (`KEEP` generations,
/// `MAX_AGE` old).
///
/// Takes the same lock every repository write takes, so the snapshot is one
/// consistent point in the change stream rather than a torn read.
pub fn snapshot(db: &Database, path: &Path) -> Result<(), StorageError> {
    rotate(path)?;
    let target = slot(path, 1);
    // VACUUM INTO refuses to clobber, and rotation should already have moved
    // the old generation away — remove it in case a previous run died here.
    let _ = std::fs::remove_file(&target);
    let conn = db.conn();
    // `ExecuteReturnedResults` means SQLite handed back the usual vacuum
    // status row; the file is written either way.
    //
    // The copy runs with durability switched off and restored right after.
    // A 2.3 MB workspace measured ≈23–64 ms here against ≈51–62 ms for the
    // same statement at `synchronous=FULL` (docs/PERFORMANCE.md: the gain is
    // consistent but noisy). It is safe because the snapshot is expendable —
    // a torn copy fails the integrity check when recovery tries it, and the
    // next open writes a better one — while `VACUUM INTO` only reads the main
    // database, so every real write still runs at `FULL`.
    conn.pragma_update(None, "synchronous", "OFF")
        .map_err(|e| StorageError::Sql(format!("backup synchronous=OFF: {e}")))?;
    let vacuum = conn.execute("VACUUM INTO ?1", rusqlite::params![target.display().to_string()]);
    if let Err(e) = conn.pragma_update(None, "synchronous", "FULL") {
        eprintln!("quire: could not restore synchronous=FULL ({e})");
    }
    match vacuum {
        Ok(_) | Err(rusqlite::Error::ExecuteReturnedResults) => {
            // Retention is housekeeping, not part of the insurance: a locked
            // or vanished old generation must not turn a good snapshot into a
            // reported failure, so the next snapshot tries the cleanup again.
            let _ = prune(path, SystemTime::now());
            Ok(())
        }
        Err(e) => Err(StorageError::Sql(format!("backup to {target:?}: {e}"))),
    }
}

/// Apply the retention policy and report what went away: generations past
/// `KEEP` (including any an older, larger `KEEP` left behind), and any inside
/// the window whose modified time is older than `MAX_AGE` at `now`.
///
/// `now` is the caller's clock for the same reason the debounce window takes
/// one: a test can age a file it just wrote instead of waiting a week.
pub fn prune(path: &Path, now: SystemTime) -> Result<Vec<PathBuf>, StorageError> {
    let mut removed = Vec::new();
    for index in 1..=KEEP + STALE_SLOTS {
        let file = slot(path, index);
        let Ok(meta) = file.metadata() else {
            continue; // no such generation (yet, or consumed by a restore)
        };
        let past_count = index > KEEP;
        // A file that will not say when it changed, or a clock that went
        // backwards, keeps it: dropping insurance is the worse mistake.
        let past_age = meta.modified().ok().is_some_and(|when| {
            now.duration_since(when)
                .is_ok_and(|age| age > MAX_AGE)
        });
        if past_count || past_age {
            std::fs::remove_file(&file)
                .map_err(|e| io(e, &format!("prune backup slot {index}")))?;
            removed.push(file);
        }
    }
    Ok(removed)
}

/// Move `.bak1` → `.bak2` → … and drop the oldest generation. A missing
/// generation (recovery consumed one) just leaves a gap that refills later.
fn rotate(path: &Path) -> Result<(), StorageError> {
    let oldest = slot(path, KEEP);
    if oldest.exists() {
        std::fs::remove_file(&oldest).map_err(|e| io(e, "remove oldest backup"))?;
    }
    for index in (1..KEEP).rev() {
        let from = slot(path, index);
        if from.exists() {
            std::fs::rename(&from, slot(path, index + 1))
                .map_err(|e| io(e, "rotate backup"))?;
        }
    }
    Ok(())
}

/// Open `path`, and if the main database is corrupt, recover it from the
/// newest snapshot that opens. On success the restored file is snapshotted
/// again, so the recovered state is protected immediately.
///
/// The returned `OpenReport` says whether that walk happened and whether the
/// snapshot could be written; `open_with_recovery` itself stays silent.
///
/// A backup is *moved* into the main path rather than copied into it: the
/// handle the recovery validated is then stale on the next open, and the
/// moved-away corpse keeps SQLite from replaying the bad file's WAL.
pub fn open_with_recovery(path: &Path) -> Result<(Database, OpenReport), StorageError> {
    let db = match Database::open(path) {
        Ok(db) => db,
        Err(error @ StorageError::Corrupt(_)) => return recover(path, error),
        Err(error) => return Err(error),
    };
    let mut report = OpenReport::default();
    // Backups are insurance, not a startup requirement: a read-only or
    // full directory must never stop the app from opening. The report is the
    // one place this fact is said — saying it here too would double the line.
    if snapshot(&db, path).is_err() {
        report.backup_failed = true;
    }
    Ok((db, report))
}

fn recover(path: &Path, error: StorageError) -> Result<(Database, OpenReport), StorageError> {
    for index in 1..=KEEP {
        let candidate = slot(path, index);
        if !candidate.exists() {
            continue;
        }
        // Validate by opening: migrations run (a no-op on a current snapshot),
        // and `PRAGMA integrity_check` has to say "ok".
        let valid = match Database::open(&candidate) {
            Ok(db) => {
                // The file is only movable once its handle is closed, which
                // is also what checkpoints its WAL into it.
                drop(db);
                true
            }
            Err(StorageError::Corrupt(_)) | Err(StorageError::Open(_)) => {
                eprintln!("quire: backup {candidate:?} is unusable");
                false
            }
            Err(_) => false,
        };
        if !valid {
            continue;
        }
        quarantine(path);
        std::fs::rename(&candidate, path).map_err(|e| io(e, "restore backup"))?;
        match Database::open(path) {
            Ok(db) => {
                let mut report = OpenReport {
                    recovered_from: Some(candidate.clone()),
                    backup_failed: false,
                };
                // The recovered state is what the next failure falls back to,
                // so protect it right away.
                if snapshot(&db, path).is_err() {
                    report.backup_failed = true;
                }
                return Ok((db, report));
            }
            Err(reopen) => {
                eprintln!("quire: restore of {candidate:?} failed ({reopen})");
                continue;
            }
        }
    }
    // Nothing usable: hand the original error back and leave the corrupt file
    // in place, so `sqlite3 .recover` or a later snapshot still has something
    // to work from.
    Err(error)
}

/// Move the unreadable main file aside as `<path>.corrupt` and drop its
/// sidecars, so the restored database is not haunted by the old WAL.
fn quarantine(path: &Path) {
    for sidecar in sidecars(path) {
        let _ = std::fs::remove_file(&sidecar);
    }
    if path.exists() {
        let corpse = {
            let mut name = path.as_os_str().to_os_string();
            name.push(".corrupt");
            PathBuf::from(name)
        };
        let _ = std::fs::remove_file(&corpse);
        let _ = std::fs::rename(path, &corpse);
    }
}
