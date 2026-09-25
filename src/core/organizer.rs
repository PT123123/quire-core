// Notes and tasks (SPEC §四十一) — the organizer: a second top-level area next
// to the document editor, with entities of its own.
//
// Data only, like every other module in `core`: no SQL, no Slint, no clock. The
// two instants (`created` / `edited`) are unix seconds *stamped by the caller*
// and carried inside the row, which is what makes an undone edit replay the row
// it described rather than inventing a new birthday for it.
//
// Three decisions are worth stating here, because everything downstream (the
// change list, the merge, the store) is their consequence:
//
// * **A separate entity layer, not blocks.** A note is not a paragraph and a
//   task is not a `Todo` block. The document editor's model is a tree of blocks
//   inside a page, and §四十一's area has no page, no parent, no caret and no
//   inline marks — its lists are derived (inbox / today / a user's list), which
//   a block tree cannot express without a second meaning for `parent`. So these
//   are rows of their own three tables, reached through the same `Repository`
//   change stream as everything else.
// * **`tags` and `subtasks` are denormalized into JSON columns** on the row that
//   owns them. They are only ever read and written *with* their host, have no
//   identity anyone references, and are never the subject of a query ("which
//   tasks carry this tag" is a scan of a catalog that is already in memory).
//   Normalizing them into `db_value_items`-style tables would buy a join nobody
//   runs and two more write paths to keep in step — the opposite trade from
//   `db_values`, which SQLite genuinely has to filter and sort on.
// * **Enums are stored as short stable strings** (`as_str` / `try_from_str` /
//   `from_stored`), like `BlockKind` and `PropertyKind`: a spelling this build
//   does not know is a *value* it cannot mean, and the two ways to handle that
//   (fold it, or report the file as corrupt) are answered per type below rather
//   than by guessing at read time.
//
// The inbox is the one unusual shape: `ListId(0)` is a **sentinel**, not a row.
// See [`ListId::INBOX`].

use std::fmt;

use super::types::{ColorKind, OrderKey};

/// The id newtypes. A local copy of `core::types`'s `id_newtype!` rather than an
/// export of it (that macro is private to its module) and of `core::database`'s
/// `db_id!`, which is the same copy made for the same reason: an id is an id and
/// never an index (SPEC §九), and the three names never mix by accident.
macro_rules! org_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);

        impl $name {
            pub fn as_u64(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

org_id!(NoteId, "Stable identifier of a note.");
org_id!(TaskId, "Stable identifier of a task.");
org_id!(
    ListId,
    "Stable identifier of a task list. `0` is the inbox — a sentinel and not a row (see [`ListId::INBOX`])."
);

impl ListId {
    /// The inbox: the list a task with no list of its own belongs to.
    ///
    /// **A sentinel and not a row.** `task_lists` has no row 0, because the
    /// inbox has nothing to store — no name to rename, no colour to pick, no
    /// place in the chip order — and a row would be one more thing a merge or a
    /// delete could disagree about. `tasks.list = 0` therefore means "in the
    /// inbox" with no foreign key and no guaranteed row behind it, the same way
    /// `blocks.attachment` names a file with no constraint behind *it*: the
    /// pointer is honest about what it means and the renderer is what decides
    /// how a dangling one paints.
    pub const INBOX: ListId = ListId(0);

    /// Whether this id is the inbox rather than a stored list. The one question
    /// every list-shaped read has to ask, asked in one place.
    pub fn is_inbox(self) -> bool {
        self.0 == 0
    }
}

/// One note (SPEC §四十一 「笔记」): a title, a body, and nothing else the
/// document editor would need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: NoteId,
    pub title: String,
    /// The body as plain text with its newlines kept. v1 deliberately renders
    /// no Markdown: a note that stored *and* rendered a second content format
    /// is the mistake §三十八 refuses for templates, and the editor's budget is
    /// better spent on the task half.
    pub body: String,
    /// Pinned notes sort above the rest of the list. The flag is the user's;
    /// nothing derives it (a "last edited" ordering is a *sort*, not a pin).
    pub pinned: bool,
    pub tags: Vec<String>,
    /// Unix seconds. Stamped by the app layer — this module has no clock — and
    /// carried in the row so that undo and sync reproduce the same instant.
    pub created: i64,
    /// The last write's instant, restamped by whoever edits the row.
    pub edited: i64,
    /// The note this one comments on, when it is a reply (SPEC §四十一's 引用).
    /// `None` is an ordinary note.
    ///
    /// This is the organizer's first **self-reference**, and it deliberately keeps
    /// the rules blocks already keep: there is no foreign key, and a *dangling* id
    /// is tolerated rather than scrubbed. Deleting the note a comment answers must
    /// not delete the comment — the comment is the user's writing — so a ref that
    /// resolves to nothing simply paints as an ordinary note, exactly as a
    /// dangling `Block::page_ref` does.
    pub ref_note: Option<NoteId>,
}

/// One user's list of tasks (SPEC §四十一 「任务」's 清单). The inbox is *not*
/// one of these; see [`ListId::INBOX`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskList {
    pub id: ListId,
    pub name: String,
    /// The chip's colour dot. The closed palette the document editor already
    /// uses, so a list and a block cannot pick two different "greens".
    pub color: ColorKind,
    /// Where the list sits in the chip row. The order is the user's, so it is a
    /// column and not the id's order.
    pub ord: OrderKey,
}

