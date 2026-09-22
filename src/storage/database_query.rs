// The view rules' SQL half (SPEC §三十九 「操作」, the red line
// 「filter / sort 在 SQL 侧完成，不在 UI 侧过滤」; ADR-0064's document,
// ADR-0076's compilation).
//
// D3's table drew the database's own listing order. D4 gives a view *rules* —
// a filter tree, a sort list, a group — and every one of them is compiled into
// the ONE statement the windowed read already runs. That is the whole red line
// made structural rather than promised:
//
//     the view's rules (core::database_view, parsed from ADR-0064's document)
//         │
//         ▼   this module: text + binds, and nothing else
//     one statement:  SELECT … FROM … WHERE db ∧ filter ORDER BY sorts … LIMIT/OFFSET
//         │
//         ▼
//     the window (`core::database::window`) slices THAT order
//
// A window is a slice of an order, and a filter changes both the order's
// *membership* and the count the window is computed from — so compiling the
// rules anywhere but inside the statement would leave Rust holding rows only to
// throw them away, which is the exact defect the red line names. No function
// here runs a query; every one returns SQL text and the binds its
// placeholders take, and `storage::database_store` is the only caller that
// executes anything.
//
// Three contracts worth restating because they span modules:
//
// 1. **The count is SQL's, before the window.** `count_query` answers "how
//    many rows does this view show" with one `COUNT(*)` over the same `FROM`
//    and `WHERE` the row read runs; the window arithmetic is computed *from
//    that number* (`core::database::window`), and only then are rows asked
//    for. The grouped path keeps the same contract with different plumbing:
//    `group_query` is one `GROUP BY` over an option-bounded column (a
//    handful of rows), the entry count is Σ(count + 1), and a group's rows
//    are fetched with their own `LIMIT`/`OFFSET` *inside the group* — so a
//    10 000-row group realizes the same 31 rows it would ungrouped, and its
//    header costs one entry, not one row.
// 2. **The tie-break is the database's own listing order.** Every `ORDER BY`
//    this module emits ends in `r.ord, r.id`, so equal rows always come back
//    in one order and a re-read of the same window is the same rows — the
//    stability ADR-0070 asked for, now across however many terms the view
//    names.
// 3. **An empty or half-written filter constrains nothing.** A clause whose
//    value was never filled in (ADR-0076's `FilterValue::Missing`) compiles to
//    a literal `1`, so "add a rule" cannot hide rows before the user has said
//    what the rule is. Hiding rows is what an *unreadable* filter does — and
//    that case never reaches this module: the parser (core) has already
//    dropped the whole tree and given the view a visible note.

use rusqlite::types::Value;

use crate::core::database::{
    Property, PropertyId, PropertyKind, RowRequest, RowWindow, SortColumn,
};
use crate::core::database_view::{FilterClause, FilterNode, FilterOp, FilterValue, GroupKey, GroupSpec};

/// A statement under construction: its `SELECT`, its `FROM`, the binds its
/// placeholders take *in the order they were emitted*, and the value joins it
/// has made for properties that are neither the title column nor a visible one.
///
/// The counter is why this is a struct at all. A sort term, a filter clause and
/// a group each arrive with a bind of a different *type* (an id, a number, a
/// piece of text), and hand-counted `?N`s are how a window read would silently
/// bind a property id into a `LIMIT`. Placeholders are numbered by their
/// position in `binds`, so the text may name them in any order — the number is
/// the contract, not the position in the string.
pub struct Sql {
    pub select: String,
    pub from: String,
    pub binds: Vec<Value>,
    /// The value joins made on demand for hidden properties — a sort key or a
    /// filter column the view does not show — as `(property id, alias)` in the
    /// order they were first asked for. One join per property, however many
    /// clauses and terms mention it.
    hidden: Vec<(u64, String)>,
}

/// One visible column's three read slots — the expressions its cell's value
/// comes from. A derived kind (`created time` / `last edited time`, ADR-0068)
/// reads the record's own columns and joins nothing; its `num`/`flag` slots are
/// the literal `NULL`, so every downstream reader keeps one shape.
pub struct ColumnSlots {
    pub property: PropertyId,
    pub text: String,
    pub num: String,
    pub flag: String,
}

