// The organizer's SQL (SPEC §四十一): `notes`, `task_lists`, `tasks`, and the
// whole-catalog read the app starts from.
//
// A module of its own rather than arms inside `repository`, for the reason
// `database_store` is one: the store's job is to say *what statement a change
// means*, and the repository's is to be the exhaustive table of contents over
// `Change`. Splitting them means this slice adds a file instead of editing one.
//
// Two things are worth knowing before reading the statements:
//
// * **The catalog is read whole.** Unlike §三十九's records (windowed, ADR-0067),
//   the organizer is a few hundred rows a user typed and every "view" of it
//   (inbox / today / one list) is a projection of the whole set. So there is one
//   read per table and no `LIMIT` anywhere — the one place this module and
//   `database_store` deliberately differ.
// * **Two columns hold JSON**, `notes.tags` and `tasks.tags` / `tasks.subtasks`,
//   and this file is the only place their shape is known. Core stores
//   `Vec<String>` and `Vec<Subtask>`; SQLite stores text; the conversions below
//   are the seam, and they are local to this module so that no other layer has
//   to know what the bytes look like.

use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};

use crate::core::organizer::{
    ListId, Note, NoteId, OrganizerCatalog, Priority, Repeat, Subtask, Task, TaskId, TaskList,
};
use crate::core::types::{ColorKind, OrderKey};
use crate::core::StorageError;

use super::database::{ord_from_db, ord_to_db};
use super::repository::require_hit;
use super::SqliteRepository;

fn sql(e: rusqlite::Error) -> StorageError {
    StorageError::Sql(e.to_string())
}

fn id(value: u64) -> i64 {
    value as i64
}

/// A `bool` as the `0` / `1` SQLite's `INTEGER` column holds. A local copy of
/// `repository`'s, for the reason `core::organizer` copies `id_newtype!`: the
/// original is private to its module and the two sides must agree.
fn flag(value: bool) -> i64 {
    i64::from(value)
}

fn unflag(value: i64) -> bool {
    value != 0
}

/// One subtask as the JSON inside `tasks.subtasks` spells it. A wire struct of
/// this module's own, like `services::sync::model`'s `S*` rows and for the same
/// reason: the core types carry no serde derives, and the shape of the stored
/// bytes is a storage decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredSubtask {
    id: u64,
    title: String,
    done: bool,
}

impl From<&Subtask> for StoredSubtask {
    fn from(s: &Subtask) -> Self {
        StoredSubtask {
            id: s.id,
            title: s.title.clone(),
            done: s.done,
        }
    }
}

impl StoredSubtask {
    fn to_core(&self) -> Subtask {
        Subtask {
            id: self.id,
            title: self.title.clone(),
            done: self.done,
        }
    }
}

/// The tag list as the one text column holds it: a JSON array of strings. The
/// fallback is the empty list, because a `Vec<String>` has no way to fail
/// serialization and "no tags" is the honest answer if one ever did.
fn tags_to_json(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".into())
}

/// The inverse. **Anything unreadable is no tags** rather than a failed load:
/// `''` is the column default, and a hand-edited or half-written value must cost
/// a note its labels, not cost the user a library — the same fold the load path
/// makes for a colour string it cannot name.
fn tags_from_json(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}

/// The checklist as the one text column holds it: a JSON array of
/// `{id,title,done}`. Same fallback and same tolerance as the tags above — a
/// checklist nobody can parse is an empty checklist, and the task itself still
/// opens.
fn subtasks_to_json(subtasks: &[Subtask]) -> String {
    let wire: Vec<StoredSubtask> = subtasks.iter().map(Into::into).collect();
    serde_json::to_string(&wire).unwrap_or_else(|_| "[]".into())
}

fn subtasks_from_json(text: &str) -> Vec<Subtask> {
    let wire: Vec<StoredSubtask> = serde_json::from_str(text).unwrap_or_default();
    wire.iter().map(StoredSubtask::to_core).collect()
}

