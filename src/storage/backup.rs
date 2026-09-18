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
// Loss window: `.bak1` is the database as of the most recent successful open,
// so a corruption that arrives mid-session costs the edits made since startup.
// Widening that window (a periodic snapshot during long sessions) is a
// deliberate omission — see ADR-0015.

use std::path::{Path, PathBuf};

use crate::core::StorageError;

use super::database::Database;

/// How many snapshots to keep. Three answers "the last two opens were both
/// bad" without doubling the startup write cost.
pub const KEEP: usize = 3;

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

/// Shift the family by one and write the current database into `.bak1`.
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
        Ok(_) | Err(rusqlite::Error::ExecuteReturnedResults) => Ok(()),
        Err(e) => Err(StorageError::Sql(format!("backup to {target:?}: {e}"))),
    }
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
/// A backup is *moved* into the main path rather than copied into it: the
/// handle the recovery validated is then stale on the next open, and the
/// moved-away corpse keeps SQLite from replaying the bad file's WAL.
pub fn open_with_recovery(path: &Path) -> Result<Database, StorageError> {
    let db = match Database::open(path) {
        Ok(db) => db,
        Err(error @ StorageError::Corrupt(_)) => return recover(path, error),
        Err(error) => return Err(error),
    };
    // Backups are insurance, not a startup requirement: a read-only or
    // full directory must never stop the app from opening.
    if let Err(e) = snapshot(&db, path) {
        eprintln!("quire: backup skipped ({e})");
    }
    Ok(db)
}

fn recover(path: &Path, error: StorageError) -> Result<Database, StorageError> {
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
                eprintln!("quire: recovered {path:?} from {candidate:?}");
                let _ = snapshot(&db, path).map_err(|e| {
                    eprintln!("quire: backup after recovery skipped ({e})");
                    e
                });
                return Ok(db);
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