impl Sql {
    /// A builder with its `SELECT` and the record table plus the page join
    /// (ADR-0063) already in place. Everything else is appended as the rules
    /// ask for it.
    pub fn new(select: &str, from: &str) -> Self {
        Sql {
            select: select.to_string(),
            from: from.to_string(),
            binds: Vec::new(),
            hidden: Vec::new(),
        }
    }

    fn push(&mut self, value: Value) -> String {
        self.binds.push(value);
        format!("?{}", self.binds.len())
    }

    /// An integer bind — ids, flags-as-0/1, the window's `LIMIT`/`OFFSET`.
    pub fn bind_int(&mut self, value: i64) -> String {
        self.push(Value::Integer(value))
    }

    /// An id bind. `u64` on the core side, `i64` in SQLite, the same rule
    /// every store statement follows.
    pub fn bind_id(&mut self, value: u64) -> String {
        self.push(Value::Integer(value as i64))
    }

    pub fn bind_real(&mut self, value: f64) -> String {
        self.push(Value::Real(value))
    }

    pub fn bind_text(&mut self, value: &str) -> String {
        self.push(Value::Text(value.to_string()))
    }

    /// The alias of a **hidden** property's value join — a sort key or a filter
    /// column the view does not show — made on first ask, reused after. `s0`,
    /// `s1`, … in first-ask order, so a plan printed for debugging names one
    /// join per hidden column the rules actually use.
    pub fn hidden_join(&mut self, property: PropertyId) -> String {
        if let Some((_, alias)) = self.hidden.iter().find(|(p, _)| *p == property.as_u64()) {
            return alias.clone();
        }
        let alias = format!("s{}", self.hidden.len());
        let bind = self.bind_id(property.as_u64());
        self.from.push_str(&format!(
            " LEFT JOIN db_values {alias} ON {alias}.record = r.id AND {alias}.property = {bind}"
        ));
        self.hidden.push((property.as_u64(), alias.clone()));
        alias
    }
}

/// The `db_records` column a derived kind projects (ADR-0068), or `None` for
/// every other kind.
fn derived_column(kind: PropertyKind) -> Option<&'static str> {
    match kind {
        PropertyKind::CreatedTime => Some("created"),
        PropertyKind::LastEditedTime => Some("edited"),
        _ => None,
    }
}

/// The `FROM` clause every statement about one view's rows shares: the record
/// table and its page join are the caller's; the title's value join is always
/// made (the title's one home is the `COALESCE`, whether or not the view shows
/// the column, ADR-0063); one join per *visible* column follows. The slots come
/// back in column order — the same order the row read walks to paint cells.
///
/// The count and group queries pass an empty `columns`: they need no cell
/// slots, and a filter on a visible column then takes a hidden join of its own
/// rather than the visible one — one extra index probe on a `COUNT`, and the
/// row read keeps its aliases exactly as D2's plan pinned them.
pub fn build_from(sql: &mut Sql, title: PropertyId, columns: &[Property]) -> Vec<ColumnSlots> {
    let title_bind = sql.bind_id(title.as_u64());
    sql.from.push_str(&format!(
        " LEFT JOIN db_values t ON t.record = r.id AND t.property = {title_bind}"
    ));
    let mut slots: Vec<ColumnSlots> = Vec::with_capacity(columns.len());
    for column in columns {
        match derived_column(column.kind) {
            Some(stamp) => slots.push(ColumnSlots {
                property: column.id,
                text: format!("r.{stamp}"),
                num: "NULL".to_string(),
                flag: "NULL".to_string(),
            }),
            None => {
                let alias = format!("v{}", slots.len());
                let bind = sql.bind_id(column.id.as_u64());
                sql.from.push_str(&format!(
                    " LEFT JOIN db_values {alias} ON {alias}.record = r.id \
                     AND {alias}.property = {bind}"
                ));
                let text = if column.id == title {
                    format!("COALESCE(p.title, {alias}.text)")
                } else {
                    format!("{alias}.text")
                };
                slots.push(ColumnSlots {
                    property: column.id,
                    text,
                    num: format!("{alias}.num"),
                    flag: format!("{alias}.flag"),
                });
            }
        }
    }
    slots
}

