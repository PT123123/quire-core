// The rotating session log (SPEC §二十五, M8 D9). Nothing else in the app
// writes a log file: startup facts, the panic report and the session metadata
// all land in `quire.log` beside the database, so "what happened before it
// crashed" has one answer.
//
// Three files at ≤1 MB each bound a chatty session to ~3 MB and keep the
// current session (the interesting one) in the file that gets appended to:
// crossing the size limit shifts the family `.1 → .2 → dropped`, the way the
// database snapshots do (storage::backup).
//
// A panic is the one event worth surviving a restart for, so the hook writes
// `panic-report.txt` beside the log and the *next* `start()` promotes it to the
// `last_session_aborted` entry of `session.meta`. Recording it on the next
// launch rather than inside the panic is deliberate: a panicking process may
// die at any moment, so it only does the cheap write, while the read-modify-
// write of a metadata file runs on a thread known to be healthy.
//
// A panic is also the only unclean end that can write its own evidence. For
// the rest — a kill, an access violation, power loss — the signal is the
// absence of the clean-exit record: `main` notes `END_RECORD` after the final
// flush, and the next `start()` that finds neither a panic report nor that
// record as the newest one of the family (it may have rotated into `.1`)
// blames the kill the same way. A family with no record at all is a first
// run, which is a clean start by definition.
//
// `session.meta` is the file-backed twin of the database `metadata` table
// (`Change::MetaSet`): the logger runs before any database is open, so it
// cannot use that one. `meta_entries()` hands the entries to whoever opens the
// database next (docs/M8_FEEDBACK.md notes the wiring).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// The active log file, beside which `.1` and `.2` sit newest-first.
pub const LOG_NAME: &str = "quire.log";
/// Panic evidence the next startup turns into `last_session_aborted`.
pub const PANIC_NAME: &str = "panic-report.txt";
/// Key/value session metadata, the file-backed `metadata` table.
pub const META_NAME: &str = "session.meta";

/// Files in the family, the active one included.
pub const KEEP: usize = 3;
/// Rotate once the active file reaches this size (≈2 500 lines).
pub const MAX_BYTES: u64 = 1024 * 1024;

/// The one metadata key this module writes: a one-line summary of the unclean
/// end of the previous session — a panic, stored verbatim from its report, or
/// the missing clean-exit record (see `UNCLEAN_END_SUMMARY`).
pub const KEY_SESSION_ABORTED: &str = "last_session_aborted";

/// The record a clean exit writes last: `main` notes it after the final
/// flush, so a session that ended any other way — killed, crashed natively,
/// lost power — leaves the newest record something else (M8_FEEDBACK #10).
pub const END_RECORD: &str = "session ended";

/// What `last_session_aborted` says when the end-record is missing. Worded so
/// it cannot be mistaken for a panic, which is stored verbatim from its
/// report and starts "panicked at …".
const UNCLEAN_END_SUMMARY: &str =
    "did not shut down cleanly (killed, crashed natively, or lost power)";

