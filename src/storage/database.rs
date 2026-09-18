// Connection ownership: open + pragmas + startup integrity check
// (SPEC §二十五). The single `Connection` is mutex-wrapped so the
// `Repository` stays `Send + Sync` for the debounced flush (core/
// persistence.rs contract note).

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::ffi::ErrorCode;
use rusqlite::Connection;

use crate::core::StorageError;

use super::migrations;

/// An opened, migrated SQLite database ready for repository use.
pub struct Database {
    conn: Mutex<Connection>,
}

/// A file the recovery machinery already rejects at open/pragma time
/// ("not a database", "file is corrupt") is corrupt, not an IO problem —
/// the caller must see `Corrupt` so startup can branch on it (SPEC §二十五).
fn map_startup_error(e: rusqlite::Error, what: &str) -> StorageError {
    let corrupt = matches!(
        &e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(
                f.code,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
            )
    );
    if corrupt {
        StorageError::Corrupt(format!("{what}: {e}"))
    } else {
        StorageError::Open(format!("{what}: {e}"))
    }
}

/// Maps `OrderKey`'s u64 onto SQLite's i64 so signed `ORDER BY` reproduces
/// unsigned ordering: flipping the sign bit is monotonic over the domain.
pub(crate) fn ord_to_db(key: u64) -> i64 {
    (key as i64) ^ i64::MIN
}

pub(crate) fn ord_from_db(value: i64) -> u64 {
    (value ^ i64::MIN) as u64
}

impl Database {
    /// Open (creating if needed) `path`, apply WAL/journal pragmas, run
    /// pending migrations, then verify the file with `PRAGMA integrity_check`
    /// before handing it out (SPEC §二十五: startup integrity check).
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let mut conn =
            Connection::open(path).map_err(|e| map_startup_error(e, "open database"))?;
        Self::configure(&conn)?;
        migrations::ensure_current(&mut conn)?;
        let db = Database {
            conn: Mutex::new(conn),
        };
        db.check_integrity()?;
        Ok(db)
    }

    /// In-memory database for tests; identical pragmas minus the journal.
    pub fn open_in_memory() -> Result<Self, StorageError> {
        let mut conn = Connection::open_in_memory().map_err(|e| StorageError::Open(e.to_string()))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| StorageError::Open(e.to_string()))?;
        migrations::ensure_current(&mut conn)?;
        Ok(Database {
            conn: Mutex::new(conn),
        })
    }

    fn configure(conn: &Connection) -> Result<(), StorageError> {
        // WAL + synchronous=FULL: a committed batch survives kill -9 and
        // power loss; an uncommitted one can never be observed (SPEC §十八
        // atomic transaction, §二十五 journal strategy).
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| map_startup_error(e, "journal_mode=WAL"))?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|e| map_startup_error(e, "synchronous=FULL"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_startup_error(e, "foreign_keys=ON"))?;
        Ok(())
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `PRAGMA integrity_check`; anything but "ok" is reported as corrupt.
    pub fn check_integrity(&self) -> Result<(), StorageError> {
        let conn = self.conn();
        // A failing PRAGMA itself (e.g. "malformed database schema") is
        // corruption, not a plain SQL error.
        let report: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|e| StorageError::Corrupt(e.to_string()))?;
        if report == "ok" {
            Ok(())
        } else {
            Err(StorageError::Corrupt(report))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_key_mapping_preserves_unsigned_order() {
        let keys = [0u64, 1, 1 << 32, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX - 1, u64::MAX];
        for window in keys.windows(2) {
            assert!(ord_to_db(window[0]) < ord_to_db(window[1]));
        }
        for key in keys {
            assert_eq!(ord_from_db(ord_to_db(key)), key);
        }
    }

    #[test]
    fn integrity_check_rejects_a_mangled_file() {
        let dir = std::env::temp_dir().join(format!("quire-integrity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corp.db");
        {
            let db = Database::open(&path).unwrap();
            drop(db);
        }
        // Destroy the middle of the file while keeping the header intact,
        // so SQLite still recognizes it as a database worth checking.
        let len = std::fs::metadata(&path).unwrap().len();
        let mut bytes = std::fs::read(&path).unwrap();
        for b in &mut bytes[100..len as usize - 100] {
            *b = 0xAB;
        }
        std::fs::write(&path, &bytes).unwrap();
        let result = Database::open(&path);
        assert!(
            matches!(result, Err(StorageError::Corrupt(_))),
            "expected Corrupt, got {:?}",
            result.err().map(|e| e.to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
