// Debounced persistence pipeline (SPEC §十九): the controller records
// ordered change batches on every mutation; once the input stream has
// been quiet for `debounce` the whole queue is written in one `apply`
// transaction. Ctrl+S / shutdown use `force_flush`. No SQLite write
// happens per keystroke (SPEC §三十三).
//
// Deliberately Slint- and thread-free: the app layer decides when to
// call `flush_if_due` (its own timer) and may call `force_flush` from
// any thread — `Repository` is `Send + Sync`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::persistence::{Change, Repository, StorageError};

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

#[derive(Default)]
struct Dirty {
    queue: Vec<Change>,
    /// Stamp of the first change in the current burst; `None` = clean.
    since_ms: Option<i64>,
}

pub struct PersistenceService {
    repo: Arc<dyn Repository>,
    clock: Arc<dyn Clock>,
    debounce_ms: i64,
    dirty: Mutex<Dirty>,
    last_error: Mutex<Option<StorageError>>,
}

impl PersistenceService {
    pub fn new(repo: Arc<dyn Repository>, clock: Arc<dyn Clock>, debounce_ms: i64) -> Self {
        PersistenceService {
            repo,
            clock,
            debounce_ms,
            dirty: Mutex::new(Dirty::default()),
            last_error: Mutex::new(None),
        }
    }

    pub fn with_default_clock(repo: Arc<dyn Repository>) -> Self {
        Self::new(repo, Arc::new(SystemClock::default()), DEFAULT_DEBOUNCE_MS)
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
    /// write was attempted. Cheap enough to call from a periodic tick.
    pub fn flush_if_due(&self) -> Result<bool, StorageError> {
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
}