/// The SQL expression one property is compared in — **the decision ADR-0070
/// tables, now shared by the sort and the filter.** A number compares in its
/// `REAL` column (`2` before `10`), a date in its fixed-width text (bytes are
/// time order because the stored shape is, ADR-0062), a checkbox in its flag,
/// the two stamps in the record's own columns, and everything else text-shaped
/// in the `text` column — the title through the `COALESCE`, because a
/// page-backed row's title is `pages.title` and its value row may not exist.
///
/// The expression reuses an existing join when the property has one (the
/// title's `t`, a visible column's `v{n}`) and makes a hidden one (`s{n}`) when
/// it does not — one join per property, however many rules mention it.
pub fn column_expr(
    sql: &mut Sql,
    property: PropertyId,
    column: SortColumn,
    title: PropertyId,
    slots: &[ColumnSlots],
) -> String {
    match column {
        SortColumn::Created => return "r.created".to_string(),
        SortColumn::Edited => return "r.edited".to_string(),
        SortColumn::Text | SortColumn::Number | SortColumn::Flag => {}
    }
    let slot = match column {
        SortColumn::Text => 0,
        SortColumn::Number => 1,
        SortColumn::Flag => 2,
        // Handled above: the two stamps take no join at all.
        _ => unreachable!("derived stamps are returned before the slot choice"),
    };
    if property == title {
        return match slot {
            0 => "COALESCE(p.title, t.text)".to_string(),
            1 => "t.num".to_string(),
            _ => "t.flag".to_string(),
        };
    }
    if let Some(found) = slots.iter().find(|s| s.property == property) {
        return match slot {
            0 => found.text.clone(),
            1 => found.num.clone(),
            _ => found.flag.clone(),
        };
    }
    let alias = sql.hidden_join(property);
    match slot {
        0 => format!("{alias}.text"),
        1 => format!("{alias}.num"),
        _ => format!("{alias}.flag"),
    }
}

/// A statement and the binds that go with it, built together so the two cannot
/// disagree about what `?4` is.
pub struct RowQuery {
    pub sql: String,
    pub binds: Vec<Value>,
}

/// The statement one row read runs: the record's id, the title's `COALESCE`,
/// one (text, num, flag) triple per visible column, the view's `WHERE`
/// (database, then the filter tree), the view's `ORDER BY` (the sort terms,
/// then the listing-order tie-break), and — when a window was asked for — the
/// window as `LIMIT`/`OFFSET`. Without the window this is the Markdown
/// export's control read (ADR-0065): the same statement minus two clauses, so
/// the difference between the two reads *is* the window and nothing else.
pub fn row_query_plan(req: &RowRequest<'_>, window: Option<RowWindow>) -> RowQuery {
    let mut sql = Sql::new(
        "SELECT r.id, COALESCE(p.title, t.text)",
        " FROM db_records r LEFT JOIN pages p ON p.id = r.page",
    );
    let slots = build_from(&mut sql, req.title, req.columns);
    for slot in &slots {
        sql.select
            .push_str(&format!(", {}, {}, {}", slot.text, slot.num, slot.flag));
    }
    let order = order_clause(&mut sql, req, &slots);
    let wh = where_clause(&mut sql, req, &slots);
    let mut text = format!("{}{}{}{}", sql.select, sql.from, wh, order);
    if let Some(window) = window {
        let limit = sql.bind_int(window.len() as i64);
        let offset = sql.bind_int(window.start as i64);
        text.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
    }
    RowQuery {
        sql: text,
        binds: sql.binds,
    }
}

