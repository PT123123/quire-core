# quire-core

Quire's model and store, without a window in sight.

Extracted from the `quire` desktop repository on 2026-09-23 with
[git-filter-repo](https://github.com/newren/git-filter-repo): 69 of its commits
touch these paths, from the first `Repository` trait (the app's ADR-0012) to the
last database slice (ADR-0092). The split that made this a crate is the app's
ADR-0093.

## The rule

**Nothing under `src/` may name Slint, a file dialog, a clipboard or a platform
API.** `image` and `rusqlite` are the entire dependency list. The desktop shell
and the Android shell are two consumers of one model, so a dependency in the
other direction ends the port — quietly, at the point where someone adds
`slint::` to a file and the crate stops building for a target that has no GPU
renderer.

Two seams are left visible rather than papered over:

- `storage::data_location::roaming_root()` is the only function here that reads
  an environment variable (`%APPDATA%`). Its neighbour `app_data(&Path)` already
  takes the value as a parameter, so an Android shell hands in its own documents
  directory instead of restructuring the module.
- The crate's only two `#[cfg(windows)]` blocks are a probe's
  `K32GetProcessMemoryInfo` call, and the `not(windows)` fallback beside it
  already returns `None`.

## Layout

```
src/core/       document model, commands, history, the database model
                (properties, views, filters, formula, relation, rollup)
src/storage/    SQLite: repository, migrations, query compiler, search index,
                backups, version snapshots, data placement
src/services/   persistence, Markdown import/export, search, find, attachments,
                settings, logging, and the LAN framing a sync module grows out of
src/testing.rs  self-deleting scratch directories for the tests
tests/          five integration suites: storage, markdown, search, find, backup
```

## Building and testing

```
cargo check --all-targets
cargo test --all-targets
```

That is the whole gate — no renderer, no screenshot sweep, no installer. The
ignored probes (`cargo test --lib -- --ignored --nocapture`) print numbers that
`docs/PERFORMANCE.md` in the shell repository quotes, so run them from there if
you are about to publish a figure.

## Where the design record lives

The specification, the ADR chain and the per-slice reports are still in the
desktop repository, because they describe the product and not only this crate:
`docs/SPEC.md` §三十九 and §四十, `docs/DECISIONS.md`, `docs/PERFORMANCE.md`.
Copy them across if this repository starts making its own product decisions.
