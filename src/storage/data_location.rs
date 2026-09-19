// Where the library lives, and the one-time move that gets it there
// (SPEC §二十五, M8 D12).
//
// Until now the database sat at `appdata/quire.db` *relative to the working
// directory*, which is right for a development checkout and wrong for an
// installed app: launching Quire from two folders (a shortcut with a different
// "start in", a project directory, a USB stick) opens — and seeds — two
// unrelated libraries, and the per-user installer puts the exe in a folder the
// user may not own. So the default is now `%APPDATA%\Quire\quire.db`, and the
// first start that resolves it carries the old library across whole: the main
// file, its snapshots, its sidecars.
//
// Two escape hatches stay, both parsed into [`LaunchOptions`] by `main.rs` and
// handed to this module as data:
//   --db <path>   use exactly this file (no migration, no per-user directory) —
//                 what the benchmark harness uses to keep out of real notes;
//   --portable    the pre-D12 behavior — a library beside the working
//                 directory. That is what a stick install wants.
// Together: `--db` wins. The override names the file and everything beside it
// (log included), so it suppresses the migration; `--portable` then only
// matters when a run wants the default-shaped library kept in place.
//
// The module reads no process state: the seam
// [`effective_path(requested, options)`] is called with what the command line
// said, so a caller that named a file for itself is taken at its word and no
// test or tool can be rerouted by an environment variable. `main.rs` resolves
// once through [`migration`] — which is where it learns to report the move —
// and hands the finished path to `SqliteRepository::open_at` (M8_FEEDBACK #13).

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::{fs, io};

use super::backup;
use super::Database;

/// Folder inside `%APPDATA%`, matching the installer's own naming
/// (`install/quire.iss` installs to `{localappdata}\Programs\Quire`).
pub const APP_DIR_NAME: &str = "Quire";
/// The library's file name, in every placement.
pub const DB_NAME: &str = "quire.db";
/// Pre-D12: `appdata/` beside the working directory.
pub const LEGACY_DIR: &str = "appdata";
/// Snapshot generations probed during a move: `backup::KEEP` plus the stale
/// slots it also sweeps, rounded up so a family left behind by an older,
/// larger setting travels instead of being orphaned in the old folder.
const SNAPSHOT_SLOTS: usize = 8;
/// The log family, whose owner is `services::logging` (its own `KEEP`). Spelled
/// out rather than imported: storage must not depend on services.
const LOG_FILES: [&str; 3] = ["quire.log", "quire.log.1", "quire.log.2"];

/// The launch flags that steer placement, as data rather than as a command
/// line. `main.rs`'s `parse_launch_args` fills both fields; nothing in this
/// module looks at `std::env::args()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    /// `--db <path>`: the file to open, whatever the placement rules say.
    pub db_override: Option<PathBuf>,
    /// `--portable`: keep the library beside the working directory.
    pub portable: bool,
}

impl LaunchOptions {
    /// Read the two flags off an argument list. The binary's own parser is the
    /// production caller; this exists so the tests (and anything holding a raw
    /// argv, like a future CLI wrapper) can state a case as flags.
    pub fn from_args(argv: &[String]) -> Self {
        let (db_override, portable) = scan(argv);
        LaunchOptions { db_override, portable }
    }
}

/// What the command line and the environment say about where the library goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Somebody named the file — an explicit `--db`, or a caller that passed a
    /// path of its own instead of the default. Nothing is migrated.
    Fixed(PathBuf),
    /// `--portable`, or no per-user directory available: the pre-D12 default.
    Portable(PathBuf),
    /// The per-user library, with the folder the previous version used named
    /// so the move can be attempted (and reported) by whoever opens it.
    Roaming { path: PathBuf, legacy: PathBuf },
}

impl Placement {
    /// The file to open.
    pub fn path(&self) -> &Path {
        match self {
            Placement::Fixed(p) | Placement::Portable(p) => p,
            Placement::Roaming { path, .. } => path,
        }
    }

    /// The folder the previous version used, when one has to be carried over.
    pub fn legacy(&self) -> Option<&Path> {
        match self {
            Placement::Roaming { legacy, .. } => Some(legacy),
            _ => None,
        }
    }
}

/// The pre-D12 default: `appdata/quire.db`, relative to the working directory.
pub fn legacy_db() -> PathBuf {
    PathBuf::from(LEGACY_DIR).join(DB_NAME)
}