/// One group's slice of the window: the group predicate joins the `WHERE`, and
/// the `LIMIT`/`OFFSET` are the slice *inside the group* — `skip` of the
/// group's own leading rows and `len` after them, in the view's order. This is
/// what keeps a grouped view's rows as virtualized as an ungrouped one's: a
/// group with 10 000 rows is fetched through the same window arithmetic as the
/// whole table would be, 31 rows at a time.
pub fn row_query_in_group(
    req: &RowRequest<'_>,
    spec: &GroupSpec,
    key: &GroupKey,
    skip: usize,
    len: usize,
) -> RowQuery {
    let mut sql = Sql::new(
        "SELECT r.id, COALESCE(p.title, t.text)",
        " FROM db_records r LEFT JOIN pages p ON p.id = r.page",
    );
    let slots = build_from(&mut sql, req.title, req.columns);
    for slot in &slots {
        sql.select
            .push_str(&format!(", {}, {}, {}", slot.text, slot.num, slot.flag));
    }
    let order = order_clause(&mut sql, req, &slots);
    let db = sql.bind_id(req.db.as_u64());
    let group = group_predicate(&mut sql, spec, key, req.title, &slots);
    let filter_part = match req.filter {
        Some(tree) => format!(" AND ({})", node_predicate(&mut sql, tree, req.title, &slots)),
        None => String::new(),
    };
    let wh = format!(" WHERE r.db = {db} AND ({group}){filter_part}");
    let mut text = format!("{}{}{}{}", sql.select, sql.from, wh, order);
    let limit = sql.bind_int(len as i64);
    let offset = sql.bind_int(skip as i64);
    text.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
    RowQuery {
        sql: text,
        binds: sql.binds,
    }
}

/// The count a filtered view shows: `COUNT(*)` over the same `FROM` and
/// `WHERE` the row read runs — the number the window is computed from,
/// computed in SQL **before** any row is taken. The grouped path keeps the
/// same contract with different plumbing: the group query's counts sum to
/// this, because they are counts over the same predicate.
pub fn count_query(req: &RowRequest<'_>) -> RowQuery {
    let mut sql = Sql::new(
        "SELECT count(*)",
        " FROM db_records r LEFT JOIN pages p ON p.id = r.page",
    );
    let slots = build_from(&mut sql, req.title, &[]);
    let wh = where_clause(&mut sql, req, &slots);
    RowQuery {
        sql: format!("{}{}{}", sql.select, sql.from, wh),
        binds: sql.binds,
    }
}

/// The group query: one row per distinct value of the group column among the
/// rows the filter admits, with its count. **Small by construction** — the
/// groupable kinds are the option-bounded ones (`checkbox`, `select`,
/// `status`, ADR-0076) — which is what makes this the list of *headers* and
/// not a second copy of the table. No `ORDER BY`: the header order is the
/// schema's own option order (ADR-0061), which lives in the column's config
/// JSON where SQL cannot see it, so the caller orders these few rows in Rust.
pub fn group_query(req: &RowRequest<'_>, spec: &GroupSpec) -> RowQuery {
    let mut sql = Sql::new(
        "",
        " FROM db_records r LEFT JOIN pages p ON p.id = r.page",
    );
    let slots = build_from(&mut sql, req.title, &[]);
    let column = match spec.kind {
        PropertyKind::Checkbox => SortColumn::Flag,
        _ => SortColumn::Text,
    };
    let expr = column_expr(&mut sql, spec.property, column, req.title, &slots);
    let wh = where_clause(&mut sql, req, &slots);
    sql.select = format!("SELECT {expr}, count(*)");
    RowQuery {
        sql: format!("{}{}{} GROUP BY {expr}", sql.select, sql.from, wh),
        binds: sql.binds,
    }
}

/// The span one column takes across the rows the filter admits:
/// `min(expr), max(expr)` over the same `FROM`/`WHERE` the row read runs.
///
/// D5's timeline asks exactly one question about the whole (filtered) set —
/// "how many days wide is it" — and the answer is two aggregates, not the rows:
/// the *lanes* are windowed like any other layout's rows, but the axis under
/// them must be the range of every row the view shows, or a bar would be drawn
/// against an axis that did not include it. One scan produces both ends; no
/// row is taken.
///
/// The expression is the column the kind compares in (ADR-0070's table, the
/// same one the sort and the filter use), so a date range is byte order —
/// which is time order for the stored fixed-width shape (ADR-0062).
pub fn range_query(req: &RowRequest<'_>, property: PropertyId, kind: PropertyKind) -> RowQuery {
    let mut sql = Sql::new("", " FROM db_records r LEFT JOIN pages p ON p.id = r.page");
    let slots = build_from(&mut sql, req.title, &[]);
    let column = match kind {
        PropertyKind::Checkbox => SortColumn::Flag,
        PropertyKind::Number => SortColumn::Number,
        PropertyKind::CreatedTime => SortColumn::Created,
        PropertyKind::LastEditedTime => SortColumn::Edited,
        _ => SortColumn::Text,
    };
    let expr = column_expr(&mut sql, property, column, req.title, &slots);
    let wh = where_clause(&mut sql, req, &slots);
    sql.select = format!("SELECT min({expr}), max({expr})");
    RowQuery {
        sql: format!("{}{}{}", sql.select, sql.from, wh),
        binds: sql.binds,
    }
}

