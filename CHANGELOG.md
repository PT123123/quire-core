# Changelog

## Unreleased

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