/// `%APPDATA%\Quire`, or `None` when the environment has no roaming directory
/// (a bare service account, a container, a stripped test runner).
pub fn roaming_root() -> Option<PathBuf> {
    app_data(Path::new(&std::env::var_os("APPDATA")?))
}

/// The same, with the environment variable's value handed in — the testable
/// half. An empty value counts as absent.
pub fn app_data(appdata: &Path) -> Option<PathBuf> {
    if appdata.as_os_str().is_empty() {
        return None;
    }
    Some(appdata.join(APP_DIR_NAME))
}

/// Is `path` the default the app asks for when no `--db` was given? A leading
/// `./` names the same file.
fn is_default(path: &Path) -> bool {
    let trimmed = match path.components().next() {
        Some(Component::CurDir) => path.strip_prefix(".").unwrap_or(path),
        _ => path,
    };
    trimmed == legacy_db()
}

/// The value of `--db <path>` / `--db=<path>`, and whether `--portable`
/// appears anywhere in the argument list — the parsing behind
/// [`LaunchOptions::from_args`].
fn scan(args: &[String]) -> (Option<PathBuf>, bool) {
    let mut db = None;
    let mut portable = false;
    let mut index = 1; // argv[0] is the exe, which may itself contain "--db"
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--portable" {
            portable = true;
        } else if arg == "--db" {
            if let Some(value) = args.get(index + 1) {
                db = Some(PathBuf::from(value));
                index += 1;
            }
        } else if let Some(value) = arg.strip_prefix("--db=") {
            db = Some(PathBuf::from(value));
        }
        index += 1;
    }
    (db, portable)
}

/// Decide, from the launch flags and the per-user library directory (what
/// [`roaming_root`] returns; `None` when the environment has none), where the
/// library is. Pure, so the whole policy is testable without touching process
/// state.
///
/// An explicit `--db` beats everything, including `--portable` and a caller
/// that was handed a path of its own instead of the default — the override is
/// the file and the folder beside it, no migration involved.
pub fn decide(options: &LaunchOptions, requested: &Path, per_user: Option<&Path>) -> Placement {
    if let Some(path) = &options.db_override {
        return Placement::Fixed(path.clone());
    }
    if !is_default(requested) {
        return Placement::Fixed(requested.to_path_buf());
    }
    if options.portable {
        return Placement::Portable(requested.to_path_buf());
    }
    match per_user {
        Some(dir) => Placement::Roaming {
            path: dir.join(DB_NAME),
            legacy: requested.to_path_buf(),
        },
        // Nowhere to move to: stay where the app already is.
        None => Placement::Portable(requested.to_path_buf()),
    }
}

/// Where the library is after the placement rules ran: the file to open, plus
/// the folder it was carried from when a move happened (the old path only stays
/// in the report while it still names files that travelled — an
/// already-migrated or empty legacy folder reports `None`).
pub struct Migration {
    pub path: PathBuf,
    pub from: Option<PathBuf>,
}

/// [`decide`], plus the one-time move and its report.
pub fn migration(options: &LaunchOptions, requested: &Path, per_user: Option<&Path>) -> Migration {
    migrate_placement(&decide(options, requested, per_user))
}

/// The move half of [`migration`], split out so a test can drive it with a
/// fixture-shaped [`Placement`] no command line would produce.
///
/// A move that fails does not redirect the caller to an empty per-user
/// library; that would read as "my notes vanished". The old path comes back,
/// with the reason on stderr, so the session opens the data it can reach.
fn migrate_placement(placement: &Placement) -> Migration {
    let Some(legacy) = placement.legacy().map(Path::to_path_buf) else {
        return Migration { path: placement.path().to_path_buf(), from: None };
    };
    let target = placement.path().to_path_buf();
    if let Some(parent) = target.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match migrate_legacy(&legacy, &target) {
        Ok(moved) => {
            if !moved.is_empty() {
                eprintln!(
                    "quire: moved the library from {} to {} ({} files); it now lives in your \
                     user profile",
                    legacy.display(),
                    target.display(),
                    moved.len()
                );
            }
            Migration { path: target, from: (!moved.is_empty()).then_some(legacy) }
        }
        Err(e) => {
            eprintln!(
                "quire: could not move the library to {} ({e}); continuing with {}",
                target.display(),
                legacy.display()
            );
            Migration { path: legacy, from: None }
        }
    }
}

