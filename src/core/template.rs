//! The built-in template library (SPEC §三十八 "模板 · 预置若干本地模板").
//!
//! Five shapes this app can actually draw, written the way the app reads them:
//! Markdown through `import_service::parse_markdown`, which is §二十六's
//! channel and the only import path a template may use. That is what §三十八's
//! "模板的表示必须是「块序列的副本」，不得引入第二套内容格式" asks for, and it
//! has a consequence worth stating: **a preset can only hold what Markdown can
//! carry.** No colors, no callouts, no table, no column layout — those exist as
//! blocks, but §二十六's parser has no syntax for them, so a preset that asked
//! for one would silently arrive as a paragraph. Anything the user saves with
//! "Save as template" is different: it is a copy of real rows, and it keeps
//! whatever the page had.
//!
//! The Markdown is written in the order the rows appear, one line per block,
//! and an empty list marker (`- ` alone) is deliberate: a template's job is to
//! leave room, so a preset that arrives already filled in would be a page
//! somebody has to delete lines from.
//!
//! These are *content*, not a resource: no file is read, no download happens,
//! and a library that never sees another preset still has these five. They land
//! as ordinary hidden pages on a library's first start
//! (`AppState::seed_builtin_templates`), which is where the name on the menu
//! comes from — nothing here stores a name separately from the page it names.

/// One preset: the label the menu shows, and the body it copies from.
pub struct Preset {
    pub name: &'static str,
    pub markdown: &'static str,
}

/// The library, in the order the menus show it. Ordered by how soon somebody
/// needs it, not alphabetically: the meeting is the reason the feature exists.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Meeting notes",
        markdown: r#"> Who is in the room, and the one question this meeting exists to answer.

## Agenda

- Owner — item — how long it should take

## Notes

- 

## Decisions

- Decision, and what it rules out

## Actions

- [ ] Action — owner — due date
"#,
    },
    Preset {
        name: "Weekly review",
        markdown: r#"## Shipped

- What landed, and who can see it

## In progress

- [ ] Still open — what unblocks it

## Blocked

- Blocker — who can clear it

## Next week

1. The one thing that has to happen
2. 
"#,
    },
    Preset {
        name: "Project brief",
        markdown: r#"## Problem

Who has it, how often, and what it costs them. One paragraph.

## Outcome

What "done" looks like, in a sentence with a number in it.

## Not doing

- Out of scope, and why it stays out

## Milestones

- Date — gate — what has to be true by then

## Open questions

- 
"#,
    },
    Preset {
        name: "Bug report",
        markdown: r#"## What happened

- Expected:
- Actual:

## Steps to reproduce

1. 
2. 

## Environment

```json
{
  "app": "",
  "os": "",
  "build": ""
}
```

## Tracker

https://tracker.example.com/issue
"#,
    },
    Preset {
        name: "Long-form draft",
        markdown: r#"<!-- quire:toc -->

## Overview

One paragraph that could stand on its own.

## Details

### The first part

- 

### The second part

- 

## Sources

- 
"#,
    },
];