/// One line of a task's checklist.
///
/// **No identity of its own beyond the id.** A subtask is never referenced from
/// anywhere, listed on its own, moved between tasks, or asked about by a query:
/// it is read and written with the task that owns it, which is why it lives
/// inside the task's row and not in a table of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtask {
    /// Opaque, and scoped to the task that holds it. The app allocates it from
    /// the same watermark as task ids because the two are never cross-referenced
    /// — one counter is one thing to seed, and a second would be bookkeeping a
    /// reader can never observe.
    pub id: u64,
    pub title: String,
    pub done: bool,
}

/// How urgent a task is. A closed set, so the picker's order is this list's
/// order and the stored spelling is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Priority {
    None,
    Low,
    Medium,
    High,
}

impl Priority {
    pub const ALL: [Priority; 4] = [
        Priority::None,
        Priority::Low,
        Priority::Medium,
        Priority::High,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Priority::None => "none",
            Priority::Low => "low",
            Priority::Medium => "medium",
            Priority::High => "high",
        }
    }

    pub fn try_from_str(s: &str) -> Option<Priority> {
        Priority::ALL.iter().copied().find(|p| p.as_str() == s)
    }

    /// What the picker shows. `as_str` is what the file says, `label` is what
    /// the user is asked to choose — the split `PropertyKind::label` keeps.
    pub fn label(self) -> &'static str {
        match self {
            Priority::None => "None",
            Priority::Low => "Low",
            Priority::Medium => "Medium",
            Priority::High => "High",
        }
    }

    /// The menu slot, which is the position in [`Self::ALL`] — the number the
    /// shell's `task-priority-set(task, slot)` callback carries, the same
    /// convention `ColorKind::slot` and `PageFont::slot` use.
    pub fn slot(self) -> i32 {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0) as i32
    }

    pub fn from_slot(slot: i32) -> Priority {
        Priority::ALL
            .get(slot.max(0) as usize)
            .copied()
            .unwrap_or(Priority::None)
    }

    /// A spelling this build does not know is *no priority* rather than a
    /// failed load: a library written by a build with a fifth level still
    /// opens, and one task reads as unprioritised — the same fold
    /// `ColorKind::from_stored`'s callers make for a colour they cannot name.
    pub fn from_stored(s: &str) -> Priority {
        Priority::try_from_str(s).unwrap_or(Priority::None)
    }
}

/// How often a task comes back once it is done (SPEC §四十一 「任务」's 重复).
///
/// v1 stores the rule and shows it; rolling a completed repeating task forward
/// is the next step, and it is deliberately not faked here by rewriting `due`
/// — that would be a derived write with no undo of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Repeat {
    None,
    Daily,
    /// Every weekday, Monday to Friday.
    Weekdays,
    Weekly,
    Monthly,
}

impl Repeat {
    pub const ALL: [Repeat; 5] = [
        Repeat::None,
        Repeat::Daily,
        Repeat::Weekdays,
        Repeat::Weekly,
        Repeat::Monthly,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Repeat::None => "none",
            Repeat::Daily => "daily",
            Repeat::Weekdays => "weekdays",
            Repeat::Weekly => "weekly",
            Repeat::Monthly => "monthly",
        }
    }

    pub fn try_from_str(s: &str) -> Option<Repeat> {
        Repeat::ALL.iter().copied().find(|r| r.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Repeat::None => "Does not repeat",
            Repeat::Daily => "Daily",
            Repeat::Weekdays => "Every weekday",
            Repeat::Weekly => "Weekly",
            Repeat::Monthly => "Monthly",
        }
    }

    /// The menu slot, as [`Priority::slot`] is.
    pub fn slot(self) -> i32 {
        Self::ALL.iter().position(|r| *r == self).unwrap_or(0) as i32
    }

    pub fn from_slot(slot: i32) -> Repeat {
        Repeat::ALL
            .get(slot.max(0) as usize)
            .copied()
            .unwrap_or(Repeat::None)
    }

    /// An unknown spelling is "does not repeat", for the reason
    /// [`Priority::from_stored`] gives: a task without a rule is a task, while
    /// a task that refuses to load is a library nobody can open.
    pub fn from_stored(s: &str) -> Repeat {
        Repeat::try_from_str(s).unwrap_or(Repeat::None)
    }
}

