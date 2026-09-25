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
