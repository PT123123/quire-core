// Settings and metadata persistence (the "state that must survive a restart"
// half of SPEC §二十五). The document goes through `PersistenceService`; this
// is the small key/value layer behind it: theme, sidebar expansion, window
// size, and whatever else the UI wants to remember.
//
// It talks to the frozen `Repository` trait only, through the change
// variants the contract has (`SettingSet`/`SettingDelete`, `MetaSet`/
// `MetaDelete`), so it works against the SQLite backend and against a test
// double alike. `settings` and `metadata` are separate tables with separate
// variants: settings are the user's choices, metadata is the app's note about
// the session. Which key lives where stays the caller's decision; this module
// only keeps the two namespaces apart.
//
// Cost note: reading means `Repository::load()`, which reads every page and
// block too (≈7 ms for a 10 000-block workspace, docs/PERFORMANCE.md). That is
// fine once at startup; when the app already holds the state it loaded, build
// the maps with `Settings::from_state` instead of calling back here.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::core::persistence::{Change, Repository, StorageError};
use crate::core::types::{PageId, PersistedState};

/// Removals are real deletes now (`SettingDelete`/`MetaDelete`); this only
/// filters rows left behind by databases written before the delete variants
/// existed, where a removal was stored as an empty value.
pub const TOMBSTONE: &str = "";

/// One namespace's contents, plus the two keys the UI already edits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    map: BTreeMap<String, String>,
}

impl Settings {
    /// The user's theme, as the settings panel writes it.
    pub const KEY_THEME: &'static str = "theme";
    /// Pages whose sidebar branch is open.
    pub const KEY_EXPANDED: &'static str = "sidebar.expanded";

    pub fn new() -> Self {
        Self::default()
    }

    /// Take the map a repository handed back, dropping removal tombstones.
    pub fn from_map(map: BTreeMap<String, String>) -> Self {
        Settings {
            map: map
                .into_iter()
                .filter(|(_, value)| value != TOMBSTONE)
                .collect(),
        }
    }

    /// Read both namespaces out of a state the caller already loaded, so
    /// startup stays one `load`.
    pub fn from_state(state: &PersistedState) -> (Settings, Settings) {
        (
            Settings::from_map(state.settings.clone()),
            Settings::from_map(state.meta.clone()),
        )
    }

    pub fn to_map(&self) -> BTreeMap<String, String> {
        self.map.clone()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.map.insert(key.into(), value.into());
    }

    pub fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn theme(&self) -> Option<&str> {
        self.get(Self::KEY_THEME)
    }

    pub fn set_theme(&mut self, theme: &str) {
        self.set(Self::KEY_THEME, theme);
    }

    /// Expanded pages, encoded as ascending comma-separated ids under one key.
    pub fn expanded_pages(&self) -> Vec<PageId> {
        self.get(Self::KEY_EXPANDED)
            .into_iter()
            .flat_map(|list| list.split(','))
            .filter_map(|token| token.trim().parse::<u64>().ok())
            .map(PageId)
            .collect()
    }

    pub fn set_expanded_pages(&mut self, pages: &[PageId]) {
        let mut ids: Vec<u64> = pages.iter().map(|id| id.as_u64()).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            self.remove(Self::KEY_EXPANDED);
        } else {
            let list: Vec<String> = ids.into_iter().map(|id| id.to_string()).collect();
            self.set(Self::KEY_EXPANDED, &list.join(","));
        }
    }
}

/// Writes settings and metadata through whatever `Repository` the app holds.
pub struct SettingsStore {
    repo: Arc<dyn Repository>,
}

impl SettingsStore {
    pub fn new(repo: Arc<dyn Repository>) -> Self {
        SettingsStore { repo }
    }

    pub fn load_settings(&self) -> Result<Settings, StorageError> {
        Ok(Settings::from_map(self.repo.load()?.settings))
    }

    pub fn load_meta(&self) -> Result<Settings, StorageError> {
        Ok(Settings::from_map(self.repo.load()?.meta))
    }

    /// The changes that move the stored settings to `desired`, without
    /// writing them: hand these to `PersistenceService` to keep a burst
    /// (window dragging, a slider) inside the debounce window.
    pub fn settings_changes(&self, desired: &Settings) -> Result<Vec<Change>, StorageError> {
        Ok(diff(
            self.repo.load()?.settings,
            desired.to_map(),
            |key, value| Change::SettingSet { key, value },
            |key| Change::SettingDelete { key },
        ))
    }