/// One task (SPEC §四十一 「任务」).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    /// The list it belongs to, or [`ListId::INBOX`] for the inbox. No foreign
    /// key: the inbox is a sentinel (see [`ListId::INBOX`]), and a task whose
    /// list is gone is a state the "delete a list" command *plans* — it moves
    /// them into the inbox in the same change batch rather than leaving them
    /// dangling by accident.
    pub list: ListId,
    pub title: String,
    /// The long-form notes under the title. Same v1 plain text as a note body.
    pub notes: String,
    pub priority: Priority,
    /// The deadline as an ISO calendar date (`YYYY-MM-DD`, `core::date`'s one
    /// format), or `None`. A date and not an instant: "due Friday" is what the
    /// user meant, and a stored timestamp would make the same list read
    /// differently in another zone.
    pub due: Option<String>,
    pub repeat: Repeat,
    pub done: bool,
    /// When it was ticked, or `None` while it is open. Cleared on untick, so
    /// the two fields cannot disagree about the same fact.
    pub completed_at: Option<i64>,
    pub tags: Vec<String>,
    /// The checklist, in display order. Stored in this row, stored whole.
    pub subtasks: Vec<Subtask>,
    pub created: i64,
    pub edited: i64,
    /// Where the task sits among its siblings — its list's reading order. A
    /// dense key like `pages.ord`, so reordering is one `UPDATE`.
    pub ord: OrderKey,
}

impl Task {
    /// Whether this task is in the inbox rather than in a stored list.
    pub fn in_inbox(&self) -> bool {
        self.list.is_inbox()
    }
}

/// Everything about the organizer that a startup load carries: every note,
/// every task, every list.
///
/// **Whole, unlike the database layer.** §三十九's records are windowed
/// (ADR-0067) because a database may hold 10 000 rows and a view shows thirty
/// of them; the organizer is a few hundred rows a user typed, its "views" are
/// computed in memory from the whole set (inbox / today / one list), and asking
/// SQLite for "the tasks due today" would put a second definition of *today* in
/// the store. So the catalog is loaded once and projected in the app.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrganizerCatalog {
    pub notes: Vec<Note>,
    pub tasks: Vec<Task>,
    /// The stored lists, inbox excluded (it has no row).
    pub lists: Vec<TaskList>,
}

