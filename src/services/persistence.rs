// Debounced persistence pipeline (SPEC §十九): the controller records
// ordered change batches on every mutation; once the input stream has
// been quiet for `debounce` the whole queue is written in one `apply`
// transaction. Ctrl+S / shutdown use `force_flush`. No SQLite write
// happens per keystroke (SPEC §三十三).
//
// The same two entry points also carry the periodic database snapshot (M8
// D10): the app already arms a timer per recorded burst, so a snapshot that
// rides on the flush needs no thread and no second timer of its own.
//
// Deliberately Slint- and thread-free: the app layer decides when to
// call `flush_if_due` (its own timer) and may call `force_flush` from
// any thread — `Repository` is `Send + Sync`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::persistence::{Change, Repository, StorageError};
use crate::services::logging;
use crate::storage::SqliteRepository;

/// Monotonic milliseconds; injectable so tests drive the debounce window
/// with a fake clock instead of sleeping.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

#[derive(Default)]
pub struct SystemClock {
    origin: OnceLock<i64>,
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        let origin = self.origin.get_or_init(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        });
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        now - origin
    }
}

/// Settable fake clock for tests.
#[derive(Default)]
pub struct FakeClock {
    now: AtomicI64,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, ms: i64) {
        self.now.store(ms, Ordering::SeqCst);
    }

    pub fn advance(&self, delta_ms: i64) {
        self.now.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

/// Default quiet period before a batch is written (SPEC §十九 "一小段时间").
pub const DEFAULT_DEBOUNCE_MS: i64 = 300;

/// Default period between in-session database snapshots (M8 D10, SPEC
/// §二十五): the loss window the startup snapshot alone leaves is "everything
/// since open", which for a working day is far too much. Ten minutes costs one
/// `VACUUM INTO` per ten minutes of activity (≈25–50 ms for a scene-D
/// workspace, docs/PERFORMANCE.md).
pub const DEFAULT_SNAPSHOT_INTERVAL_MS: i64 = 10 * 60 * 1000;

#[derive(Default)]
struct Dirty {
    queue: Vec<Change>,
    /// Stamp of the first change in the current burst; `None` = clean.
    since_ms: Option<i64>,
}

/// Rotating state of the periodic snapshot. `last_ms` is when the last one
/// ran; a fresh service counts as "just ran", because opening the database
/// already wrote a snapshot (storage::backup). `pending` says a write landed
/// since then, which is the only reason a snapshot is worth taking — an idle
/// session has nothing new to protect.
#[derive(Default)]
struct SnapshotState {
    last_ms: i64,
    pending: bool,
}

pub struct PersistenceService {
    repo: Arc<dyn Repository>,
    clock: Arc<dyn Clock>,
    debounce_ms: i64,
    dirty: Mutex<Dirty>,
    last_error: Mutex<Option<StorageError>>,
    snapshot_hook: Option<SnapshotHook>,
    snapshot_interval_ms: i64,
    snapshot_state: Mutex<SnapshotState>,
    snapshot_error: Mutex<Option<StorageError>>,
}

/// What a periodic snapshot runs: `storage::backup::snapshot` for the app's
/// own database, a counting closure in a test.
type SnapshotHook = Arc<dyn Fn() -> Result<(), StorageError> + Send + Sync>;

impl PersistenceService {
    pub fn new(repo: Arc<dyn Repository>, clock: Arc<dyn Clock>, debounce_ms: i64) -> Self {
        let now = clock.now_ms();
        PersistenceService {
            repo,
            clock,
            debounce_ms,
            dirty: Mutex::new(Dirty::default()),
            last_error: Mutex::new(None),
            snapshot_hook: None,
            snapshot_interval_ms: DEFAULT_SNAPSHOT_INTERVAL_MS,
            snapshot_state: Mutex::new(SnapshotState {
                last_ms: now,
                pending: false,
            }),
            snapshot_error: Mutex::new(None),
        }
    }

    pub fn with_default_clock(repo: Arc<dyn Repository>) -> Self {
        Self::new(repo, Arc::new(SystemClock::default()), DEFAULT_DEBOUNCE_MS)
    }

    /// Attach the periodic snapshot: once `interval_ms` has passed since the
    /// last one *and* something was written, `hook` runs from the flush path
    /// that is already ticking. A service without a hook behaves exactly as
    /// before, so the startup snapshot stays the only one until the app wires
    /// this (docs/M8_FEEDBACK.md #12).
    pub fn with_snapshotter(
        mut self,
        interval_ms: i64,
        hook: impl Fn() -> Result<(), StorageError> + Send + Sync + 'static,
    ) -> Self {
        self.snapshot_hook = Some(Arc::new(hook));
        self.snapshot_interval_ms = interval_ms.max(0);
        self
    }

    /// The app's one-line version: snapshot the SQLite file behind `repo` at
    /// [`DEFAULT_SNAPSHOT_INTERVAL_MS`]. Needs the concrete `Arc` because
    /// `Repository` has no snapshot method (ADR-0014's precedent,
    /// docs/M8_FEEDBACK.md #3). For another period use
    /// [`Self::with_snapshotter`] with `SqliteRepository::snapshot`.
    pub fn with_database_snapshots(self, repo: &Arc<SqliteRepository>) -> Self {
        let snapshotted = repo.clone();
        self.with_snapshotter(DEFAULT_SNAPSHOT_INTERVAL_MS, move || snapshotted.snapshot())
    }

    fn dirty(&self) -> MutexGuard<'_, Dirty> {
        self.dirty.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn last_error(&self) -> MutexGuard<'_, Option<StorageError>> {
        self.last_error.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue one change batch (one user operation / command).
    pub fn record(&self, changes: Vec<Change>) {
        self.record_all([changes])
    }

    /// Queue several change batches (e.g. a burst replayed at shutdown).
    pub fn record_all(&self, batches: impl IntoIterator<Item = Vec<Change>>) {
        let now = self.clock.now_ms();
        let mut dirty = self.dirty();
        for batch in batches {
            if dirty.since_ms.is_none() && !batch.is_empty() {
                dirty.since_ms = Some(now);
            }
            dirty.queue.extend(batch);
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.dirty().queue.is_empty()
    }

    pub fn pending_len(&self) -> usize {
        self.dirty().queue.len()
    }

    /// When the next `flush_if_due` can succeed, for scheduling a timer.
    pub fn next_deadline_ms(&self) -> Option<i64> {
        let dirty = self.dirty();
        dirty.since_ms.map(|since| since + self.debounce_ms)
    }

    /// Write the queue if the quiet period elapsed; returns `true` when a
    /// write was attempted. Cheap enough to call from a periodic tick — and
    /// the tick that carries the periodic snapshot (D10).
    pub fn flush_if_due(&self) -> Result<bool, StorageError> {
        let wrote = self.write_due_batch();
        self.snapshot_if_due();
        wrote
    }

    fn write_due_batch(&self) -> Result<bool, StorageError> {
        let now = self.clock.now_ms();
        let batch = {
            let mut dirty = self.dirty();
            let due = dirty.since_ms.is_some_and(|since| now - since >= self.debounce_ms);
            if !due || dirty.queue.is_empty() {
                return Ok(false);
            }
            dirty.since_ms = None;
            std::mem::take(&mut dirty.queue)
        };
        match self.write(batch) {
            None => Ok(false),
            Some(result) => result.map(|()| true),
        }
    }

    /// Write everything queued, right now (Ctrl+S, program shutdown —
    /// SPEC §十九). Returns an error if the write failed.
    pub fn force_flush(&self) -> Result<(), StorageError> {
        let wrote = self.write_queued_now();
        self.snapshot_if_due();
        wrote
    }

    fn write_queued_now(&self) -> Result<(), StorageError> {
        let batch = {
            let mut dirty = self.dirty();
            dirty.since_ms = None;
            std::mem::take(&mut dirty.queue)
        };
        match self.write(batch) {
            None => Ok(()),
            Some(Err(e)) => Err(e),
            Some(Ok(())) => Ok(()),
        }
    }

    /// The most recent deferred write failure, if the queue is still
    /// retrying. Cleared on the next successful write.
    pub fn take_last_error(&self) -> Option<StorageError> {
        self.last_error().take()
    }

    /// The most recent periodic-snapshot failure, if one has happened since the
    /// last call. The flush never fails because of it (the writes are already
    /// durable); this is how the app gets to say "this session is not backed
    /// up", the same fact `OpenReport::backup_failed` says at startup.
    pub fn take_snapshot_error(&self) -> Option<StorageError> {
        self.snapshot_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Take a snapshot if the period elapsed and something was written since
    /// the last one. Runs on whatever thread the flush ran on, and never
    /// fails the flush: the error is parked for `take_snapshot_error` and
    /// written to the log, and the period restarts so a failing disk does not
    /// produce one `VACUUM INTO` per tick.
    fn snapshot_if_due(&self) -> bool {
        let Some(hook) = self.snapshot_hook.clone() else {
            return false;
        };
        let now = self.clock.now_ms();
        {
            let mut state = self.snapshot_state();
            if !state.pending || now - state.last_ms < self.snapshot_interval_ms {
                return false;
            }
            state.last_ms = now;
            state.pending = false;
        }
        match hook() {
            Ok(()) => true,
            Err(e) => {
                *self
                    .snapshot_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(e.clone());
                logging::warn(&format!("periodic database snapshot failed: {e}"));
                false
            }
        }
    }

    fn snapshot_state(&self) -> MutexGuard<'_, SnapshotState> {
        self.snapshot_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Write one taken batch; on failure the changes go back at the front
    /// of the queue (retried next deadline) and the error is recorded.
    /// `Some(())`/`Some(Err)` when a write happened, `None` when the batch
    /// was empty.
    fn write(&self, batch: Vec<Change>) -> Option<Result<(), StorageError>> {
        if batch.is_empty() {
            return None;
        }
        match self.repo.apply(&batch) {
            Ok(()) => {
                *self.last_error() = None;
                // Durable data is what a snapshot is for: remember that this
                // session has new bytes worth protecting.
                self.snapshot_state().pending = true;
                Some(Ok(()))
            }
            Err(e) => {
                let mut dirty = self.dirty();
                if dirty.since_ms.is_none() {
                    // retry at the next tick without re-arming a full window
                    dirty.since_ms = Some(self.clock.now_ms() - self.debounce_ms);
                }
                dirty.queue.splice(0..0, batch);
                *self.last_error() = Some(e.clone());
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{BlockId, OrderKey, Page, PageId, PersistedState};
    use std::sync::Mutex as StdMutex;

    /// Records every `apply` call; can make the next one fail.
    struct SpyRepo {
        batches: StdMutex<Vec<Vec<Change>>>,
        fail_next: std::sync::atomic::AtomicBool,
    }

    impl SpyRepo {
        fn new() -> Arc<Self> {
            Arc::new(SpyRepo {
                batches: StdMutex::new(Vec::new()),
                fail_next: std::sync::atomic::AtomicBool::new(false),
            })
        }
        fn batches(&self) -> Vec<Vec<Change>> {
            self.batches.lock().unwrap().clone()
        }
    }

    impl Repository for SpyRepo {
        fn load(&self) -> Result<PersistedState, StorageError> {
            Ok(PersistedState::default())
        }
        fn apply(&self, changes: &[Change]) -> Result<(), StorageError> {
            if self
                .fail_next
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Err(StorageError::Sql("injected failure".into()));
            }
            self.batches.lock().unwrap().push(changes.to_vec());
            Ok(())
        }
        fn replace_all(&self, _state: &PersistedState) -> Result<(), StorageError> {
            Ok(())
        }
    }

    fn page_created(id: u64) -> Change {
        Change::PageCreated(Page {
            id: PageId(id),
            title: "t".into(),
            parent: None,
            order: OrderKey::FIRST,
            favorite: false,
            expanded: false,
        })
    }

    fn text_set(id: u64, text: &str) -> Change {
        Change::BlockTextSet {
            id: BlockId(id),
            text: text.into(),
        }
    }

    fn service() -> (PersistenceService, Arc<SpyRepo>, Arc<FakeClock>) {
        let repo = SpyRepo::new();
        let clock = Arc::new(FakeClock::new());
        let svc = PersistenceService::new(repo.clone(), clock.clone(), 300);
        (svc, repo, clock)
    }

    #[test]
    fn nothing_is_written_before_the_quiet_period() {
        let (svc, repo, clock) = service();
        svc.record(vec![text_set(1, "a")]);
        for t in [0, 100, 299] {
            clock.set(t);
            assert_eq!(svc.flush_if_due().unwrap(), false);
        }
        assert!(repo.batches().is_empty());
        clock.set(300);
        assert_eq!(svc.flush_if_due().unwrap(), true);
        assert_eq!(repo.batches(), vec![vec![text_set(1, "a")]]);
    }

    #[test]
    fn typing_burst_coalesces_into_one_batch() {
        let (svc, repo, clock) = service();
        for ch in "hello 世界".chars() {
            svc.record(vec![text_set(1, &ch.to_string())]);
            clock.advance(30); // keystrokes inside the window
            assert_eq!(svc.flush_if_due().unwrap(), false);
        }
        clock.advance(300);
        assert_eq!(svc.flush_if_due().unwrap(), true);
        let batches = repo.batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), "hello 世界".chars().count());
        assert!(!svc.has_pending());
    }

    #[test]
    fn window_arms_from_first_change_not_last() {
        let (svc, repo, clock) = service();
        svc.record(vec![text_set(1, "a")]); // t = 0
        clock.set(250);
        svc.record(vec![text_set(1, "b")]); // still same burst
        clock.set(300);
        assert_eq!(svc.flush_if_due().unwrap(), true);
        assert_eq!(repo.batches().len(), 1);
        // after a write, the next change starts a fresh window
        svc.record(vec![text_set(1, "c")]); // t = 300
        clock.set(599);
        assert_eq!(svc.flush_if_due().unwrap(), false);
        clock.set(600);
        assert_eq!(svc.flush_if_due().unwrap(), true);
        assert_eq!(repo.batches().len(), 2);
    }

    #[test]
    fn force_flush_writes_immediately() {
        let (svc, repo, _clock) = service();
        svc.record(vec![page_created(1)]);
        svc.force_flush().unwrap();
        assert_eq!(repo.batches(), vec![vec![page_created(1)]]);
        // flushing a clean service is a no-op, not an empty write
        svc.force_flush().unwrap();
        assert_eq!(repo.batches().len(), 1);
    }

    #[test]
    fn failed_write_is_retried_in_order() {
        let (svc, repo, clock) = service();
        repo.fail_next.store(true, Ordering::SeqCst);
        svc.record(vec![text_set(1, "a")]);
        clock.set(300);
        assert!(svc.flush_if_due().is_err());
        assert_eq!(svc.pending_len(), 1);
        assert_eq!(repo.batches().len(), 0);
        assert!(svc.take_last_error().is_some());
        // queue stays ordered: new change goes behind the failed one
        svc.record(vec![text_set(1, "b")]);
        clock.set(301); // retry deadline is re-armed back
        assert_eq!(svc.flush_if_due().unwrap(), true);
        assert_eq!(
            repo.batches(),
            vec![vec![text_set(1, "a"), text_set(1, "b")]]
        );
        assert!(svc.take_last_error().is_none());
    }

    #[test]
    fn deadline_and_pending_reflect_the_queue() {
        let (svc, _repo, clock) = service();
        assert_eq!(svc.next_deadline_ms(), None);
        svc.record(vec![text_set(1, "a")]);
        assert_eq!(svc.next_deadline_ms(), Some(300));
        clock.set(50);
        svc.record(vec![text_set(1, "b")]);
        assert_eq!(svc.next_deadline_ms(), Some(300)); // unchanged: same burst
        assert!(svc.has_pending());
    }

    #[test]
    fn system_clock_is_monotonic_enough() {
        let clock = SystemClock::default();
        let a = clock.now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(clock.now_ms() > a);
    }

    // ---- periodic snapshot (M8 D10) --------------------------------------

    use std::sync::atomic::AtomicUsize;

    /// The same service, with a snapshot hook that counts its calls (and can
    /// fail like a full disk would).
    fn snapshot_service(
        interval_ms: i64,
        fail: bool,
    ) -> (
        PersistenceService,
        Arc<SpyRepo>,
        Arc<FakeClock>,
        Arc<AtomicUsize>,
    ) {
        let repo = SpyRepo::new();
        let clock = Arc::new(FakeClock::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let svc =
            PersistenceService::new(repo.clone(), clock.clone(), 300).with_snapshotter(
                interval_ms,
                move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    if fail {
                        Err(StorageError::Sql("no space left on device".into()))
                    } else {
                        Ok(())
                    }
                },
            );
        (svc, repo, clock, calls)
    }

    fn snapshots(calls: &Arc<AtomicUsize>) -> usize {
        calls.load(Ordering::SeqCst)
    }

    #[test]
    fn a_snapshot_needs_both_the_period_and_a_write() {
        let (svc, _repo, clock, calls) = snapshot_service(600_000, false);
        // a burst inside the period writes but does not snapshot
        svc.record(vec![text_set(1, "a")]);
        clock.set(300);
        assert!(svc.flush_if_due().unwrap());
        assert_eq!(snapshots(&calls), 0, "ten minutes have not passed");

        // the period elapses: the next flush that writes takes one
        svc.record(vec![text_set(1, "b")]);
        clock.set(600_000);
        assert!(svc.flush_if_due().unwrap());
        assert_eq!(snapshots(&calls), 1);
    }

    #[test]
    fn the_period_restarts_from_the_snapshot_not_the_write() {
        let (svc, _repo, clock, calls) = snapshot_service(1000, false);
        svc.record(vec![text_set(1, "a")]);
        clock.set(1000);
        assert!(svc.flush_if_due().unwrap());
        assert_eq!(snapshots(&calls), 1, "write and snapshot on the same tick");
        // written again 500 ms after that snapshot: too early for another one
        svc.record(vec![text_set(1, "b")]);
        clock.set(1500);
        assert!(svc.flush_if_due().unwrap());
        assert_eq!(snapshots(&calls), 1);
        // the write that is now pending is saved by the next tick past the
        // deadline, whether or not that tick has a batch of its own
        clock.set(2000);
        assert!(
            !svc.flush_if_due().unwrap(),
            "nothing was queued, so no data write happened"
        );
        assert_eq!(snapshots(&calls), 2);
    }

    #[test]
    fn force_flush_carries_the_snapshot_too() {
        // Ctrl+S and shutdown are the other half of the app's flush traffic
        let (svc, _repo, clock, calls) = snapshot_service(1000, false);
        svc.record(vec![page_created(1)]);
        clock.set(1000);
        svc.force_flush().unwrap();
        assert_eq!(snapshots(&calls), 1);
    }

    #[test]
    fn an_idle_session_writes_no_snapshots() {
        let (svc, _repo, clock, calls) = snapshot_service(1000, false);
        for t in [0, 500, 1000, 5000] {
            clock.set(t);
            svc.force_flush().unwrap();
            assert_eq!(snapshots(&calls), 0, "nothing changed since the open");
        }
    }

    #[test]
    fn a_failing_snapshot_does_not_break_the_flush() {
        let (svc, repo, clock, calls) = snapshot_service(1000, true);
        svc.record(vec![text_set(1, "a")]);
        clock.set(1000);
        assert!(
            svc.flush_if_due().unwrap(),
            "the data write itself succeeded; the snapshot is insurance"
        );
        assert_eq!(repo.batches().len(), 1);
        assert_eq!(snapshots(&calls), 1);
        let error = svc.take_snapshot_error().expect("reported for the UI");
        assert!(error.to_string().contains("no space left"), "{error}");
        assert!(svc.take_snapshot_error().is_none(), "reported once");
        // the period restarted, so a dead disk is not retried on every tick
        svc.record(vec![text_set(1, "b")]);
        clock.set(1400);
        assert!(svc.flush_if_due().unwrap());
        assert_eq!(snapshots(&calls), 1);
    }

    #[test]
    fn a_service_without_a_snapshotter_is_unchanged() {
        let (svc, repo, clock) = service();
        for t in [300, 1000, 600_000] {
            svc.record(vec![text_set(1, "a")]);
            clock.set(t * 10);
            assert!(svc.flush_if_due().unwrap());
        }
        assert_eq!(repo.batches().len(), 3);
        assert!(svc.take_snapshot_error().is_none());
    }
}
