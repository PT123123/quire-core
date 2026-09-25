# Architecture Decision Records — quire-core

Format: decision → context → consequences. Newest first. Numbering is per
repository, so these numbers have nothing to do with the desktop shell's or the
Compose shell's.

This file did not exist until the first decision that was *this crate's own*
rather than the product's. `README.md` says where the chain used to live: the
desktop repository, because it described the product. A decision about the shared
model — one the shells consume rather than make — belongs here.

## ADR-0001 · A note may reference a note, with no foreign key and a tolerated dangling id

Decision: `org::Note` gains `ref_note: Option<NoteId>` — the note this one comments
on, `None` for an ordinary note. It is stored in `notes.ref_note` (added by
migration **27**, an `ALTER` behind the same `pragma_table_info` guard
`add_page_columns` keeps), travels as `SNote.ref_note`, and is remapped by
`merge` alongside the area's other pointers. There is **no foreign key**, and an id
that names nothing is **kept, not cleared**: a ref which resolves to nothing paints
as an ordinary note.

Why: SPEC §四十一's 引用 is "one note answers another", and the cheapest honest
model of that is a note holding a ref — not a second table, not a second row type.
A comment that were its own entity would need its own id space, its own wire row,
its own merge case and its own projections in every shell, for a row that is, in
every other respect, a note. Nothing in the area's reads or writes changes shape.

The two rules that look like omissions are the load-bearing part, and both are the
`Block::page_ref` precedent:

- **No foreign key, and no `ON DELETE SET NULL`.** Deleting the note a comment
  answers must not delete the comment and must not fail. The comment is the user's
  writing; losing it because the thing it replied to went away would be a delete
  nobody asked for. A rule enforced in SQLite is also a rule no undo can reach —
  the same argument `tasks.list` and `DeleteTaskList` already make.
- **A dangling id is tolerated.** The read side is where "names nothing" is folded
  to "an ordinary note", so a half-arrived merge or a deleted parent is a paint
  difference and never an error.

Why migration 27 and not an edit to v24: v24 has already run in libraries that
exist. The step is a replayable `ALTER` so that a file which already has the column
(a leftover build, a restored backup) converges instead of failing on a duplicate
name — and note that `ref_note` is deliberately **nullable rather than
`DEFAULT 0`**, because `0` would name `NoteId(0)` and "no ref" is an absence.

Consequences:

- `merge` gained a `note_map` in its renumber pass. Until this column, nothing
  referenced a note, so a renumbered note was simply a row under a fresh id; now a
  comment that was renumbered without its ref remapped would answer whatever this
  device happens to hold under the old id. The remap is
  `note_map.get(&v).unwrap_or(&v)` — the `page_ref` shape, so a ref to a note that
  did not survive is left alone and becomes correct if the parent arrives later.
- Cross-shell follow-ups, recorded here so they are not lost: the desktop shell's
  `docs/SPEC.md` §四十一 and its ADR chain live in the desktop repository and
  should gain the 引用 paragraph and a decision of their own when that shell bumps
  its pinned `rev`; its organizer projection paints a note with no ref today, which
  is the "ordinary note" answer and therefore not wrong, only incomplete.
- The shells are the only writers of the ref: `core::command` needed nothing new,
  because `CreateNote` / `UpdateNote` / `DeleteNote` carry the whole row — so undo
  replays the ref for free.