impl SqliteRepository {
    /// Every note, every task and every stored list, in one read per table
    /// (SPEC §四十一's area's startup load).
    ///
    /// No window and no `COUNT(*)`: §三十九's records are read a viewport at a
    /// time because a database may hold 10 000 of them, while the organizer is
    /// what a person typed and its smart views ("inbox", "today") are derived in
    /// memory — asking SQLite for "the tasks due today" would put a second
    /// definition of *today* in the store, next to the one the app draws with.
    ///
    /// The order is the one that makes the reads cheap and the result stable:
    /// `notes` by id, `task_lists` by `ord`, `tasks` by `(list, ord)` — the last
    /// one being exactly the index v26 builds. Nothing depends on it (the app
    /// re-sorts every projection), which is why it can be the index's order.
    pub fn load_organizer(&self) -> Result<OrganizerCatalog, StorageError> {
        let conn = self.database().conn();
        Ok(OrganizerCatalog {
            notes: read_notes(&conn)?,
            lists: read_task_lists(&conn)?,
            tasks: read_tasks(&conn)?,
        })
    }
}

fn read_notes(conn: &Connection) -> Result<Vec<Note>, StorageError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, title, body, pinned, tags, created, edited, ref_note
               FROM notes ORDER BY id",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ))
        })
        .map_err(sql)?;
    let mut out = Vec::new();
    for row in rows {
        let (note, title, body, pinned, tags, created, edited, ref_note) = row.map_err(sql)?;
        out.push(Note {
            id: NoteId(note as u64),
            title,
            body,
            pinned: unflag(pinned),
            tags: tags_from_json(&tags),
            created,
            edited,
            // Carried through exactly as stored, **including an id that names no
            // note**: a dangling ref is a real state (the note it answered was
            // deleted), and the store is not the layer that decides what it means.
            ref_note: ref_note.map(|r| NoteId(r.max(0) as u64)),
        });
    }
    Ok(out)
}

fn read_task_lists(conn: &Connection) -> Result<Vec<TaskList>, StorageError> {
    let mut stmt = conn
        .prepare("SELECT id, name, color, ord FROM task_lists ORDER BY ord, id")
        .map_err(sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .map_err(sql)?;
    let mut out = Vec::new();
    for row in rows {
        let (list, name, color, ord) = row.map_err(sql)?;
        out.push(TaskList {
            id: ListId(list as u64),
            name,
            // Like every other colour in the store: a spelling this build does
            // not know is the theme default, not a load that fails.
            color: ColorKind::try_from_str(&color).unwrap_or(ColorKind::Default),
            ord: OrderKey(ord_from_db(ord)),
        });
    }
    Ok(out)
}

fn read_tasks(conn: &Connection) -> Result<Vec<Task>, StorageError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, list, title, notes, priority, due, repeat, done, completed_at, tags,
                    subtasks, created, edited, ord
               FROM tasks
              ORDER BY list, ord, id",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, i64>(11)?,
                r.get::<_, i64>(12)?,
                r.get::<_, i64>(13)?,
            ))
        })
        .map_err(sql)?;
    let mut out = Vec::new();
    for row in rows {
        let (
            task,
            list,
            title,
            notes,
            priority,
            due,
            repeat,
            done,
            completed_at,
            tags,
            subtasks,
            created,
            edited,
            ord,
        ) = row.map_err(sql)?;
        out.push(Task {
            id: TaskId(task as u64),
            // Row 0 is the inbox sentinel, which is not a row and needs no
            // lookup to mean what it means (`ListId::INBOX`).
            list: ListId(list.max(0) as u64),
            title,
            notes,
            priority: Priority::from_stored(&priority),
            // An absent or blank deadline is no deadline. The string itself is
            // handed back exactly as stored: validating it here would let the
            // load *decide* that a hand-edited date is no date, and the app's
            // paint path already has to tolerate a value it cannot lay out.
            due: due.filter(|d| !d.is_empty()),
            repeat: Repeat::from_stored(&repeat),
            done: unflag(done),
            completed_at,
            tags: tags_from_json(&tags),
            subtasks: subtasks_from_json(&subtasks),
            created,
            edited,
            ord: OrderKey(ord_from_db(ord)),
        });
    }
    Ok(out)
}