    pub fn meta_changes(&self, desired: &Settings) -> Result<Vec<Change>, StorageError> {
        Ok(diff(
            self.repo.load()?.meta,
            desired.to_map(),
            |key, value| Change::MetaSet { key, value },
            |key| Change::MetaDelete { key },
        ))
    }

    pub fn save_settings(&self, desired: &Settings) -> Result<(), StorageError> {
        self.write(self.settings_changes(desired)?)
    }

    pub fn save_meta(&self, desired: &Settings) -> Result<(), StorageError> {
        self.write(self.meta_changes(desired)?)
    }

    fn write(&self, changes: Vec<Change>) -> Result<(), StorageError> {
        if changes.is_empty() {
            return Ok(());
        }
        self.repo.apply(&changes)
    }
}

/// Add or modify what changed and delete what disappeared; unchanged keys
/// stay out of the batch, so a save of three keys is three changes. A key
/// deleted in the desired map but absent from `current` emits nothing.
fn diff(
    current: BTreeMap<String, String>,
    desired: BTreeMap<String, String>,
    make: impl Fn(String, String) -> Change,
    del: impl Fn(String) -> Change,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (key, value) in &desired {
        if current.get(key).map(String::as_str) != Some(value.as_str()) {
            changes.push(make(key.clone(), value.clone()));
        }
    }
    for key in current.keys() {
        if !desired.contains_key(key) {
            changes.push(del(key.clone()));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{Block, BlockId, BlockKind, ColorKind, Lang, OrderKey, Page};
    use std::sync::Mutex;

    /// In-memory stand-in: applies each change set to its own
    /// `PersistedState`, which is the contract the SQLite backend implements.
    struct FakeRepo {
        state: Mutex<PersistedState>,
        batches: Mutex<Vec<Vec<Change>>>,
    }

    impl FakeRepo {
        fn new() -> Arc<Self> {
            Arc::new(FakeRepo {
                state: Mutex::new(PersistedState::default()),
                batches: Mutex::new(Vec::new()),
            })
        }
        fn batches(&self) -> Vec<Vec<Change>> {
            self.batches.lock().unwrap().clone()
        }
        fn settings(&self) -> BTreeMap<String, String> {
            self.state.lock().unwrap().settings.clone()
        }
    }

    impl Repository for FakeRepo {
        fn load(&self) -> Result<PersistedState, StorageError> {
            Ok(self.state.lock().unwrap().clone())
        }
        fn apply(&self, changes: &[Change]) -> Result<(), StorageError> {
            let mut state = self.state.lock().unwrap();
            for change in changes {
                match change {
                    Change::SettingSet { key, value } => {
                        state.settings.insert(key.clone(), value.clone());
                    }
                    Change::SettingDelete { key } => {
                        state.settings.remove(key);
                    }
                    Change::MetaSet { key, value } => {
                        state.meta.insert(key.clone(), value.clone());
                    }
                    Change::MetaDelete { key } => {
                        state.meta.remove(key);
                    }
                    _ => {}
                }
            }
            self.batches.lock().unwrap().push(changes.to_vec());
            Ok(())
        }
        fn replace_all(&self, next: &PersistedState) -> Result<(), StorageError> {
            *self.state.lock().unwrap() = next.clone();
            Ok(())
        }
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let store = SettingsStore::new(FakeRepo::new());
        let mut desired = Settings::new();
        desired.set_theme("dark");
        desired.set("font", "serif");
        desired.set_expanded_pages(&[PageId(9), PageId(3), PageId(9)]);
        store.save_settings(&desired).unwrap();

        let loaded = store.load_settings().unwrap();
        assert_eq!(Some("dark"), loaded.theme());
        assert_eq!(Some("serif"), loaded.get("font"));
        assert_eq!(vec![PageId(3), PageId(9)], loaded.expanded_pages());
        assert_eq!(desired, loaded);
    }

    #[test]
    fn meta_and_settings_are_separate_namespaces() {
        let repo = FakeRepo::new();
        let store = SettingsStore::new(repo.clone());
        let mut meta = Settings::new();
        meta.set("last.page", "7");
        store.save_meta(&meta).unwrap();
        assert!(store.load_settings().unwrap().is_empty());
        assert_eq!(Some("7"), store.load_meta().unwrap().get("last.page"));

        // saving settings leaves metadata alone, and vice versa
        let mut settings = Settings::new();
        settings.set_theme("light");
        store.save_settings(&settings).unwrap();
        assert_eq!(Some("7"), store.load_meta().unwrap().get("last.page"));
        assert_eq!(None, store.load_settings().unwrap().get("last.page"));
    }

    #[test]
    fn removing_a_key_deletes_the_row_and_it_reads_back_absent() {
        let repo = FakeRepo::new();
        let store = SettingsStore::new(repo.clone());
        let mut first = Settings::new();
        first.set_theme("dark");
        first.set("font", "serif");
        store.save_settings(&first).unwrap();

        let mut second = first.clone();
        second.remove("font");
        assert_eq!(
            vec![Change::SettingDelete {
                key: "font".into(),
            }],
            store.settings_changes(&second).unwrap()
        );
        store.save_settings(&second).unwrap();

        assert_eq!(None, store.load_settings().unwrap().get("font"));
        // the row is really gone from the table, not hidden by the read
        assert_eq!(None, repo.settings().get("font"));
        // saving again emits nothing: the delete converged
        assert!(
            store.settings_changes(&second).unwrap().is_empty(),
            "no churn after the row is gone"
        );
    }

    #[test]
    fn an_unchanged_save_writes_nothing() {
        let repo = FakeRepo::new();
        let store = SettingsStore::new(repo.clone());
        let mut settings = Settings::new();
        settings.set_theme("dark");
        store.save_settings(&settings).unwrap();
        store.save_settings(&settings).unwrap();
        assert_eq!(
            vec![vec![Change::SettingSet {
                key: "theme".into(),
                value: "dark".into(),
            }]],
            repo.batches()
        );
    }

    #[test]
    fn changes_can_be_queued_instead_of_written() {
        let repo = FakeRepo::new();
        let store = SettingsStore::new(repo.clone());
        let mut desired = Settings::new();
        desired.set("window.width", "1200");
        let changes = store.settings_changes(&desired).unwrap();
        assert!(repo.batches().is_empty(), "the caller decides when to write");
        repo.apply(&changes).unwrap();
        assert_eq!(
            Some("1200"),
            store.load_settings().unwrap().get("window.width")
        );
    }

    #[test]
    fn a_state_loaded_for_the_document_already_carries_both_maps() {
        let mut state = PersistedState::default();
        state.blocks.push(Block {
            id: BlockId(1),
            page: PageId(1),
            parent: None,
            order: OrderKey::FIRST,
            kind: BlockKind::Paragraph,
            text: "hello".into(),
            checked: false,
            marks: Vec::new(),
            color: ColorKind::Default,
            background: ColorKind::Default,
            page_ref: None,
            folded: false,
            attachment: None,
            img_percent: 100,
            columns: 0,
            lang: Lang::Plain,
        });
        state.pages.push(Page {
            id: PageId(1),
            title: "p1".into(),
            parent: None,
            order: OrderKey::FIRST,
            favorite: false,
            expanded: true,
            font: crate::core::PageFont::default(),
            full_width: false,
            small_text: false,
            icon: String::new(),
            cover: None,
            locked: false,
            template: false,
        });
        state.settings.insert("theme".into(), "dark".into());
        state.meta.insert("last.page".into(), "1".into());
        state.meta.insert("stale".into(), TOMBSTONE.into());
        let (settings, meta) = Settings::from_state(&state);
        assert_eq!(Some("dark"), settings.theme());
        assert_eq!(Some("1"), meta.get("last.page"));
        assert_eq!(None, meta.get("stale"));
    }

    #[test]
    fn a_page_id_list_round_trips_through_one_key() {
        let mut settings = Settings::new();
        settings.set_expanded_pages(&[PageId(12), PageId(3)]);
        assert_eq!(Some("3,12"), settings.get(Settings::KEY_EXPANDED));
        assert_eq!(vec![PageId(3), PageId(12)], settings.expanded_pages());
        // clearing drops the key, so the next save deletes the row
        settings.set_expanded_pages(&[]);
        assert_eq!(None, settings.get(Settings::KEY_EXPANDED));
        // junk inside the list is skipped, not fatal
        let mut sloppy = Settings::new();
        sloppy.set(Settings::KEY_EXPANDED, "4, ,x,8");
        assert_eq!(vec![PageId(4), PageId(8)], sloppy.expanded_pages());
    }
}
