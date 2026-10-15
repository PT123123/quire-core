# Architecture Decision Records — quire-core

Format: decision → context → consequences. Newest first. Numbering is per
repository, so these numbers have nothing to do with the desktop shell's or the
Compose shell's.

This file did not exist until the first decision that was *this crate's own*
rather than the product's. `README.md` says where the chain used to live: the
desktop repository, because it described the product. A decision about the shared
model — one the shells consume rather than make — belongs here.

## ADR-0006 · `user_version` records that a step ran, not that its objects exist — so every open repairs and then verifies

Decision: `ensure_current` ends with a repair pass and then `check_schema`, and
therefore runs on **every** open, not only when a step is pending. A step is
replayed only if it is listed in `REPLAYABLE`, which names the objects it owns;
the repair then runs that step's own backfill, and whatever is still missing
makes the open **fail**.

Context: `PRAGMA user_version` is the only record of whether a migration ran.
Nothing in it says the tables that migration was supposed to create are there,
and a step whose DDL was refused — a SQLite built without FTS5, a failed open —
still advanced the version on any build that did not stop it. The file then
claims to be current and is missing half its schema.

The consequence is not a degraded feature, it is a dead library. `apply`
maintains the FTS mirror inside the *same transaction* as the rows it indexes
(ADR-0014), so one missing `search_pages` fails **every** write and rolls back
the whole batch. This was found on a real desktop library: nine hours and ten
sessions produced no persisted change of any kind — no notes, no window size,
no `sync.device-id`, which re-minted itself on every launch and filled the
phone's peer table with dead entries for one machine. The file opened normally,
passed `integrity_check`, and reported the current schema version throughout.

`check_schema` already existed for precisely this and already listed
`search_pages` / `search_blocks`, with a comment saying a file reaching the
current version without them is one whose upgrade did not run. It was called
from its own tests and from nowhere else, so the guard was real and inert.

Consequences: a repaired library indexes the rows it already held rather than
starting empty. Only `CREATE ... IF NOT EXISTS` steps are replayable, and they
must be named — an `ALTER TABLE ADD COLUMN` is not idempotent, and replaying one
against a library that already has the column is the single way this could make
things worse, so silence stays the safe default. A library that cannot be
repaired is refused at open, which a shell already handles visibly (memory-only
session plus a notice) — the point is that neither outcome may be a file which
opens and then quietly refuses everything.

## ADR-0005 · A connection waits for the writer ahead of it instead of failing

Decision: `Database::configure` sets `busy_timeout` to 5 seconds. It is the one
pragma this crate never set, and its absence was a field failure rather than a
missing feature: a transaction that met another connection's write returned
`SQLITE_BUSY` *immediately* — the default is 0, not a short wait.

