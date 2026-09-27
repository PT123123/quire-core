# Architecture Decision Records — quire-core

Format: decision → context → consequences. Newest first. Numbering is per
repository, so these numbers have nothing to do with the desktop shell's or the
Compose shell's.

This file did not exist until the first decision that was *this crate's own*
rather than the product's. `README.md` says where the chain used to live: the
desktop repository, because it described the product. A decision about the shared
model — one the shells consume rather than make — belongs here.

## ADR-0003 · A delete is a tombstone on the row, and 回收站 is a view of one catalog

Decision: `org::Note` and `org::Task` each gain `deleted_at: Option<i64>` — the
instant the row went into 回收站, `None` while it is live. Trashing is an ordinary
`UpdateNote` / `UpdateTask` that stamps it, restoring is one that clears it, and only
a *purge* reaches `DeleteNote` / `DeleteTask`. Migration **29** adds the two nullable
columns (the guarded `ALTER` shape, **no backfill**), the wire rows carry the field,
`OrganizerCatalog` grows a `live_*` / `trashed_*` pair per kind, and
`SNAPSHOT_VERSION` moves **2 → 3**.

Why a column and not a `deleted` table: the row has to stay in the file. The whole
protocol beneath this crate reads "present locally, absent remotely" as a
**deletion** — that is how the two-way merge lands removals — so a row that left the
vector the moment the user deleted it would arrive at the next sync as a row to
remove *permanently*, and the bin would empty itself the first time two devices
talked. A tombstone keeps the row where the merge can see it, and it costs nothing:
the change stream already carries whole rows, so "delete" becomes the `Update` that
was already there, with the same one-step undo.

Why an instant and not a `bool`: the bin is read as *what went in, and when*, and one
instant column answers both where a flag would need a second to say the same thing.

Why the version bump, when the uuid next door did not take one: a uuid a peer cannot
see is **absent**, and the merge's own normalisation mints one on arrival. A
tombstone a peer cannot see is **wrong** — the row reads as live at that peer, its
answer carries the row un-deleted, and the merge resolves that as an ordinary remote
edit. The first sync after a delete would therefore un-delete it, silently. That is
exactly the failure the exact-equality gate exists to refuse, so a v2 build and a v3
build refuse each other loudly and the three repositories ship together.

Consequences:

- **No new `Change` variants and no new commands.** `NoteUpdated` / `TaskUpdated`
  carry the whole row, so a trash and a restore are the rows they always were and the
  undo stack keeps its one-step rule. `organizer_store` writes one more column on each
  of two tables; `repository::apply_one` is untouched.
- The **shells** filter their projections on the tombstone. That is why `live_notes` /
  `live_tasks` live here rather than in an app layer: "which half of one collection"
  is a question about the model that owns the collection.
- `tasks_in` deliberately keeps **both** halves — its callers are writes, and a write
  that could not see a tombstone would step on it.
- **Still not here**: an automatic empty-the-bin policy, a per-device bin setting
  (both are shells' business), and any *history* — a tombstone says when a row went
  in, not what it used to be. A restore brings back the row's current content, which
  is all a row ever holds.

## ADR-0002 · A note and a task each carry a 唯一 ID, and a merge cannot disagree about it

Decision: `org::Note` and `org::Task` each gain `uuid: String` — 32 lowercase hex
characters (`core::organizer::new_uuid`), minted once when the row is created and
never re-minted. It is stored in a `uuid` column on each table (migration **28**, an
`ALTER` behind the same `pragma_table_info` guard `notes.ref_note` introduced) and
travels as `SNote.uuid` / `STask.uuid`. `merge` **normalises it before the three-way
decision**, by the row's own id: a blank adopts the other side's value; two real
values for one row are two independent backfills and the smaller wins; a row neither
side named is minted on arrival.

Why: the shells hand a note or a task to an AI and take a batch of instructions back,
and each instruction has to name exactly one row. The only identity this crate had was
`id`, and `id` is deliberately **not** stable across devices: it is a per-device
`max + 1` watermark, and `merge` *renumbers* a colliding id rather than dropping a row.
An instruction that named a row by `id` would therefore name a different row — or none
— on another device, and the failure would be silent and destructive (an edit or a
delete landing on the wrong note). A uuid is the smallest value that cannot do that.

Why not a millisecond stamp: the two ends of a sync are typically one person's phone
and laptop, minting rows in the same millisecond routinely. A clock-derived id is not
"less unique", it is *the* collision this column exists to prevent, and it would fail
exactly when a user first syncs two devices that already hold content. `RandomState`'s
OS-seeded hasher — two of them, over a running nonce and the wall clock — is 128 bits,
per-process unpredictable, and free of any RNG dependency; it is the same call
`services::sync` already makes for its device id.

Why an additive wire field instead of a snapshot version bump: the field is
`#[serde(default)]`, so a peer at the previous rev parses our snapshot (ignoring the
uuid) and its own parses here with the uuid blank. A bump would lock that peer out —
the cost accepted for a *new collection*, and not one an added attribute should pay.

Consequences:

- **The identity of a row is now an attribute, so the merge has to treat it as one.**
  Left in the ordinary field-by-field comparison, a uuid that only one side carried
  would read as "both edited the row" (a logged conflict) or, with no shadow, as two
  devices that minted one id for different rows — a *renumber*, which duplicates the
  row. The normalisation pass is what stops the identity from changing what the
  three-way merge decides about the row itself.
- **Two devices can backfill the same row differently**, because migration 28's
  `randomblob` runs locally on each. "The smaller value wins" is what collapses that to
  one answer both peers agree on, instead of each preferring its own and trading the
  row on every sync. It is a rule about a value no user can observe, so it costs
  nothing to be arbitrary — only to be *deterministic*.
- `new_uuid` lives in `core`, so neither shell owns an RNG and both mint the same
  shape. A shell that creates a row must mint one; nothing else changes, because every
  organizer command already carries the whole row, so an edit, an undo and a redo
  replay the identity for free.
- A peer that never sends one is still addressable: a shell should name a row by its
  `uuid` when that is non-empty and by `local:<id>` when it is not, and an instruction
  that carries an `id` is accepted as well as one that carries a `uuid`.
- Cross-shell follow-ups, recorded here so they are not lost: the desktop shell's
  `docs/SPEC.md` §四十一 and its ADR chain live in the desktop repository and should
  gain the 唯一 ID paragraph and a decision of their own when that shell bumps its
  pinned `rev`.

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