// ─── writes ─────────────────────────────────────────────────────────────────
//
// One function per change arm, all inside the caller's transaction, named after
// the change so `repository::apply_one` reads as a table of contents — the rule
// `database_store`'s write block states.
//
// Every `set_*` writes **every** column of the row and not just the edited one:
// the change carries the whole row (`Change::NoteUpdated(Note)`), the merge
// compares whole rows, and a partial `UPDATE` would let the file hold a row that
// is neither of the two the change described. Every one of them also
// `require_hit`s: this catalog is in the app's memory in full, so a row that is
// not in the file is a desynchronized session and not a race.

pub(crate) fn insert_note(tx: &Transaction, note: &Note) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO notes (id, title, body, pinned, tags, created, edited, ref_note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id(note.id.as_u64()),
            note.title,
            note.body,
            flag(note.pinned),
            tags_to_json(&note.tags),
            note.created,
            note.edited,
            note.ref_note.map(|r| id(r.as_u64())),
        ],
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn set_note(tx: &Transaction, note: &Note) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE notes SET title = ?2, body = ?3, pinned = ?4, tags = ?5, created = ?6,
                              edited = ?7, ref_note = ?8
              WHERE id = ?1",
            params![
                id(note.id.as_u64()),
                note.title,
                note.body,
                flag(note.pinned),
                tags_to_json(&note.tags),
                note.created,
                note.edited,
                note.ref_note.map(|r| id(r.as_u64())),
            ],
        )
        .map_err(sql)?;
    require_hit(n, "NoteUpdated", note.id.as_u64())
}

pub(crate) fn delete_note(tx: &Transaction, note: NoteId) -> Result<(), StorageError> {
    let n = tx
        .execute("DELETE FROM notes WHERE id = ?1", params![id(note.as_u64())])
        .map_err(sql)?;
    require_hit(n, "NoteDeleted", note.as_u64())
}

pub(crate) fn insert_task_list(tx: &Transaction, list: &TaskList) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO task_lists (id, name, color, ord) VALUES (?1, ?2, ?3, ?4)",
        params![
            id(list.id.as_u64()),
            list.name,
            list.color.as_str(),
            ord_to_db(list.ord.0),
        ],
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn set_task_list(tx: &Transaction, list: &TaskList) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE task_lists SET name = ?2, color = ?3, ord = ?4 WHERE id = ?1",
            params![
                id(list.id.as_u64()),
                list.name,
                list.color.as_str(),
                ord_to_db(list.ord.0),
            ],
        )
        .map_err(sql)?;
    require_hit(n, "TaskListUpdated", list.id.as_u64())
}

pub(crate) fn delete_task_list(tx: &Transaction, list: ListId) -> Result<(), StorageError> {
    // The tasks that were in it are **not** cascaded: "delete my list" must not
    // be a way to lose tasks, and the command that deletes one moves them into
    // the inbox with a `TaskUpdated` each, in the same batch. There is no
    // foreign key here to cascade from in the first place — `tasks.list = 0` is
    // the inbox sentinel (`ListId::INBOX`), and a constraint that had to
    // special-case it would be a constraint nobody could read.
    let n = tx
        .execute(
            "DELETE FROM task_lists WHERE id = ?1",
            params![id(list.as_u64())],
        )
        .map_err(sql)?;
    require_hit(n, "TaskListDeleted", list.as_u64())
}

pub(crate) fn insert_task(tx: &Transaction, task: &Task) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO tasks (id, list, title, notes, priority, due, repeat, done, completed_at,
                            tags, subtasks, created, edited, ord)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        task_params(task),
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn set_task(tx: &Transaction, task: &Task) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE tasks SET list = ?2, title = ?3, notes = ?4, priority = ?5, due = ?6,
                              repeat = ?7, done = ?8, completed_at = ?9, tags = ?10,
                              subtasks = ?11, created = ?12, edited = ?13, ord = ?14
              WHERE id = ?1",
            task_params(task),
        )
        .map_err(sql)?;
    require_hit(n, "TaskUpdated", task.id.as_u64())
}