Context: `Mutex<Connection>` serialises writers inside one process and does
nothing across two, and two writers is this app's normal case rather than an edge.
The persistence service queues writes and flushes them on a timer; the sync engine
flushes on its own beat; `apply_now` (the Compose shell's settings writes) goes
straight past the queue; and the desktop's tray design (its ADR-0096) deliberately
keeps an instance alive after the window closes, so a second instance can hold the
same file open.

The flush that is not allowed to fail was the one racing. An inbound push makes
the receiving side call `sync_export` → `force_flush` from the engine's pump — on
the same 250 ms beat as the ordinary writer — so the one snapshot whose failure
aborts the round is the one most likely to lose the race. It lost, and the message
reached the user blaming the *other* device: the log's device column said
「Android」 and the sentence beside it said this side could not flush, so a phone
was reported as the culprit when the writer that failed was the desktop reading
it. `synchronous=FULL`, chosen for durability, widens the window on a slow disk.

Consequences:

- A colliding writer now waits its turn, and the round completes. The 5 s is
  SQLite's own suggested default for a busy handler and is spent waiting, not
  lost; a local transaction takes milliseconds, so the budget is never the thing
  that expires.
- A genuine lock — another process holding the file open for minutes — still
  fails, and still should: that is a real condition and pretending otherwise
  would trade a clear error for a hang.
- The wording was fixed at the desktop's ADR-0128 because the fix alone would
  leave the same trap: a message about this machine's disk is read through the
  column naming the peer.
- **`open_in_memory` is deliberately left alone.** Nothing shares an in-memory
  database across connections in this codebase, so the pragma would be a line of
  no test and no behaviour.

## ADR-0004 · Notes and tasks merge by their 唯一 ID, and the revision decides the loser

Decision: `merge`'s **organizer half** stops using the id-keyed flat pass. `notes`
and `tasks` are merged by their `uuid` — the 唯一 ID ADR-0002 minted — and when two
copies of one row differ, the **revision** orders them: the newer copy stands, on
both ends, in one round. The revision is a new `rev: String` on `org::Note` and
`org::Task`, stored in a `rev` column on each table (migration **30**, backfilled
in place from `edited` / `deleted_at`), carried as `SNote.rev` / `STask.rev`,
stamped by the shells through a new `Command::stamp_rev` at the head of each
shell's organizer funnel, and shaped `"{millis:013}-{device}"` by
`core::organizer::rev`. Lists keep the id-keyed pass (a list carries no uuid), and
`SNAPSHOT_VERSION` moves **3 → 4**.

Why: two defects, both proven against the pinned rev, and both consequences of
keying on an integer that was never an identity.

- **Two rows sharing one uuid.** The old pass collapsed the uuid *by the row's
  integer id*, before the pass that exists because the same integer may be two
  different rows. Two devices that each held one note at id `1` came out of a
  merge as two rows carrying **one** uuid, so a comment's `ref_note` — and the
  clipboard's `local:<uuid>` — could point at the other device's note forever.
  Keying on the uuid makes that state unreachable instead of papering over it, and
  the renumber cascade the organizer used to need disappears with it.
- **A conflict settled twice, the other way.** Each side answered "both edited it"
  by keeping its own copy *and writing that copy into its own shadow*, so the next
  round read the winner's row as a one-sided edit and converged on an argument
  nobody won. A revision both ends can compare inverts that: the same two strings
  pick the same winner wherever they are read, so the round that finds a conflict
  is the round that settles it.

Why the revision is a field of its own and not `edited`: the area deliberately does
not move `edited` when a row is binned or restored (that is what keeps 详细信息's
修改 line honest), so a bin and the restore that undoes it would carry the same
stamp and no comparison could order them. It is deliberately **not** a counter
either: the two ends of a sync are separate devices, and only a clock (plus the
device, for a tie) orders two writes neither device saw the other make.

Why the shadow stays: it is the only thing that can tell *a row this side never
had* from *a row this side purged*. A bin travels as the row's own tombstone and
needs no help (ADR-0003); a purge leaves no row to carry anything, and a stateless
last-writer-wins merge — the reference app's, which has no purge — would read the
absence as "never had it" and resurrect the row on the next round.

Alternatives weighed and not taken:

- **Keep the id-keyed pass and only fix the uuid collapse** (mint a fresh uuid for a
  renumbered row, key the attachment skip-list on the uuid). Cheapest, and it
  repairs both proven bugs — but it leaves the organizer merged by a watermark that
  is not an identity, leaves the renumber cascade (and the `ref_note` /`list` remap
  pass) in place, and leaves the conflict rule re-deciding itself. The verified
  cost is the same class of bug one row over.
- **Go stateless, as the reference app does** — drop the shadow for notes and tasks
  and let the revision decide everything. Simpler, and it is what
  `aw-sync-rust` does — but it cannot express a purge, and Quire's 回收站 has one
  (`DeleteNote` / `DeleteTask` really delete). Keeping the shadow is what lets the
  behavior stay "uuid + revision decides" *and* "a purge stays purged".
- **Add a `uuid` to `pages`, `blocks`, `databases` too, and key everything on it.**
  The same argument applies, but those collections are hard-deleted with no
  tombstone, so they would need a purge story of their own first, and the cascade
  has to keep working for them in the meantime. Out of scope for this decision.

Consequences:

- **The version gate closes.** Not a wire-shape change — `rev` is
  `#[serde(default)]` — but a v3 peer keys its rows by `id` and *renumbers* what it
  thinks collides, so it would answer this build's rows under ids this build never
  wrote. Both shells move with this commit.
- **Migration 30 backfills every existing row** from what it already carries
  (`MAX(edited, deleted_at) * 1000`, empty device half), so no row reaches a merge
  unstamped. A legacy row therefore loses to any row written by the new build,
  which is the right answer: the new build's write is later in wall-clock time by
  construction.
- **A remote-only row is inserted under a fresh local id** rather than being
  renumbered only when it collides. The two devices' integer ids no longer have to
  agree about anything, and `merge` reports the `remote id → local id` map it used
  so a comment's `ref_note` follows its parent to wherever it landed.
- **`MergeOutcome::conflicts` now reports a *settled* row**, not an unresolved one:
  the line says which copy stands and why. The shells' 冲突 lines were already the
  place this surfaced.
- **Every shell write must stamp.** `Command::stamp_rev` covers the row a command
  creates or rewrites and deliberately leaves the `before` half alone, because the
  revert writes it back — an undo that moved the revision forward would let the
  peer take the undone value. Both shells call it at the head of the one funnel
  every organizer write already passes through (`exec_org` / `exec_org_all`,
  `Organizer::apply` / `apply_all`), so no call site has to remember.
- **Still not here**: a `uuid` key for the document's own collections (see above),
  and any per-row "who won" history beyond the log line — the losing copy is
  logged, not archived, which is the reference app's `trash` minus the UI to read
  it.

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
- **Superseded in part by ADR-0004**: the normalisation pass described above ran
  *before* the three-way decision and keyed on the row's integer `id`, which is
  exactly what let two rows that shared a uuid keep sharing it after a renumber.
  ADR-0004 removed the pass: notes and tasks are now *keyed* by the uuid, so there
  is nothing to normalise — and this ADR's own argument ("the identity of a row is
  an attribute, so the merge has to treat it as one") is what that decision acts on.

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

## ADR-0129 · 缺失不是删除：没有 shadow 就不删本地，空快照整轮拒绝

**日期**：2026-10-15
**状态**：已采纳（两处代码 + 五个测试）

### 发生了什么

两台安卓设备放在同一个网络上，一台是刚装五分钟、什么都没有的新手机。第一轮同步之后，
有内容的那台变成空的：10 个页面、62 条笔记、9 个任务归零，界面上收件箱和任务都是空的。

时间线说明这不是安装造成的：主库的时间戳是 17:13，而装 APK 是 18:29；出事前的
`bak2`（17:11）里 62 条笔记完好。**数据是被同步吃掉的，不是被装机吃掉的。**

### 根因

三方合并的前提是三方都有。没有 shadow（两个设备从未达成一致）时，
`decide()` 对「本地有、远端没有」这一格返回 `Pick::Remote`，而 `merge_flat` 里
`Pick::Remote` 遇 `r = None` 的语义是：

```rust
Pick::Remote => {
    if let Some(rv) = r { out.push((true, rv.clone())); }
    // an absent remote row is a delete
}
```

**行不并入输出 = 删除。** 于是新手机的空快照被读成了「对方删掉了一切」，
一次合并清空了整个库；合并结果又被存成shadow，
「空」从此成了两边的共识，数据再也回不来。

第二道问题是**闸门只长在push 方向**。`Job::ApplyRemote` 里的
「从未在网络上广播过 — 已拒绝」只对 `peer.kind.is_empty()`（广播发现的新设备）生效；
新手机一旦进了设备表成为已配对设备，`heard = true`，闸门就放行了。
危险的删除发生在 **pull** 方向，而那里没有设防。

### 决定

1. **`decide()`：没有 shadow 时，远端缺失一律保留本地。** 三方的第三份缺席了，
   「对方没有」有两种可能 —— 真删了，或者**对方从没见过这一行**。
   没有 shadow 无法区分这两者，而**误判成删除是不可恢复的，漏判只晚一轮**。
   真正见过这一行的 shadow（`Some(Some(row))`）才算删除的证据，
   所以**用户真的删掉的东西仍然会同步过来**，不会被这条规则复活。

2. **`sync_apply_remote`：整轮拒绝空快照。** 判据是「**空是一个整体**」而不是某一类为空：
   用户可以没有任务、可以收件箱为空，但不会五类同时为空而本机还有数据。
   本机为空**不是**拒绝的理由 —— 那是普通的首次同步，本机是 newcomer。
   拒绝会写进同步日志并说明「这一轮没有动本机的任何数据」。

3. **闸门补在 pull 侧。** 已配对设备发来的空快照和广播发现的新设备一样被拦下，
   因为它同样可能是刚装五分钟的手机。

### 为什么不做「移入回收站」

回收站（ADR-0003）已经存在，而且它是对的机制 —— 但它解决的是
**「用户明确删了」**，不是**「协议读错了缺席」**。把一次误判的合并塞进回收站，
会让每一次同步失败都往回收站里丢一份用户从没删过的东西，
用不了一周回收站就全是重复项，用户只会更不敢碰它。
**误判的删除应当被阻止，而不是被记录。**

### 后果

- `merge_flat` 少了一条删除路径，只有 shadow 见过的那条还在。
  这让「首次同步」从「以对方的库为准」变成「并集」，代价是对方真的删过的东西
  要等到下一次真正同步过一轮之后才会在本机消失 —— 这是把不可逆换成可逆。
- 五个测试钉住这件事，其中两个是这次事故本身：
  `no_shadow_never_reads_a_missing_remote_row_as_a_deletion`（core，纯函数级）、
  `an_empty_snapshot_from_a_peer_cannot_empty_this_device`（compose，端到端，
  真的建两个页面写一段文字再让空设备来一轮，然后断言它们还在）。
  另外三个守住没有修坏的地方：新手入的库仍能同步进来、
  shadow 见过的删除仍然是删除、有数据的对端不会被误判为空。