/// The path to really open for `requested`: the caller's, unless it is the
/// default — in which case the placement rules apply and the legacy library is
/// carried over first.
pub fn effective_path(options: &LaunchOptions, requested: &Path) -> PathBuf {
    migration(options, requested, roaming_root().as_deref()).path
}

/// The directory the session's files live in — the database and the log beside
/// it (`services::logging`, D9, keeps `quire.log` next to the data). Resolving
/// it performs the move, because the log is part of the library.
pub fn data_dir(options: &LaunchOptions) -> PathBuf {
    match effective_path(options, &legacy_db()).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Carry the whole pre-D12 library into `target_db`'s directory: the main
/// file, then its snapshot family, then whatever sidecars a crashed session
/// left behind, then the log family that shares the folder. Returns the
/// destination files it wrote, in order.
///
/// Idempotent by the destination: once `target_db` exists there is nothing to
/// migrate, so a restart — or a move interrupted after the first file — never
/// overwrites a library that has since been edited. Each file lands through a
/// temporary name and one rename, so no half-written database is ever visible
/// under its final name; the source is deleted only afterwards, and if it
/// cannot be deleted it is renamed `.migrated-away`, which keeps the old folder
/// from opening it again by accident.
pub fn migrate_legacy(legacy_db_path: &Path, target_db: &Path) -> io::Result<Vec<PathBuf>> {
    if target_db.exists() || !legacy_db_path.exists() {
        return Ok(Vec::new()); // already migrated, or nothing to migrate
    }
    let target_dir = target_db
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(target_dir)?;

    // A live WAL means committed pages that are not in the main file yet.
    // Opening and closing the old database checkpoints them into it — a no-op
    // when there is no WAL, and skipped when the file is too damaged to open,
    // in which case the sidecars move along with it.
    if sidecar(legacy_db_path, "-wal").exists() {
        if let Ok(db) = Database::open(legacy_db_path) {
            drop(db);
        }
    }

    let mut moved = Vec::new();
    move_database(legacy_db_path, target_db, &mut moved)?;
    for index in 1..=SNAPSHOT_SLOTS {
        let from = backup::slot(legacy_db_path, index);
        if from.exists() {
            let to = target_dir.join(file_name(&from)?);
            move_file(&from, &to, &mut moved)?;
        }
    }
    for suffix in ["-wal", "-shm", ".corrupt"] {
        let from = sidecar(legacy_db_path, suffix);
        if from.exists() {
            let to = target_dir.join(file_name(&from)?);
            move_file(&from, &to, &mut moved)?;
        }
    }
    let legacy_dir = legacy_db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for name in LOG_FILES {
        let from = legacy_dir.join(name);
        if from.exists() {
            // A log the move cannot delete (another session still holds it) is
            // left in place: `move_file` reports success and the new session
            // simply starts a fresh family beside the database.
            let to = target_dir.join(name);
            let _ = move_file(&from, &to, &mut moved);
        }
    }
    Ok(moved)
}

/// A database whose library just moved has no snapshots of its own yet, so the
/// first open at the new path writes one like any other start.
pub fn is_migrated(target_db: &Path) -> bool {
    target_db.exists()
}

fn file_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", path.display()),
        )
    })
}

/// `<path><suffix>`, the way SQLite names `-wal` and the way `backup` names
/// `.corrupt`.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Copy → rename → delete the source. The copy goes to a name only this
/// process would choose, so two runs never fight over one temporary file.
fn move_file(from: &Path, to: &Path, moved: &mut Vec<PathBuf>) -> io::Result<()> {
    move_with(from, to, moved, |_| Ok(()))
}

/// The main database, moved with the same copy → rename → delete steps, but
/// with the copy *opened* before the source is retired. That check is the
/// difference between a careful move and a way to lose a workspace: a copy
/// taken while a second session was mid-write can be torn, and a damaged
/// library that is only discovered at the new path has no source left to go
/// back to. Rejecting it here leaves the old folder — and its readable
/// snapshots — exactly where the startup recovery code expects them.
fn move_database(from: &Path, to: &Path, moved: &mut Vec<PathBuf>) -> io::Result<()> {
    move_with(from, to, moved, |candidate| match Database::open(candidate) {
        Ok(db) => {
            drop(db);
            Ok(())
        }
        Err(e) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("moved copy is not a readable database: {e}"),
        )),
    })
}

