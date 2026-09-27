# Changelog

## Unreleased

### A delete is reversible: 回收站 as a tombstone (ADR-0003)

- `org::Note` and `org::Task` each gain `deleted_at: Option<i64>` — the instant the
  row went into 回收站, `None` while it is live. **A tombstone on the row, not a
  second table**: trashing is an ordinary `UpdateNote` / `UpdateTask` that stamps
  the field, restoring is one that clears it, and *purging* is the `DeleteNote` /
  `DeleteTask` that already existed. The change stream, the store, the merge and the
  undo step are therefore all exactly what they were, and a delete still costs the
  one Ctrl+Z it always cost.
- Migration **29** adds the two nullable columns behind the same
  `pragma_table_info` guard, on the two tables whose rows a user deletes by hand.
  **No backfill on purpose**: a row that predates the column is a live row, which is
  what `NULL` already says, where a `DEFAULT 0` would have put a whole library in the
  bin on the first launch after the upgrade. `CURRENT_VERSION` 28 → 29.
- `OrganizerCatalog` keeps **both halves** — `notes` / `tasks` are unchanged — and
  gains `live_notes` / `trashed_notes` / `live_tasks` / `trashed_tasks`. The drawing
  paths ask the first of each pair; the write paths (`tasks_in`, and the list delete
  that moves the rows it holds) deliberately keep seeing everything, because a write
  that could not see a tombstone would step on it.
- `SNote.deleted_at` / `STask.deleted_at` on the wire, `#[serde(default)]`, and read
  back as "live" when absent — the same reading the column gives `NULL`.
- **`SNAPSHOT_VERSION` 2 → 3.** Unlike the uuid next door — where the field was
  additive and a defaulted value was honest — a tombstone a peer drops is not
  *missing*, it is **wrong**: the merge would read the row as an ordinary remote edit
  and resurrect what the user deleted. So the exact-equality gate closes again, and
  the shell family is updated together.
- Four tests: the catalog's two halves
  (`a_tombstone_splits_the_catalog_without_dropping_a_row`), the one-step trash and
  purge (`trashing_is_an_update_and_only_the_purge_is_a_delete`), the merge
  (`a_tombstone_travels_with_the_row_it_belongs_to`), and the v29 step — which also
  pins that the upgrade bins nothing.
- This is the layer the shells' own notes kept naming ("a real 回收站 needs soft
  delete in `quire-core` — a column, the store, the merge"); the two bins' screens
  are the shells' slices and land on this rev.

### A note can reference a note (ADR-0001)

- `org::Note` gains `ref_note: Option<NoteId>`: the note this one comments on,
  `None` for an ordinary note. A **dangling** id is tolerated — the read side folds
  it to "an ordinary note" — so deleting a parent never deletes its comments and
  never fails either. No foreign key; the `blocks.page_ref` rule.
- Migration **27** adds `notes.ref_note` behind the `pragma_table_info` guard, so a
  file that already has the column converges instead of erroring. `CURRENT_VERSION`
  26 → 27. Nullable, not `DEFAULT 0`: an ordinary note has no ref at all.
- `SNote.ref_note` on the sync wire, and the merge's renumber pass now builds a
  `note_map` so a comment follows the parent it answers when that parent is
  renumbered. A ref to a note that did not survive is passed through unchanged.
- No new commands: `CreateNote` / `UpdateNote` / `DeleteNote` already carry the
  whole row, so undo and redo replay the ref for free.

### The organizer gains a 唯一 ID (ADR-0002)

- `org::Note` and `org::Task` each gain `uuid: String`: 32 lowercase hex
  characters, minted once by `core::organizer::new_uuid` and never re-minted. It is
  what the shells hand to an AI and what a batch instruction addresses a row by —
  `id` cannot serve, because it is a per-device `max + 1` watermark that a sync
  *renumbers*.
- `new_uuid` is dependency-free: `RandomState`'s OS-seeded hasher over a running
  nonce and the wall clock, two of them so the value is 128 bits. A clock alone is
  exactly the collision the column exists to avoid — two devices that sync mint
  rows in the same millisecond routinely.
- Migration **28** adds `notes.uuid` and `tasks.uuid` behind the same
  `pragma_table_info` guard, and **backfills** every row that predates it with
  `lower(hex(randomblob(16)))` — SQLite's own CSPRNG, so no row is left without an
  identity and this crate needs no RNG of its own. `CURRENT_VERSION` 27 → 28.
- `SNote.uuid` / `STask.uuid` on the wire, `#[serde(default)]` and **no snapshot
  version bump**: the field is additive, so a peer at the previous rev goes on
  syncing and sends no `uuid` (which reads as blank) instead of being locked out.
- `merge` collapses the identity **before** the three-way decision, by the row's id:
  a blank adopts the other side's known value; two real values for one row are two
  independent backfills and the smaller wins, so the two peers agree on the
  survivor; a row neither side named is minted on arrival. Without this pass a uuid
  only one side carried would read as a whole-row edit — a logged conflict, or with
  no shadow a renumber, which duplicates the row instead of merging it.
- No new commands: every organizer command carries the whole row, so the identity
  rides an edit, an undo and a redo for free.