/// The `WHERE` clause: the database, then the filter tree in one parenthesized
/// group. `AND` with the tree, never string-concatenated into it — a tree that
/// compiles to `a OR b` must stay `(a OR b)` or the database predicate would
/// bind to only one side.
fn where_clause(sql: &mut Sql, req: &RowRequest<'_>, slots: &[ColumnSlots]) -> String {
    let db = sql.bind_id(req.db.as_u64());
    match req.filter {
        None => format!(" WHERE r.db = {db}"),
        Some(tree) => {
            let predicate = node_predicate(sql, tree, req.title, slots);
            format!(" WHERE r.db = {db} AND ({predicate})")
        }
    }
}

/// The predicate one filter node compiles to. Groups parenthesize themselves;
/// a lone clause does not (it is either alone in its group or one side of a
/// parenthesized one). An empty group compiles to `1` — it never reaches this
/// module from the parser, which maps it to "no filter", but a total function
/// over the tree is worth the two lines.
fn node_predicate(
    sql: &mut Sql,
    node: &FilterNode,
    title: PropertyId,
    slots: &[ColumnSlots],
) -> String {
    match node {
        FilterNode::All(children) => {
            if children.is_empty() {
                return "1".to_string();
            }
            let parts: Vec<String> = children
                .iter()
                .map(|child| node_predicate(sql, child, title, slots))
                .collect();
            if parts.len() == 1 {
                parts[0].clone()
            } else {
                format!("({})", parts.join(" AND "))
            }
        }
        FilterNode::Any(children) => {
            if children.is_empty() {
                return "1".to_string();
            }
            let parts: Vec<String> = children
                .iter()
                .map(|child| node_predicate(sql, child, title, slots))
                .collect();
            if parts.len() == 1 {
                parts[0].clone()
            } else {
                format!("({})", parts.join(" OR "))
            }
        }
        FilterNode::Not(child) => format!("NOT ({})", node_predicate(sql, child, title, slots)),
        FilterNode::Clause(clause) => clause_predicate(sql, clause, title, slots),
    }
}