impl OrganizerCatalog {
    pub fn note(&self, id: NoteId) -> Option<&Note> {
        self.notes.iter().find(|n| n.id == id)
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn list(&self, id: ListId) -> Option<&TaskList> {
        self.lists.iter().find(|l| l.id == id)
    }

    /// The tasks of one list, in no particular order — `list` being
    /// [`ListId::INBOX`] answers with the inbox's tasks, which is the same
    /// question asked of the same field.
    pub fn tasks_in(&self, list: ListId) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(move |t| t.list == list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: u64) -> Note {
        Note {
            id: NoteId(id),
            title: format!("Note {id}"),
            body: "one\ntwo".into(),
            pinned: id % 2 == 0,
            tags: vec!["idea".into()],
            created: 1_000,
            edited: 2_000,
            // Even ids reply to the note before them, so a test can exercise a ref
            // without a second helper.
            ref_note: if id % 2 == 0 && id > 1 { Some(NoteId(id - 1)) } else { None },
        }
    }

    fn task(id: u64, list: ListId) -> Task {
        Task {
            id: TaskId(id),
            list,
            title: format!("Task {id}"),
            notes: String::new(),
            priority: Priority::Medium,
            due: Some("2026-09-24".into()),
            repeat: Repeat::Weekly,
            done: false,
            completed_at: None,
            tags: vec!["work".into(), "urgent".into()],
            subtasks: vec![
                Subtask {
                    id: 100 + id,
                    title: "first".into(),
                    done: true,
                },
                Subtask {
                    id: 200 + id,
                    title: "second".into(),
                    done: false,
                },
            ],
            created: 10,
            edited: 20,
            ord: OrderKey::FIRST,
        }
    }

    #[test]
    fn priority_strings_round_trip_and_unknown_ones_fold() {
        for priority in Priority::ALL {
            assert_eq!(Priority::try_from_str(priority.as_str()), Some(priority));
            assert_eq!(Priority::from_stored(priority.as_str()), priority);
            assert_eq!(Priority::from_slot(priority.slot()), priority);
            assert!(!priority.label().is_empty());
        }
        assert_eq!(Priority::try_from_str("urgent"), None);
        // A level this build does not know is no priority, not a failed load.
        assert_eq!(Priority::from_stored("urgent"), Priority::None);
        assert_eq!(Priority::from_stored(""), Priority::None);
        // The picker's slots are `ALL`'s positions, so the two orders agree.
        assert_eq!(Priority::None.slot(), 0);
        assert_eq!(Priority::High.slot(), 3);
        assert_eq!(Priority::from_slot(-1), Priority::None);
        assert_eq!(Priority::from_slot(99), Priority::None);
    }

    #[test]
    fn repeat_strings_round_trip_and_unknown_ones_fold() {
        for repeat in Repeat::ALL {
            assert_eq!(Repeat::try_from_str(repeat.as_str()), Some(repeat));
            assert_eq!(Repeat::from_stored(repeat.as_str()), repeat);
            assert_eq!(Repeat::from_slot(repeat.slot()), repeat);
            assert!(!repeat.label().is_empty());
        }
        assert_eq!(Repeat::try_from_str("yearly"), None);
        assert_eq!(Repeat::from_stored("yearly"), Repeat::None);
        assert_eq!(Repeat::None.slot(), 0);
        assert_eq!(Repeat::from_slot(99), Repeat::None);
    }

    #[test]
    fn the_inbox_is_a_sentinel_and_no_stored_list() {
        assert!(ListId::INBOX.is_inbox());
        assert_eq!(ListId::INBOX.as_u64(), 0);
        assert!(!ListId(1).is_inbox());
        // The inbox is the shape a new task is born into, and it is spelled
        // through the constant rather than through a bare 0 at every call site.
        let fresh = task(1, ListId::INBOX);
        assert!(fresh.in_inbox());
        assert!(!task(2, ListId(3)).in_inbox());
    }

    #[test]
    fn ids_are_distinct_types() {
        let note = NoteId(7);
        let task = TaskId(7);
        let list = ListId(7);
        assert_eq!(note.as_u64(), task.as_u64());
        assert_ne!(format!("{note}"), format!("{task}"));
        assert_ne!(format!("{task}"), format!("{list}"));
    }

    #[test]
    fn a_catalog_answers_by_id_and_by_list() {
        let mut catalog = OrganizerCatalog::default();
        catalog.notes = vec![note(1), note(2)];
        catalog.lists = vec![TaskList {
            id: ListId(5),
            name: "Work".into(),
            color: ColorKind::Blue,
            ord: OrderKey::FIRST,
        }];
        catalog.tasks = vec![
            task(10, ListId::INBOX),
            task(11, ListId(5)),
            task(12, ListId(5)),
        ];

        assert_eq!(catalog.note(NoteId(2)).map(|n| n.pinned), Some(true));
        assert_eq!(catalog.note(NoteId(9)), None);
        assert_eq!(catalog.task(TaskId(11)).map(|t| t.title.as_str()), Some("Task 11"));
        assert_eq!(catalog.task(TaskId(99)), None);
        assert_eq!(catalog.list(ListId(5)).map(|l| l.color), Some(ColorKind::Blue));
        assert_eq!(catalog.list(ListId::INBOX), None, "the inbox has no row to find");
        assert_eq!(
            catalog.tasks_in(ListId(5)).map(|t| t.id).collect::<Vec<_>>(),
            vec![TaskId(11), TaskId(12)]
        );
        assert_eq!(
            catalog.tasks_in(ListId::INBOX).map(|t| t.id).collect::<Vec<_>>(),
            vec![TaskId(10)]
        );
        assert_eq!(catalog.tasks_in(ListId(9)).count(), 0);
    }

    /// The row a change carries is compared as a row: two tasks that differ in
    /// any stored field — including one subtask's `done` bit — are two rows, so
    /// the merge (which decides "both sides changed it" by `PartialEq`) cannot
    /// mistake one for the other.
    #[test]
    fn a_task_compares_as_a_whole_row() {
        let a = task(1, ListId::INBOX);
        assert_eq!(a, task(1, ListId::INBOX));
        let mut other = a.clone();
        other.subtasks[0].done = false;
        assert_ne!(a, other);
        let mut other = a.clone();
        other.tags.push("extra".into());
        assert_ne!(a, other);
        let mut other = a.clone();
        other.priority = Priority::High;
        assert_ne!(a, other);
        let mut other = a.clone();
        other.due = None;
        assert_ne!(a, other);
        let mut other = a.clone();
        other.list = ListId(2);
        assert_ne!(a, other);
    }
}