/// Where the session's data lives, which is where the log goes: the log is
/// meant to be found beside the database, and since D12 `storage::data_location`
/// owns that answer — the per-user library, unless `--db` or `--portable` says
/// otherwise. Resolving it is what carries an old `appdata/` library across,
/// log included.
pub fn data_dir() -> PathBuf {
    crate::storage::data_location::data_dir()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Warn,
    Error,
    /// Written by the panic hook only.
    Panic,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::Panic => "panic",
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `2026-09-19T04:11:02.123Z` from Unix milliseconds. UTC, so a log line and a
/// file's modified time cannot disagree about which one is newer; the civil-date
/// split is done here rather than by adding a date crate to the tree.
fn format_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (
        secs.rem_euclid(86_400) / 3600,
        secs.rem_euclid(3600) / 60,
        secs.rem_euclid(60),
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// Days since 1970-01-01 → (year, month 1-12, day): Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `<dir>/quire.log`, `<dir>/quire.log.1`, …; index 0 is the active file.
pub fn slot(dir: &Path, index: usize) -> PathBuf {
    let mut name = LOG_NAME.to_string();
    if index > 0 {
        name.push_str(&format!(".{index}"));
    }
    dir.join(name)
}

fn size_of(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Drop the oldest generation and shift the rest down, so the active file
/// becomes `.1`. A gap (nothing was ever written in that generation) is not an
/// error, and `keep == 1` means there is no history to shift: truncate.
pub fn rotate(dir: &Path, keep: usize) -> io::Result<()> {
    let last = keep.saturating_sub(1);
    if last == 0 {
        return std::fs::remove_file(slot(dir, 0)).or_else(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        });
    }
    let _ = std::fs::remove_file(slot(dir, last));
    for index in (1..last).rev() {
        let from = slot(dir, index);
        if from.exists() {
            std::fs::rename(&from, slot(dir, index + 1))?;
        }
    }
    let active = slot(dir, 0);
    if active.exists() {
        std::fs::rename(&active, slot(dir, 1))?;
    }
    Ok(())
}

/// Newlines ride as the two-character `\n` sequence, so one record is one file
/// line and `grep` still finds it.
fn flatten(text: &str) -> String {
    text.trim_end().replace('\n', "\\n")
}

/// The message of a record line (`<utc> [level] message`): what follows the
/// level bracket. A line without one is not a record — a write torn by the
/// very kill this module exists to catch — and yields `None`.
fn record_message(line: &str) -> Option<&str> {
    line.split_once("] ").map(|(_, message)| message)
}

/// One logger bound to a directory. Files are opened per line: the volume is a
/// handful of startup lines plus one panic report, so a long-lived handle would
/// buy nothing and would have to be reopened after every rotation anyway.
pub struct Logger {
    dir: PathBuf,
    max_bytes: u64,
    keep: usize,
    /// Two writers rotating the same family can lose a generation (the rename
    /// target of one is the file the other just deleted), so the whole
    /// check-size-then-append sequence is serialized.
    writing: Mutex<()>,
}

impl Logger {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Logger::with_limits(dir, MAX_BYTES, KEEP)
    }

    /// A logger with a size limit and family depth of the caller's choice —
    /// tests use a tiny limit to reach rotation without writing a megabyte.
    pub fn with_limits(dir: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> Self {
        Logger {
            dir: dir.into(),
            max_bytes,
            keep,
            writing: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn panic_report_path(&self) -> PathBuf {
        self.dir.join(PANIC_NAME)
    }

    pub fn meta_path(&self) -> PathBuf {
        self.dir.join(META_NAME)
    }

    /// Append one line, rotating first if the active file reached the limit.
    /// Errors come back to the caller instead of being printed: a log we cannot
    /// write must never become a startup failure of its own.
    pub fn log(&self, level: Level, message: &str) -> io::Result<()> {
        let _guard = self.lock();
        self.append(level, message)
    }

    /// `log` without the lock, for a caller that already holds it: the mutex is
    /// not reentrant, so `report_panic` must not reach through `log`.
    fn append(&self, level: Level, message: &str) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let active = slot(&self.dir, 0);
        if size_of(&active) >= self.max_bytes {
            rotate(&self.dir, self.keep)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&active)?;
        writeln!(
            file,
            "{} [{}] {}",
            format_utc(now_ms()),
            level.as_str(),
            flatten(message)
        )?;
        Ok(())
    }

    /// The whole family, newest first, with a header per rotated file.
    pub fn contents(&self) -> io::Result<String> {
        let mut out = String::new();
        for index in 0..self.keep.max(1) {
            match std::fs::read_to_string(slot(&self.dir, index)) {
                Ok(text) => {
                    if index > 0 {
                        let path = slot(&self.dir, index);
                        out.push_str(&format!("--- {} ---\n", path.display()));
                    }
                    out.push_str(&text);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// The newest record of the family, as its raw line. Generations shift
    /// whole (active → `.1` → `.2`), so the lowest-index file that holds any
    /// line holds the newest records, and its last line is the newest one —
    /// an end-record that rotated into `.1` is still found. `None` when no
    /// file in the family holds a record, which is a first-ever run.
    fn newest_record(&self) -> Option<String> {
        for index in 0..self.keep.max(1) {
            if let Ok(text) = std::fs::read_to_string(slot(&self.dir, index)) {
                if let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) {
                    return Some(line.to_string());
                }
            }
        }
        None
    }

    /// The session-start ritual: consume the previous run's panic report and
    /// say so in the log. Returns the summary it recorded, so a caller can see
    /// that an abort happened without reading files.
    pub fn start(&self) -> Option<String> {
        let mut aborted = match std::fs::read_to_string(self.panic_report_path()) {
            Ok(text) => {
                let _ = std::fs::remove_file(self.panic_report_path());
                // Stored raw: `set_meta` is the one place that escapes, so a
                // value that arrives pre-escaped would come back flattened.
                Some(text.trim_end().to_string())
            }
            Err(_) => None,
        };
        if aborted.is_none() {
            // No panic report: the end-record is the only other signal a
            // clean exit leaves (main notes it after the final flush), so the
            // newest record of the family says how the previous session ended.
            // The check runs before the "session started" line below is
            // written, so what gets read here is still the previous session's
            // tail — including one that rotated into `.1`, which
            // `newest_record` follows. No record at all is a first run, not a
            // kill; a report takes precedence and skips this entirely, so a
            // panic is never reported twice.
            let ended = match self.newest_record() {
                Some(line) => record_message(&line) == Some(END_RECORD),
                None => true,
            };
            if !ended {
                aborted = Some(UNCLEAN_END_SUMMARY.to_string());
            }
        }
        if let Some(summary) = &aborted {
            let _ = self.set_meta(KEY_SESSION_ABORTED, summary);
            let _ = self.log(Level::Warn, &format!("the previous session aborted: {summary}"));
        }
        let _ = self.log(
            Level::Info,
            &format!("session started (pid {})", std::process::id()),
        );
        aborted
    }

    /// Write the panic evidence: two writes, no read-modify-write, because
    /// this runs while the process is falling over.
    pub fn report_panic(&self, message: &str, location: &str) {
        let _guard = self.lock();
        let _ = self.append(
            Level::Panic,
            &format!("panicked at {location}: {message}"),
        );
        let body = format!(
            "panicked at {location}: {message}\nrecorded at {}\n",
            format_utc(now_ms())
        );
        let _ = std::fs::write(self.panic_report_path(), body);
    }

    // ---- session metadata ------------------------------------------------

    pub fn meta(&self, key: &str) -> Option<String> {
        self.read_meta().get(key).cloned()
    }

    /// Everything in `session.meta`, for the caller that holds the database and
    /// can write it as `Change::MetaSet`.
    pub fn meta_entries(&self) -> Vec<(String, String)> {
        self.read_meta().into_iter().collect()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> io::Result<()> {
        let _guard = self.lock();
        std::fs::create_dir_all(&self.dir)?;
        let mut map = self.read_meta();
        map.insert(key.into(), value.into());
        let mut body = String::new();
        for (k, v) in &map {
            body.push_str(&format!("{k}={}\n", v.replace('\n', "\\n")));
        }
        // Temp file then rename, so a process that dies mid-write leaves the
        // previous metadata readable instead of a truncated file.
        let temp = self.dir.join(format!("{META_NAME}.tmp"));
        std::fs::write(&temp, body)?;
        match std::fs::rename(&temp, self.meta_path()) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                Err(e)
            }
        }
    }

    fn read_meta(&self) -> BTreeMap<String, String> {
        let text = std::fs::read_to_string(self.meta_path()).unwrap_or_default();
        let mut map = BTreeMap::new();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                map.insert(key.to_string(), value.replace("\\n", "\n"));
            }
        }
        map
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.writing.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---- process-wide logger -------------------------------------------------

static CURRENT: Mutex<Option<Arc<Logger>>> = Mutex::new(None);
/// The panic hook is process-global, so choosing which logger it writes to is
/// a serialized act. `install` is the only path that takes this lock.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Install the process logger and the panic hook, and write the startup record.
/// Called from `main` as one line. A directory we cannot write to is printed and
/// otherwise ignored: logging is not a reason to refuse to start.
pub fn init() {
    install(Logger::new(data_dir()));
}

/// `init` for a chosen directory, returning the logger.
pub fn init_at(dir: &Path) -> Arc<Logger> {
    install(Logger::new(dir))
}

fn install(logger: Logger) -> Arc<Logger> {
    let logger = Arc::new(logger);
    if let Err(e) = std::fs::create_dir_all(logger.dir()) {
        eprintln!("quire: cannot create the log directory {:?} ({e})", logger.dir());
    }
    logger.start();
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    CURRENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(logger.clone());
    install_panic_hook(logger.clone());
    logger
}

/// The logger `note`/`warn`/`error` write to, if startup installed one.
pub fn current() -> Option<Arc<Logger>> {
    CURRENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Log through the current logger; a no-op before `init`, so code that may run
/// either way can call it without checking.
pub fn log(level: Level, message: &str) {
    if let Some(logger) = current() {
        let _ = logger.log(level, message);
    }
}

pub fn note(message: &str) {
    log(Level::Info, message);
}

/// Log and echo to stderr, which is how startup facts reached the console
/// before there was a log file.
pub fn warn(message: &str) {
    log(Level::Warn, message);
    eprintln!("quire: {message}");
}

pub fn error(message: &str) {
    log(Level::Error, message);
    eprintln!("quire: {message}");
}

/// Route the panic payload and location into the log and the abort report. The
/// hook that was installed before us still runs, so stderr keeps the usual
/// panic message.
fn install_panic_hook(logger: Arc<Logger>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| info.payload().downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown panic payload".into());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        logger.report_panic(&message, &location);
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    /// A fresh directory under `%TEMP%`, unique per call.
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quire-log-{}-{}-{label}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scratch_logger(max_bytes: u64, keep: usize) -> Arc<Logger> {
        Arc::new(Logger::with_limits(
            scratch(&format!("max{max_bytes}-keep{keep}")),
            max_bytes,
            keep,
        ))
    }

    #[test]
    fn a_line_lands_in_the_active_file() {
        let logger = scratch_logger(1024, KEEP);
        logger.log(Level::Info, "hello").unwrap();
        let text = std::fs::read_to_string(slot(logger.dir(), 0)).unwrap();
        assert!(text.contains("[info] hello"), "got {text}");
        // a readable UTC stamp leads the line, not a bare epoch counter
        assert!(
            text.starts_with("20") && text.contains("Z [info] "),
            "got {text}"
        );
    }

    #[test]
    fn rotation_keeps_three_files_and_drops_the_oldest() {
        let logger = scratch_logger(200, 3);
        // ≈40 bytes a line, so 30 lines is several times past 3 × 200 bytes.
        for n in 0..30 {
            logger.log(Level::Info, &format!("line {n}")).unwrap();
        }
        for index in 0..3 {
            assert!(slot(logger.dir(), index).exists(), "generation {index}");
        }
        assert!(
            !slot(logger.dir(), 3).exists(),
            "the family stays bounded at 3 files"
        );
        for index in 0..3 {
            let path = slot(logger.dir(), index);
            let size = size_of(&path);
            // one line may overshoot the limit, because size is checked before
            // the write rather than mid-line
            assert!(size <= 200 + 64, "{path:?} holds {size} bytes");
        }
        let all = logger.contents().unwrap();
        assert!(all.contains("line 29"), "the newest line died: {all}");
        assert!(!all.contains("line 0"), "the oldest line survived: {all}");
    }

    #[test]
    fn rotated_generations_are_older_than_the_active_one() {
        let logger = scratch_logger(120, 3);
        for n in 0..12 {
            logger.log(Level::Info, &format!("m{n}")).unwrap();
        }
        let active = std::fs::read_to_string(slot(logger.dir(), 0)).unwrap();
        let oldest = std::fs::read_to_string(slot(logger.dir(), 2)).unwrap();
        assert!(active.contains("m11"), "active file: {active}");
        assert!(!oldest.contains("m11"), "shift went the wrong way: {oldest}");
        assert!(
            oldest.lines().count() < active.lines().count() + 10,
            "a rotated generation keeps its own lines only"
        );
    }

    #[test]
    fn a_panic_becomes_aborted_metadata_on_the_next_start() {
        let logger = scratch_logger(MAX_BYTES, KEEP);
        logger.start();
        logger.report_panic("called `unwrap()` on a `None` value", "src/app/x.rs:7:1");

        let panicked = std::fs::read_to_string(logger.panic_report_path()).unwrap();
        assert!(panicked.contains("src/app/x.rs:7:1"), "got {panicked}");
        assert!(logger
            .contents()
            .unwrap()
            .contains("[panic] panicked at src/app/x.rs:7:1"));

        // the crashed session is over; a new one starts in the same directory
        let next = Logger::new(logger.dir());
        let aborted = next.start().expect("the report should be consumed once");
        assert!(aborted.contains("None"), "got {aborted}");
        assert_eq!(next.meta(KEY_SESSION_ABORTED).as_deref(), Some(aborted.as_str()));
        assert!(
            !logger.panic_report_path().exists(),
            "the report is consumed, so the next restart does not blame it twice"
        );
        assert!(next
            .contents()
            .unwrap()
            .contains("[warn] the previous session aborted"));
        assert!(next
            .meta_entries()
            .iter()
            .any(|(k, _)| k == KEY_SESSION_ABORTED));
    }

    #[test]
    fn a_clean_start_reports_no_abort() {
        let logger = Logger::new(scratch("clean"));
        assert!(logger.start().is_none());
        assert!(logger.meta(KEY_SESSION_ABORTED).is_none());
        assert!(logger.contents().unwrap().contains("[info] session started"));
    }

    #[test]
    fn metadata_survives_a_reopen_and_escapes_newlines() {
        let logger = Logger::new(scratch("meta"));
        logger
            .set_meta(KEY_SESSION_ABORTED, "first line\nsecond line")
            .unwrap();
        let reopened = Logger::new(logger.dir());
        assert_eq!(
            reopened.meta(KEY_SESSION_ABORTED).as_deref(),
            Some("first line\nsecond line")
        );
        // one file line per record, so the file stays parseable
        let raw = std::fs::read_to_string(reopened.meta_path()).unwrap();
        assert_eq!(raw.lines().count(), 1, "got {raw}");
        assert!(!slot(logger.dir(), 0).exists(), "metadata is not a log line");
    }

    #[test]
    fn panic_hook_reports_through_the_installed_logger() {
        let dir = scratch("hook");
        // `install_at` is the only thing that needs the process lock; taking it
        // here as well would self-deadlock (the mutex is not reentrant).
        let logger = init_at(&dir);
        assert!(std::panic::catch_unwind(|| panic!("boom from the hook test")).is_err());
        let report = std::fs::read_to_string(logger.panic_report_path()).unwrap();
        assert!(
            report.contains("boom from the hook test"),
            "the hook must record the payload: {report}"
        );
        assert!(report.contains("logging.rs"), "and the location: {report}");
        assert!(CURRENT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|current| current.dir() == dir));
    }

    #[test]
    fn utc_timestamps_match_the_epoch() {
        // a leap day, the usual place for this bug
        assert_eq!(format_utc(951_827_696_789), "2000-02-29T12:34:56.789Z");
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn a_missing_directory_is_created_and_a_depth_of_one_truncates() {
        let dir = scratch("nested").join("not-created");
        rotate(&dir, KEEP).unwrap(); // nothing to shift is not an error
        let logger = Logger::new(&dir);
        for n in 0..6 {
            logger.log(Level::Info, &format!("x{n}")).unwrap();
        }
        let single = Logger::with_limits(&dir, 60, 1);
        for n in 0..6 {
            single.log(Level::Info, &format!("y{n}")).unwrap();
        }
        assert!(slot(&dir, 0).exists());
        assert!(
            !slot(&dir, 1).exists(),
            "a family of one has no history: the active file is reused"
        );
        let text = std::fs::read_to_string(slot(&dir, 0)).unwrap();
        assert!(!text.contains("x0"), "old content survived: {text}");
    }

    #[test]
    fn an_end_record_makes_the_next_start_clean() {
        let logger = scratch_logger(MAX_BYTES, KEEP);
        logger.start();
        logger.log(Level::Info, END_RECORD).unwrap();

        let next = Logger::new(logger.dir());
        assert!(
            next.start().is_none(),
            "a session whose last record is the end-record ended cleanly"
        );
        assert!(next.meta(KEY_SESSION_ABORTED).is_none());
    }

    #[test]
    fn a_session_killed_without_the_end_record_is_reported() {
        let logger = scratch_logger(MAX_BYTES, KEEP);
        logger.start();
        logger.log(Level::Info, "mid-session work").unwrap();
        // …and here the process dies: no panic report, no end-record

        let next = Logger::new(logger.dir());
        let aborted = next.start().expect("the kill must be reported");
        assert_eq!(aborted, UNCLEAN_END_SUMMARY, "worded apart from a panic");
        assert_eq!(
            next.meta(KEY_SESSION_ABORTED).as_deref(),
            Some(UNCLEAN_END_SUMMARY)
        );
        assert!(next.contents().unwrap().contains(
            "[warn] the previous session aborted: did not shut down cleanly"
        ));

        // a session that got no further than its start line is a kill too:
        // its own start line must not read as a clean end
        let only_started = scratch_logger(MAX_BYTES, KEEP);
        only_started.start();
        let next = Logger::new(only_started.dir());
        assert!(
            next.start().is_some(),
            "the start line of the killed session is not an end-record"
        );
    }

    #[test]
    fn a_panic_report_beats_the_missing_end_record() {
        let logger = scratch_logger(MAX_BYTES, KEEP);
        logger.start();
        // the panicking session writes no end-record either: the report wins,
        // and the end-record path must not add a second summary
        logger.report_panic("called `unwrap()` on a `None` value", "src/app/x.rs:7:1");

        let next = Logger::new(logger.dir());
        let aborted = next.start().expect("the panic is the abort");
        assert!(aborted.contains("None"), "got {aborted}");
        assert!(
            !aborted.contains("did not shut down cleanly"),
            "the end-record must not double-report: {aborted}"
        );
        let all = next.contents().unwrap();
        assert_eq!(
            all.matches("the previous session aborted").count(),
            1,
            "exactly one abort line: {all}"
        );
    }

    #[test]
    fn an_end_record_that_rotated_still_reads_as_clean() {
        let logger = scratch_logger(MAX_BYTES, KEEP);
        logger.start();
        logger.log(Level::Info, END_RECORD).unwrap();
        // the next launch rotated the family and died before its first append:
        // the end-record is now the newest record of the family, and it sits
        // in `.1` under a missing active file
        rotate(logger.dir(), KEEP).unwrap();
        assert!(!slot(logger.dir(), 0).exists(), "setup: the family shifted");

        let next = Logger::new(logger.dir());
        assert!(next.start().is_none(), "the end-record survives the shift");
        assert!(next.meta(KEY_SESSION_ABORTED).is_none());

        // the same shift with no end-record is still a kill: the dead
        // session's records sit in `.1` and say so
        let killed = scratch_logger(MAX_BYTES, KEEP);
        killed.start();
        killed.log(Level::Info, "mid-session work").unwrap();
        rotate(killed.dir(), KEEP).unwrap();
        let next = Logger::new(killed.dir());
        assert!(
            next.start().is_some(),
            "a rotated tail without the end-record aborts"
        );
    }

    #[test]
    fn an_empty_log_is_a_first_run_not_an_abort() {
        let dir = scratch("empty-log");
        std::fs::write(slot(&dir, 0), "").unwrap();
        let logger = Logger::new(&dir);
        assert!(logger.start().is_none());
        assert!(logger.meta(KEY_SESSION_ABORTED).is_none());
    }
}