/// The predicate one clause compiles to — the red line's last mile. Every arm
/// below states what it asks SQLite, because each is a decision about what the
/// words on the panel mean in SQL:
///
/// * **`contains` on text** is `INSTR(LOWER(expr), LOWER(?)) > 0`. `INSTR`
///   rather than `LIKE` so the value's own `%` and `_` mean themselves, and
///   `LOWER` because a user's "contains" is case-blind. Known boundary:
///   SQLite's `LOWER` folds ASCII only, so non-ASCII case differences do not
///   fold — the same boundary every text search in this app has.
/// * **`contains` on a list column** is one `EXISTS` probe on
///   `db_value_items`' primary-key prefix (ADR-0062's predicted shape) — the
///   item ids are rows, and "has" is an index probe, not a scan.
/// * **`eq`** compares in the column the kind stores in, with the value bound
///   in that column's type: a `REAL` bind for a number, the fixed-width text
///   for a date, the option **id** for a pick. A checkbox's `is unchecked` is
///   `(expr = 0 OR expr IS NULL)` — an untouched checkbox *is* unchecked.
/// * **`ne`** is `NOT (eq-form)`, and three-valued logic does the rest: a cell
///   with no value answers neither `is` nor `is not`, which is the reading
///   "holds a value outside this one" asks for. (The one asymmetry worth
///   naming: `is not checked` therefore excludes untouched rows while
///   `is unchecked` includes them — "is not" asserts a value exists.)
/// * **`gt`/`gte`/`lt`/`lte`** on a number bind a `REAL`; on a date they bind
///   the same fixed-width text, which is exactly why "before" and "after" work
///   without a date parser in SQL: the stored bytes sort as time (ADR-0062).
///   A cell with no value matches neither side of a comparison.
/// * **`any-of`** is an `IN` over the bound ids for a pick column, and the
///   `EXISTS` probe with `value IN (…)` for a list column.
/// * **`is (not) empty`** is the absence test per column: `NULL` for a number
///   and a flag, `NULL` or the empty string for everything text-shaped (a text
///   cell the user blanked is as empty as a row that never existed — the same
///   rule ADR-0070's blank placement uses).
/// * **A half-written clause** (no value yet) is `1`: no constraint, so an
///   unfinished rule hides nothing.
fn clause_predicate(
    sql: &mut Sql,
    clause: &FilterClause,
    title: PropertyId,
    slots: &[ColumnSlots],
) -> String {
    let column = match clause.kind {
        PropertyKind::Number => SortColumn::Number,
        PropertyKind::Checkbox => SortColumn::Flag,
        PropertyKind::CreatedTime => SortColumn::Created,
        PropertyKind::LastEditedTime => SortColumn::Edited,
        _ => SortColumn::Text,
    };
    let expr = column_expr(sql, clause.property, column, title, slots);
    match clause.op {
        FilterOp::Contains => match clause.kind {
            PropertyKind::MultiSelect | PropertyKind::Files => match &clause.value {
                FilterValue::Text(item) if !item.is_empty() => {
                    let property = sql.bind_id(clause.property.as_u64());
                    let item = sql.bind_text(item);
                    format!(
                        "EXISTS (SELECT 1 FROM db_value_items \
                         WHERE record = r.id AND property = {property} AND value = {item})"
                    )
                }
                _ => "1".to_string(),
            },
            _ => match &clause.value {
                FilterValue::Text(needle) if !needle.is_empty() => {
                    let needle = sql.bind_text(needle);
                    format!("INSTR(LOWER({expr}), LOWER({needle})) > 0")
                }
                _ => "1".to_string(),
            },
        },
        FilterOp::Eq => eq_form(sql, clause, &expr),
        // `NOT (the eq form)`: the cell must *hold* a value outside this one.
        FilterOp::Ne => match &clause.value {
            FilterValue::Missing => "1".to_string(),
            _ => format!("NOT ({})", eq_form(sql, clause, &expr)),
        },
        FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte => {
            let comparison = match clause.op {
                FilterOp::Gt => ">",
                FilterOp::Gte => ">=",
                FilterOp::Lt => "<",
                FilterOp::Lte => "<=",
                _ => unreachable!("matched above"),
            };
            match &clause.value {
                FilterValue::Number(num) => {
                    let bind = sql.bind_real(*num);
                    format!("{expr} {comparison} {bind}")
                }
                FilterValue::Text(text) if !text.is_empty() => {
                    let bind = sql.bind_text(text);
                    format!("{expr} {comparison} {bind}")
                }
                _ => "1".to_string(),
            }
        }
        FilterOp::AnyOf => match &clause.value {
            FilterValue::Any(items) if !items.is_empty() => match clause.kind {
                PropertyKind::MultiSelect | PropertyKind::Files => {
                    let property = sql.bind_id(clause.property.as_u64());
                    let binds: Vec<String> =
                        items.iter().map(|item| sql.bind_text(item)).collect();
                    format!(
                        "EXISTS (SELECT 1 FROM db_value_items \
                         WHERE record = r.id AND property = {property} \
                         AND value IN ({}))",
                        binds.join(", ")
                    )
                }
                _ => in_expr(sql, &expr, items),
            },
            _ => "1".to_string(),
        },
        FilterOp::IsEmpty => match column {
            SortColumn::Number | SortColumn::Flag => format!("{expr} IS NULL"),
            _ => format!("({expr} IS NULL OR {expr} = '')"),
        },
        FilterOp::IsNotEmpty => match column {
            SortColumn::Number | SortColumn::Flag => format!("{expr} IS NOT NULL"),
            _ => format!("({expr} IS NOT NULL AND {expr} <> '')"),
        },
    }
}