pub(crate) fn delete_task(tx: &Transaction, task: TaskId) -> Result<(), StorageError> {
    let n = tx
        .execute("DELETE FROM tasks WHERE id = ?1", params![id(task.as_u64())])
        .map_err(sql)?;
    require_hit(n, "TaskDeleted", task.as_u64())
}

/// One task as the fourteen binds both statements take, in the order `?1` =
/// `id` … `?14` = `ord` that the two column lists above spell. One builder
/// instead of two copies so that an insert and an update can never disagree
/// about which bind is which column.
fn task_params(task: &Task) -> [Box<dyn rusqlite::ToSql>; 14] {
    [
        Box::new(id(task.id.as_u64())),
        Box::new(id(task.list.as_u64())),
        Box::new(task.title.clone()),
        Box::new(task.notes.clone()),
        Box::new(task.priority.as_str()),
        // `None` is SQL's `NULL` and the column is nullable; an empty string
        // would be a deadline that parses as nothing, which is two spellings of
        // one state.
        Box::new(task.due.clone()),
        Box::new(task.repeat.as_str()),
        Box::new(flag(task.done)),
        Box::new(task.completed_at),
        Box::new(tags_to_json(&task.tags)),
        Box::new(subtasks_to_json(&task.subtasks)),
        Box::new(task.created),
        Box::new(task.edited),
        Box::new(ord_to_db(task.ord.0)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tags column is a JSON array, and everything unreadable is *no tags*
    /// — `''` being the column default, so the tolerant read is also the one
    /// that makes a pre-tags row mean what it says.
    #[test]
    fn the_tags_column_round_trips_and_folds_what_it_cannot_read() {
        assert_eq!(tags_to_json(&[]), "[]");
        assert_eq!(tags_to_json(&["a".into(), "b".into()]), r#"["a","b"]"#);
        assert_eq!(tags_from_json(&tags_to_json(&[])), Vec::<String>::new());
        let tags = vec!["work".to_string(), "重要".to_string()];
        assert_eq!(tags_from_json(&tags_to_json(&tags)), tags);
        assert_eq!(tags_from_json(""), Vec::<String>::new());
        assert_eq!(tags_from_json("not json"), Vec::<String>::new());
        assert_eq!(tags_from_json("[1,2]"), Vec::<String>::new());
    }

    /// The checklist column is a JSON array of rows, and the round trip has to
    /// keep every field — the merge compares whole tasks, so a subtask's `done`
    /// bit going missing on the way through storage would look like a remote
    /// edit that never happened.
    #[test]
    fn the_subtasks_column_round_trips_and_folds_what_it_cannot_read() {
        let subtasks = vec![
            Subtask {
                id: 7,
                title: "buy milk".into(),
                done: true,
            },
            Subtask {
                id: 8,
                title: "重要的事".into(),
                done: false,
            },
        ];
        assert_eq!(subtasks_from_json(&subtasks_to_json(&subtasks)), subtasks);
        assert_eq!(subtasks_to_json(&[]), "[]");
        assert_eq!(subtasks_from_json(""), Vec::<Subtask>::new());
        assert_eq!(subtasks_from_json("[]"), Vec::<Subtask>::new());
        // A row whose body is not the shape this build writes costs the
        // checklist and not the task.
        assert_eq!(subtasks_from_json(r#"{"id":1}"#), Vec::<Subtask>::new());
        assert_eq!(subtasks_from_json(r#"[{"id":1,"title":"x"}]"#), Vec::<Subtask>::new());
    }

    #[test]
    fn the_bool_columns_are_zero_and_one() {
        assert_eq!(flag(true), 1);
        assert_eq!(flag(false), 0);
        assert!(unflag(1));
        assert!(!unflag(0));
        // A hand-written file with any other integer is still "set".
        assert!(unflag(2));
    }
}