fn move_with(
    from: &Path,
    to: &Path,
    moved: &mut Vec<PathBuf>,
    check: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let temp = {
        let mut name = to.as_os_str().to_os_string();
        name.push(format!(".migrating-{}", std::process::id()));
        PathBuf::from(name)
    };
    let _ = fs::remove_file(&temp);
    let staged = fs::copy(from, &temp).and_then(|_| check(&temp)).and_then(|_| match fs::rename(&temp, to) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(e)
        }
    });
    if let Err(e) = staged {
        let _ = fs::remove_file(&temp);
        return Err(io::Error::new(
            e.kind(),
            format!("move {} to {}: {e}", from.display(), to.display()),
        ));
    }
    moved.push(to.to_path_buf());
    match fs::remove_file(from) {
        Ok(()) => Ok(()),
        // The destination holds the bytes now, so an undeletable source is
        // inert rather than lost. Naming it is enough to keep a later run from
        // reading it as the live library.
        Err(_) => {
            let inert = {
                let mut name = from.as_os_str().to_os_string();
                name.push(".migrated-away");
                PathBuf::from(name)
            };
            let _ = fs::rename(from, inert);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        let mut out = vec!["quire".to_string()];
        out.extend(values.iter().map(|v| v.to_string()));
        out
    }

    /// The same case the flags would spell, as [`LaunchOptions`] — production
    /// parses the command line in `main.rs`; storage only ever sees this.
    fn options(values: &[&str]) -> LaunchOptions {
        LaunchOptions::from_args(&args(values))
    }

    fn temp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "quire-location-{}-{tag}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// A database the move will accept: `migrate_legacy` opens the copy before
    /// it retires the source, so the fixtures have to be real files.
    fn make_db(path: &Path) {
        drop(Database::open(path).unwrap());
    }

    /// The per-user directory, standing in for `%APPDATA%\Quire`.
    ///
    /// Nothing here may hand the *default-shaped* relative path to `migration`
    /// or `migrate_legacy`: those two are the only ones that act on the working
    /// directory, and the crate root has a real `appdata/quire.db` to move.
    fn per_user_dir(tag: &str) -> PathBuf {
        app_data(&temp(tag)).unwrap()
    }

    #[test]
    fn the_default_now_resolves_to_the_per_user_library() {
        let per_user = per_user_dir("appdata");
        let placement = decide(&options(&[]), &legacy_db(), Some(&per_user));
        assert_eq!(
            Placement::Roaming {
                path: per_user.join("quire.db"),
                legacy: legacy_db(),
            },
            placement
        );
        assert_eq!(Some(legacy_db().as_path()), placement.legacy());
        let _ = fs::remove_dir_all(per_user.parent().unwrap());
    }

    #[test]
    fn portable_mode_keeps_the_library_beside_the_working_directory() {
        let per_user = per_user_dir("appdata2");
        let placement = decide(&options(&["--portable"]), &legacy_db(), Some(&per_user));
        assert_eq!(Placement::Portable(legacy_db()), placement);
        assert_eq!(None, placement.legacy());
        // a portable run resolves to the same file it was asked for, twice over
        assert_eq!(
            legacy_db(),
            migration(&options(&["--portable"]), &legacy_db(), Some(&per_user)).path
        );
        assert_eq!(
            legacy_db(),
            migration(&options(&["--portable"]), &legacy_db(), Some(&per_user)).path
        );
        let _ = fs::remove_dir_all(per_user.parent().unwrap());
    }

    #[test]
    fn an_explicit_db_beats_both_the_environment_and_portable() {
        let per_user = per_user_dir("appdata3");
        let chosen = temp("chosen").join("notes.db");
        let flag = format!("--db={}", chosen.display());
        for spell in [
            options(&["--portable", "--db", &chosen.display().to_string()]),
            options(&["--portable", flag.as_str()]),
        ] {
            assert_eq!(Placement::Fixed(chosen.clone()), decide(&spell, &legacy_db(), Some(&per_user)));
            // and the resolution opens exactly that file, nothing else — twice,
            // because the run must be idempotent
            for _ in 0..2 {
                let run = migration(&spell, &legacy_db(), Some(&per_user));
                assert_eq!(chosen, run.path);
                assert_eq!(None, run.from, "--db never migrates anything");
            }
        }
        let _ = fs::remove_dir_all(per_user.parent().unwrap());
    }

    #[test]
    fn a_caller_that_named_its_own_file_is_never_rerouted() {
        let per_user = per_user_dir("appdata4");
        let mine = temp("mine").join("quire.db");
        // even with --portable in the launch flags, an explicit path is taken literally
        assert_eq!(
            Placement::Fixed(mine.clone()),
            decide(&options(&["--portable"]), &mine, Some(&per_user))
        );
        // the default is recognised with or without the leading "./"
        assert!(is_default(Path::new("./appdata/quire.db")));
        assert!(is_default(Path::new("appdata/quire.db")));
        assert!(!is_default(Path::new("other/quire.db")));
        assert!(!is_default(&mine));
        let _ = fs::remove_dir_all(per_user.parent().unwrap());
    }

    #[test]
    fn nowhere_to_move_means_no_move() {
        assert_eq!(
            Placement::Portable(legacy_db()),
            decide(&options(&[]), &legacy_db(), None)
        );
        assert_eq!(None, app_data(Path::new("")));
        assert_eq!(
            PathBuf::from("R:/Roaming/Quire"),
            app_data(Path::new("R:/Roaming")).unwrap()
        );
    }

    #[test]
    fn the_first_default_run_moves_once_and_reports_where_from() {
        let legacy = temp("legacy7");
        let per_user = temp("per-user7").join("Quire");
        let db = legacy.join("quire.db");
        make_db(&db);
        let placement = Placement::Roaming {
            path: per_user.join("quire.db"),
            legacy: db.clone(),
        };

        let run = migrate_placement(&placement);
        assert_eq!(per_user.join("quire.db"), run.path);
        assert_eq!(Some(db.clone()), run.from, "the notice names what moved");
        // the second run has nothing left to carry
        let run = migrate_placement(&placement);
        assert_eq!(per_user.join("quire.db"), run.path);
        assert_eq!(None, run.from);
        let _ = fs::remove_dir_all(legacy);
        let _ = fs::remove_dir_all(per_user.parent().unwrap());
    }

    #[test]
    fn the_launch_flags_are_what_the_command_line_spells() {
        // main.rs parses its own copy of these; the shape must agree with the
        // flags as users type them, including the exe path at argv[0].
        let exe_named_db = vec![
            "--db".to_string(),
            "C:\\somewhere\\quire.exe".to_string(),
            "--db=notes/my.db".to_string(),
            "--portable".to_string(),
        ];
        let parsed = LaunchOptions::from_args(&exe_named_db);
        assert_eq!(PathBuf::from("notes/my.db"), parsed.db_override.unwrap());
        assert!(parsed.portable);
        assert_eq!(LaunchOptions::default(), LaunchOptions::from_args(&args(&[])));
    }

    #[test]
    fn the_whole_library_moves_in_one_pass() {
        let legacy = temp("legacy");
        let per_user = temp("per-user").join("Quire");
        let db = legacy.join("quire.db");
        make_db(&db);
        fs::write(backup::slot(&db, 1), b"snapshot 1").unwrap();
        fs::write(backup::slot(&db, 2), b"snapshot 2").unwrap();
        fs::write(sidecar(&db, ".corrupt"), b"damaged corpse").unwrap();
        fs::write(legacy.join("quire.log"), b"log history").unwrap();
        fs::write(legacy.join("unrelated.txt"), b"somebody else").unwrap();

        let target = per_user.join("quire.db");
        let moved = migrate_legacy(&db, &target).unwrap();

        assert!(Database::open(&target).is_ok(), "the copy is a database");
        assert_eq!(
            b"snapshot 1".as_slice(),
            fs::read(backup::slot(&target, 1)).unwrap()
        );
        assert_eq!(
            b"snapshot 2".as_slice(),
            fs::read(backup::slot(&target, 2)).unwrap()
        );
        assert_eq!(b"damaged corpse".as_slice(), fs::read(sidecar(&target, ".corrupt")).unwrap());
        assert_eq!(
            b"log history".as_slice(),
            fs::read(per_user.join("quire.log")).unwrap()
        );
        assert_eq!(5, moved.len(), "{moved:?}");
        // the old folder keeps nothing that could be opened again — but it is
        // not ours to empty, so anything else it held stays untouched
        assert!(!db.exists());
        assert!(!backup::slot(&db, 1).exists());
        assert!(!legacy.join("quire.log").exists());
        assert!(legacy.join("unrelated.txt").exists());
        assert!(is_migrated(&target));
        for stale in [legacy, per_user] {
            let _ = fs::remove_dir_all(stale);
        }
    }

    #[test]
    fn a_second_run_does_not_move_a_library_that_is_already_live() {
        let legacy = temp("legacy2");
        let per_user = temp("per-user2").join("Quire");
        let db = legacy.join("quire.db");
        make_db(&db);
        let target = per_user.join("quire.db");

        assert!(!migrate_legacy(&db, &target).unwrap().is_empty());
        fs::write(&target, b"edited after the move").unwrap();
        // a source file appears anyway (an undeletable one, a second working
        // directory): the destination wins, untouched
        fs::write(&db, b"stale").unwrap();
        assert!(migrate_legacy(&db, &target).unwrap().is_empty());
        assert_eq!(b"edited after the move".as_slice(), fs::read(&target).unwrap());
        assert_eq!(b"stale".as_slice(), fs::read(&db).unwrap());
        let _ = fs::remove_dir_all(legacy);
        let _ = fs::remove_dir_all(per_user);
    }

    #[test]
    fn a_database_the_copy_rejects_is_left_where_recovery_can_reach_it() {
        let legacy = temp("legacy6");
        let per_user = temp("per-user6").join("Quire");
        let db = legacy.join("quire.db");
        // not a database: the copy is made, opened, refused — and the source
        // stays, so `open_with_recovery` can still roll back to the snapshot
        // beside it in the old folder
        fs::write(&db, b"SQLite format 4, and a page of nonsense").unwrap();
        fs::write(backup::slot(&db, 1), b"the readable one").unwrap();

        let target = per_user.join("quire.db");
        assert!(migrate_legacy(&db, &target).is_err());
        assert!(db.exists(), "a refused move must not retire the source");
        assert!(backup::slot(&db, 1).exists());
        assert!(!target.exists());
        assert_eq!(
            b"the readable one".as_slice(),
            fs::read(backup::slot(&db, 1)).unwrap()
        );
        let _ = fs::remove_dir_all(legacy);
        let _ = fs::remove_dir_all(per_user.parent().unwrap());
    }

    #[test]
    fn an_empty_legacy_folder_is_not_a_migration() {
        let legacy = temp("legacy3");
        let per_user = temp("per-user3").join("Quire");
        let target = per_user.join("quire.db");
        assert!(migrate_legacy(&legacy.join("quire.db"), &target).unwrap().is_empty());
        assert!(!target.exists());
        assert!(!is_migrated(&target));
        let _ = fs::remove_dir_all(legacy);
        let _ = fs::remove_dir_all(per_user);
    }

    #[test]
    fn a_destination_that_cannot_be_written_leaves_the_source_readable() {
        let legacy = temp("legacy4");
        let db = legacy.join("quire.db");
        fs::write(&db, b"my notes").unwrap();
        fs::write(backup::slot(&db, 1), b"snapshot").unwrap();
        // a *file* where the per-user directory belongs: nothing can be created
        // inside it, so the whole move fails
        let blocker = temp("blocked").join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let target = blocker.join("Quire").join("quire.db");

        assert!(migrate_legacy(&db, &target).is_err());
        assert_eq!(b"my notes".as_slice(), fs::read(&db).unwrap());
        assert!(backup::slot(&db, 1).exists(), "nothing moves before the destination is ready");
        let _ = fs::remove_dir_all(legacy);
        let _ = fs::remove_dir_all(blocker.parent().unwrap());
    }

    #[test]
    fn a_moved_database_still_opens_as_one() {
        use crate::core::persistence::{Change, Repository};
        // the moved bytes must be a database the app can open, not a copy
        // SQLite rejects — the same check `open_with_recovery` runs at startup
        let legacy = temp("legacy5");
        let per_user = temp("per-user5").join("Quire");
        let db = legacy.join("quire.db");
        {
            let repo = crate::storage::SqliteRepository::open(&db).unwrap();
            repo.apply(&[Change::SettingSet {
                key: "theme".into(),
                value: "dark".into(),
            }])
            .unwrap();
            drop(repo);
        }

        let target = per_user.join("quire.db");
        assert!(!migrate_legacy(&db, &target).unwrap().is_empty());
        let reopened = crate::storage::SqliteRepository::open(&target).unwrap();
        assert_eq!(
            Some("dark"),
            reopened.load().unwrap().settings.get("theme").map(String::as_str)
        );
        assert_eq!(Some(target.as_path()), reopened.path());
        let _ = fs::remove_dir_all(legacy);
        let _ = fs::remove_dir_all(per_user.parent().unwrap());
    }
}