/// The `is` predicate for one clause's value, in the column the kind stores in.
/// Shared by `Eq` and — negated — by `Ne`, so the two can never drift apart.
fn eq_form(sql: &mut Sql, clause: &FilterClause, expr: &str) -> String {
    match &clause.value {
        FilterValue::Missing => "1".to_string(),
        FilterValue::Number(num) => {
            let bind = sql.bind_real(*num);
            format!("{expr} = {bind}")
        }
        FilterValue::Flag(flag) => {
            if *flag {
                format!("{expr} = 1")
            } else {
                // An untouched checkbox is unchecked: the absence of the row
                // and the row with `0` are the same state to the user.
                format!("({expr} = 0 OR {expr} IS NULL)")
            }
        }
        FilterValue::Text(text) => {
            let bind = sql.bind_text(text);
            format!("{expr} = {bind}")
        }
        // `is` with a list value is not a shape the parser produces (that is
        // `any-of`); a total match keeps it honest if a hand-edited document
        // ever carries one.
        FilterValue::Any(items) => in_expr(sql, expr, items),
    }
}

fn in_expr(sql: &mut Sql, expr: &str, items: &[String]) -> String {
    let binds: Vec<String> = items.iter().map(|item| sql.bind_text(item)).collect();
    format!("{expr} IN ({})", binds.join(", "))
}

/// The statement's `ORDER BY`: one blank-placement term plus one value term per
/// sort key, then the listing-order tie-break (`r.ord, r.id`) that keeps equal
/// rows in one order across re-reads. Empty `sorts` is the database's own
/// listing order — which is also every term's tie-break, so a view is never in
/// an order nothing defined.
///
/// Where the blanks go is said out loud, per term, because SQLite puts NULLs
/// first and "the rows with no number floated to the top" is not what anyone
/// means by "sort by number" (ADR-0062's rule, ADR-0070's statement). The
/// nullness term is always ascending, so a *descending* sort turns the values
/// around and leaves the blanks where they were; for the text-shaped kinds a
/// blanked cell (`''`) is as blank as an absent row.
pub fn order_clause(sql: &mut Sql, req: &RowRequest<'_>, slots: &[ColumnSlots]) -> String {
    if req.sorts.is_empty() {
        return " ORDER BY r.ord, r.id".to_string();
    }
    let mut terms = Vec::with_capacity(req.sorts.len());
    for sort in req.sorts {
        let expr = column_expr(sql, sort.property, sort.column, req.title, slots);
        let empty = match sort.column {
            SortColumn::Text => format!("({expr} IS NULL OR {expr} = '')"),
            SortColumn::Number | SortColumn::Flag => format!("({expr} IS NULL)"),
            SortColumn::Created => "(r.created = '')".to_string(),
            SortColumn::Edited => "(r.edited = '')".to_string(),
        };
        let direction = if sort.descending { "DESC" } else { "ASC" };
        terms.push(format!("{empty} ASC, {expr} {direction}"));
    }
    format!(" ORDER BY {}, r.ord, r.id", terms.join(", "))
}

/// The `WHERE` fragment one group's fetch adds: "this group's rows only", in
/// the same normalized keys the group list was reported in. A select group is
/// its option **id**; the empty group is the absence of a value; a checkbox's
/// unchecked group is `0` *or* no row, because an untouched checkbox is
/// unchecked.
pub fn group_predicate(
    sql: &mut Sql,
    spec: &GroupSpec,
    key: &GroupKey,
    title: PropertyId,
    slots: &[ColumnSlots],
) -> String {
    let column = match spec.kind {
        PropertyKind::Checkbox => SortColumn::Flag,
        _ => SortColumn::Text,
    };
    let expr = column_expr(sql, spec.property, column, title, slots);
    match (spec.kind, key) {
        (_, GroupKey::Empty) => format!("({expr} IS NULL OR {expr} = '')"),
        (PropertyKind::Checkbox, GroupKey::Checked) => format!("{expr} = 1"),
        (PropertyKind::Checkbox, GroupKey::Unchecked) => {
            format!("({expr} = 0 OR {expr} IS NULL)")
        }
        (_, GroupKey::Option(id)) => {
            let bind = sql.bind_text(id);
            format!("{expr} = {bind}")
        }
        // A checkbox has no `Empty` key and a select has no `Checked`/`Unchecked`
        // ones; the pairings that cannot be produced compile to no constraint
        // rather than to a wrong one.
        _ => "1".to_string(),
    }
}
