//! The formula engine (SPEC §三十九 「需计算」, D6): a pure lexer, a recursive
//! descent parser and a tree-walking interpreter for the `formula` property
//! kind, and nothing else.
//!
//! SPEC's constraint is the reason this file exists at all: **纯词法 + 自写解释器,
//! 不引入 JS / WASM 运行时**. So there is no `eval`, no embedded scripting
//! runtime and no formula-parsing crate — a few hundred lines of Rust instead of
//! a few hundred kilobytes of somebody else's engine in a local notes app. The
//! three hard rules that follow from the SPEC sentence are all enforced here:
//!
//! 1. **Finite evaluation.** The grammar has no loops, no recursion and no
//!    user-defined functions, and every budget is a constant in this file
//!    ([`FORMULA_MAX_TOKENS`], [`FORMULA_MAX_DEPTH`], [`FORMULA_MAX_STEPS`],
//!    [`FORMULA_RESULT_MAX`]). An expression that exceeds one of them is an
//!    error value, never a hang — see [`FormulaError`].
//! 2. **No clock, no I/O, no store.** Nothing in this module reads the time, a
//!    file or a database: a formula's value is a pure function of the cells it
//!    names, which is what makes the projection's recompute contract (ADR-0083)
//!    a statement about *inputs* rather than about scheduling. `today()` and
//!    friends are deliberately absent from the function set — a formula that
//!    read the clock would paint a different value on every frame for the same
//!    document.
//! 3. **Same-row references only, by construction.** A property reference is
//!    resolved through the callback [`Program::eval`] is handed, and that
//!    callback's only inputs are a property id and a depth — **there is no
//!    record parameter, so a formula cannot name another row even in principle**.
//!    Cross-row values are `rollup` / `relation`'s job, which this build does
//!    not have (ADR-0084: those wait for §四十's reference infrastructure,
//!    Track 2).
//!
//! ## The language (the whole of it)
//!
//! ```text
//! expression  := or
//! or          := and (("or") and)*
//! and         := not (("and") not)*
//! not         := "not" not | comparison
//! comparison  := sum (("==" | "=" | "!=" | "<" | "<=" | ">" | ">=") sum)?
//! sum         := product (("+" | "-") product)*
//! product     := unary (("*" | "/") unary)*
//! unary       := "-" unary | primary
//! primary     := number | string | "true" | "false"
//!              | "[" column-name "]"            (a property reference)
//!              | function "(" arguments ")"
//!              | "(" expression ")"
//! ```
//!
//! * **numbers** are decimal digits with an optional fractional part (`2`,
//!   `0.5`); exponent notation is not part of the grammar.
//! * **strings** are double-quoted, with exactly two escapes (`\"` and `\\`).
//! * **`[Column]`** is a reference to a column of *this row*, resolved by name
//!   against the database's schema at parse time (ADR-0082): a name that names
//!   no column is a syntax error, not an empty value — a typo is visible in the
//!   editor instead of silently blanking the cell.
//! * **functions** are the minimal set [`FUNCTIONS`] lists: `if`, `length`,
//!   `round`, `abs`, `min`, `max`, `text`. Comparisons are operators, and string
//!   concatenation is `+` on two texts.
//!
//! ## The type system (four types, no implicit conversions)
//!
//! [`Val`] has four value shapes — number, text, boolean, date — plus one
//! absence ([`Val::Empty`]). Where the four may meet:
//!
//! | operation | rule |
//! |-----------|------|
//! | `+` | number + number = sum; text + text = concatenation; anything else is a type error |
//! | `- * /` | numbers only (date and text are not numbers) |
//! | `/` by zero | an **error**, never an infinity or a NaN: a formula's value must be a number a cell can show |
//! | comparisons | same type on both sides; number compares numerically, boolean by `false < true`, text and date by bytes (ADR-0062 stores a date fixed-width, so bytes are chronological); a text may be compared to a date, both are bytes, and that is the storage shape rather than a conversion |
//! | `and` / `or` / `not` / `if`'s condition | booleans only |
//! | `length` | text only (`text(x)` is how you ask a number for its text) |
//! | `round` / `abs` | numbers only |
//! | `min` / `max` | all numbers (numeric) or all text/date (bytes); mixing the two families is a type error |
//! | `text(x)` | the one **explicit** conversion: number → its `Display` form, boolean → `Yes` / `No` (ADR-0065's words), date → its stored ISO text, `Empty` → `""` |
//!
//! Where the brief says 「写明不支持隐式转换的地方」, these are the places:
//! `"Total: " + [Points]` is a **type error** (not `"Total: 31"`), `[Points] + 1`
//! on an empty cell is `Empty` (not `1`), and `length([Points])` is a type error
//! (not the digit count). Every one of them is an explicit `text(...)` away from
//! working, and every one of them is a parse-time-known or eval-time-reported
//! message rather than a wrong number.
//!
//! ## Empty is contagious
//!
//! An operand with no value makes the result `Empty`: `[Points] * 2` on a row
//! whose `Points` is blank is blank, not `0` — the same discipline ADR-0062's
//! storage already follows (「空」= 没有行，从来不是 0). The exceptions are the
//! ones that would otherwise be useless: `if` short-circuits (only the taken
//! branch is evaluated, so `if([Done], "done", "open")` works with a blank
//! branch), and `text(Empty)` is `""` — the explicit way to *ask* for a blank to
//! become text.
//!
//! ## What is deliberately not here
//!
//! * **No date arithmetic** (`dateAdd`, day differences): the function set the
//!   brief asked for is arithmetic, comparison, `if`, concatenation, `length`,
//!   `round`, `abs`, `min` / `max`, and a date is compared, not computed with.
//!   Adding it is a function, not a redesign — and it would need a calendar
//!   (D5 already hand-rolled the civil-date helpers).
//! * **No aggregate or cross-row function** (`count`, `sum` over a relation):
//!   that is `rollup`, and `rollup` needs `relation` (ADR-0084).
//! * **No list-typed operand**: a `multi-select` / `files` cell reads as
//!   [`Val::Empty`] (see [`val_of`]), because "which option is this cell" has no
//!   answer without the column's option table — a formula that needs one gets a
//!   blank rather than an id.
//! * **No `select` / `status` value either**, for the same reason: the stored
//!   value is an option *id*, and an id in a concatenation is worse than a blank.

use crate::core::database::{CellValue, PropertyId, PropertyKind};
use crate::core::database_property::json::Json;
use std::collections::BTreeSet;

// ─── the budgets (the SPEC sentence 「表达式必须有限求值」 as constants) ───────

/// The most tokens one expression may lex into. This bounds the parser's work
/// before it starts: an expression past this length is refused with a message
/// rather than parsed. A formula a human writes in a cell's editor is tens of
/// tokens; two thousand is a bound, not a limit anyone meets.
pub const FORMULA_MAX_TOKENS: usize = 2_048;

/// How deeply expressions may nest — parser recursion, and (more importantly)
/// **property-reference chains** at eval time: `[A]` may name another formula
/// column, which evaluates its own expression for the same row, and a chain is
/// legal. This is the guard that makes an *old or hand-edited* document with a
/// reference cycle paint `Error` instead of hanging: the write path refuses such
/// a cycle before it is stored (ADR-0082's save-time check), and this is the
/// belt for documents that never went through that door.
pub const FORMULA_MAX_DEPTH: u32 = 32;

/// How many expression nodes one top-level evaluation may visit. Every node
/// costs one step, so a formula's cost is bounded by its own size rather than by
/// the data — the arithmetic that lets ADR-0083 promise a *window-bounded*
/// recompute. (`10_000` steps for an expression that is at most
/// [`FORMULA_MAX_TOKENS`] long is unreachable without a chain of formula
/// columns; it exists so that "finite" is a number rather than a claim.)
pub const FORMULA_MAX_STEPS: u32 = 10_000;

/// The longest text a formula may produce, in characters. Concatenation is the
/// one operator that can grow a value without bound (`[A] + [A] + …` over a long
/// text), and a cell that paints a megabyte is a frame budget defect. Past this
/// the evaluation is an error.
pub const FORMULA_RESULT_MAX: usize = 65_536;

/// What a cell paints when its formula could not be evaluated. The *editor*
/// shows the message ([`FormulaError::message`]); a cell in a table has room for
/// one word, and "Error" is the honest one — a blank would read as "no value",
/// which is a different fact.
pub const FORMULA_ERROR_PAINT: &str = "Error";

// ─── values ─────────────────────────────────────────────────────────────────

/// What a formula computes: the four types the brief asked for, plus the one
/// absence. A date is its own shape rather than a text with a flag, so that
/// [`display`] and the type errors can name it — but it *compares* as bytes,
/// because that is what ADR-0062 stores (fixed-width ISO).
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    /// No value. Produced by a reference to a blank cell, and propagated by
    /// every operator except `if` (which short-circuits) and `text` (which is
    /// the explicit way to ask for `""`).
    Empty,
    Num(f64),
    Str(String),
    Flag(bool),
    /// A stored `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM` (ADR-0062's two shapes).
    Date(String),
}

/// The painted form of a computed value — the same words the rest of the app
/// paints values with: `Display` for a number (so `3` and not `3.0`), ADR-0065's
/// `Yes` / `No` for a boolean, the stored ISO text for a date, `""` for empty.
///
/// A formula column has no `config` of its own (no number format, no date
/// format): its result is the *value's* form, which is exactly what
/// `CellValue::display` is for the stored kinds. Kept here in the same words so
/// a formula's `62` and a number column's `62` cannot drift apart.
pub fn display(val: &Val) -> String {
    match val {
        Val::Empty => String::new(),
        Val::Num(num) => format!("{num}"),
        Val::Str(text) => text.clone(),
        Val::Flag(true) => "Yes".into(),
        Val::Flag(false) => "No".into(),
        Val::Date(iso) => iso.clone(),
    }
}

/// The [`Val`] a stored cell has, given the column's kind (ADR-0062's storage
/// shapes meet the engine's four types here — this is the *only* place the two
/// vocabularies touch).
///
/// Two deliberate folds, both documented in the module header: a list-typed cell
/// (`multi-select` / `files`) and a pick-typed one (`select` / `status`) read as
/// [`Val::Empty`], because their stored form is a set of ids and an id has no
/// meaning to an arithmetic expression. `created time` / `last edited time`
/// (ADR-0068) are dates: they are stored as the same fixed-width ISO text.
pub fn val_of(kind: PropertyKind, value: &CellValue) -> Val {
    match (kind, value) {
        (_, CellValue::Empty) => Val::Empty,
        (PropertyKind::Number, CellValue::Number(num)) => Val::Num(*num),
        (PropertyKind::Checkbox, CellValue::Flag(flag)) => Val::Flag(*flag),
        (
            PropertyKind::Date | PropertyKind::CreatedTime | PropertyKind::LastEditedTime,
            CellValue::Text(iso),
        ) => Val::Date(iso.clone()),
        (PropertyKind::Select
        | PropertyKind::Status
        | PropertyKind::MultiSelect
        | PropertyKind::Relation
        | PropertyKind::Files, _) => Val::Empty,
        (_, CellValue::Text(text)) => Val::Str(text.clone()),
        // A kind that stores one shape and holds another is a mismatch the
        // write path's own parse rules should have prevented (ADR-0069); the
        // read path's answer to "a value this column cannot hold" is a blank.
        _ => Val::Empty,
    }
}

// ─── errors ─────────────────────────────────────────────────────────────────

/// A formula that did not work, in the two flavours a user can be told apart:
/// it does not *parse* (the editor's error line, and a refusal to save) or it
/// parsed and could not *evaluate* (a cell paints `Error`, and the editor shows
/// the message in its preview).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormulaError {
    /// The text is not an expression: an unexpected character, a missing
    /// parenthesis, an unknown function, a column name that names nothing, or
    /// one of the budgets above. Carries a user-facing sentence.
    Syntax(String),
    /// The expression parsed but this row cannot produce a value: a type
    /// error, division by zero, a reference chain past [`FORMULA_MAX_DEPTH`],
    /// or a result past [`FORMULA_RESULT_MAX`].
    Eval(String),
}

impl FormulaError {
    /// The sentence a user reads — in the editor's error line, or in the
    /// notice queue when a save is refused.
    pub fn message(&self) -> &str {
        match self {
            FormulaError::Syntax(message) | FormulaError::Eval(message) => message,
        }
    }

    /// Whether this is a parse failure (as opposed to a value this row could not
    /// produce). The editor uses it to decide whether the text may be saved:
    /// a parse failure blocks the save, an eval failure does not — the same
    /// document computes fine on a row whose cells are filled in.
    pub fn is_syntax(&self) -> bool {
        matches!(self, FormulaError::Syntax(_))
    }
}

impl std::fmt::Display for FormulaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

// ─── the abstract syntax tree ───────────────────────────────────────────────

/// The comparison an operator names. Kept as a type rather than folded into
/// `Expr` so the evaluator's one comparison function has one place to hold the
/// rules (numeric for numbers, bytes for text and dates, same-type on both
/// sides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// One parsed expression node. Boxes are the recursion; the parser's depth cap
/// is what bounds how deep this can get.
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Num(f64),
    Str(String),
    Flag(bool),
    /// A column of **this row** — the only way to reach data, and the reason a
    /// formula is a per-row function (module header, rule 3).
    Ref(PropertyId),
    Neg(Box<Expr>),
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    Compare(Cmp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Length(Box<Expr>),
    Round(Box<Expr>),
    Abs(Box<Expr>),
    /// `min` / `max` are variadic in the grammar and **left-folded** into this
    /// binary node by the parser, so the evaluator has one shape to walk.
    Extreme(ExtremeOp, Box<Expr>, Box<Expr>),
    /// `text(x)` — the one explicit conversion (module header's table).
    Text(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtremeOp {
    Min,
    Max,
}

/// The function set, in one list: the name the parser accepts, and its arity as
/// `(least, most)` — `None` for the most means "unbounded" (only `min` / `max`
/// use it). `if` and the conversions are here too, so "what can I write" has one
/// answer in one place.
pub const FUNCTIONS: [(&str, usize, Option<usize>); 7] = [
    ("if", 3, Some(3)),
    ("length", 1, Some(1)),
    ("round", 1, Some(1)),
    ("abs", 1, Some(1)),
    ("min", 1, None),
    ("max", 1, None),
    ("text", 1, Some(1)),
];

// ─── the program ────────────────────────────────────────────────────────────

/// A parsed formula: the tree, the columns it names (its **direct** dependencies
/// — see [`Program::deps`]), and nothing else. Parsing happens once per formula
/// column per projection, not once per row: the tree is a pure function of the
/// expression text.
///
/// `deps` is what ADR-0083's contract is written in terms of: the set of columns
/// whose *cells* a formula reads. It is direct (a formula that names another
/// formula column lists that column, not what *it* names); the caller expands it
/// transitively when it needs "which formulas could this cell change", and the
/// save-time cycle check ([`would_cycle`]) walks the same sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    expr: Expr,
    deps: BTreeSet<u64>,
}

impl Program {
    /// Parse `source` into a program, resolving `[Column]` names through
    /// `resolve` (name → id, exact match — the caller owns the schema).
    ///
    /// Every failure is a [`FormulaError::Syntax`] carrying a sentence with the
    /// byte offset the parser stopped at, and the budgets are checked here:
    /// token count before parsing, depth during it.
    pub fn parse(
        source: &str,
        resolve: impl Fn(&str) -> Option<PropertyId>,
    ) -> Result<Program, FormulaError> {
        let tokens = lex(source)?;
        let mut parser = Parser {
            tokens,
            at: 0,
            depth: 0,
            resolve: &resolve,
            deps: BTreeSet::new(),
        };
        let expr = parser.expression()?;
        if let Some(token) = parser.peek() {
            return Err(FormulaError::Syntax(format!(
                "unexpected {} at character {}",
                token.kind.describe(),
                token.at
            )));
        }
        Ok(Program {
            expr,
            deps: parser.deps,
        })
    }

    /// The ids of the columns this formula reads **directly** (a reference to
    /// another formula column appears as that column's id — the caller decides
    /// whether to expand).
    pub fn deps(&self) -> &BTreeSet<u64> {
        &self.deps
    }

    /// Evaluate for one row. `cell` answers "what is this column's value on the
    /// row being evaluated" and is called with a **depth**: a reference to
    /// another *formula* column is the caller's cue to evaluate that column's
    /// program at `depth`, and an implementation that cannot (or that has walked
    /// past [`FORMULA_MAX_DEPTH`]) must return an [`FormulaError::Eval`] rather
    /// than recurse. **There is no record parameter** — a formula's references
    /// are resolved against the row under evaluation and nothing else, which is
    /// how "本行内引用" is a property of the API instead of a rule in a comment.
    pub fn eval(
        &self,
        cell: &mut dyn FnMut(PropertyId, u32) -> Result<Val, FormulaError>,
    ) -> Result<Val, FormulaError> {
        self.eval_at(0, cell)
    }

    /// [`Program::eval`] at an explicit depth — what the caller's `cell` callback
    /// uses to evaluate a *nested* formula column. `depth` counts how many
    /// formula columns deep the evaluation already is.
    pub fn eval_at(
        &self,
        depth: u32,
        cell: &mut dyn FnMut(PropertyId, u32) -> Result<Val, FormulaError>,
    ) -> Result<Val, FormulaError> {
        let mut steps = FORMULA_MAX_STEPS;
        let value = self.node(&self.expr, depth, &mut steps, cell)?;
        // The one post-condition on a result: a text a cell cannot paint.
        if let Val::Str(text) = &value {
            if text.chars().count() > FORMULA_RESULT_MAX {
                return Err(FormulaError::Eval(format!(
                    "the result is longer than {FORMULA_RESULT_MAX} characters"
                )));
            }
        }
        Ok(value)
    }

    /// One node. `steps` is the shared budget for the whole evaluation, so a
    /// wide expression cannot buy extra work by being wide (each node and each
    /// argument costs).
    fn node(
        &self,
        expr: &Expr,
        depth: u32,
        steps: &mut u32,
        cell: &mut dyn FnMut(PropertyId, u32) -> Result<Val, FormulaError>,
    ) -> Result<Val, FormulaError> {
        if *steps == 0 {
            return Err(FormulaError::Eval(
                "the formula is too large to evaluate".into(),
            ));
        }
        *steps -= 1;
        if depth > FORMULA_MAX_DEPTH {
            return Err(FormulaError::Eval(format!(
                "formulas are nested more than {FORMULA_MAX_DEPTH} deep"
            )));
        }
        match expr {
            Expr::Num(num) => Ok(Val::Num(*num)),
            Expr::Str(text) => Ok(Val::Str(text.clone())),
            Expr::Flag(flag) => Ok(Val::Flag(*flag)),
            Expr::Ref(property) => {
                // The callback evaluates at `depth + 1`: naming a *formula*
                // column is one level deeper, naming a stored column is a read
                // the callback answers without recursing at all.
                cell(*property, depth + 1)
            }
            Expr::Neg(inner) => match self.node(inner, depth, steps, cell)? {
                Val::Num(num) => Ok(Val::Num(-num)),
                Val::Empty => Ok(Val::Empty),
                other => Err(type_error("-", &other, "a number")),
            },
            Expr::Arith(op, left, right) => {
                let left = self.node(left, depth, steps, cell)?;
                // Short-circuit on the left operand: an empty cell makes the
                // expression empty, and the right-hand side's *errors* (a
                // division by zero, say) must not turn a blank into a visible
                // failure.
                if left == Val::Empty {
                    return Ok(Val::Empty);
                }
                let right = self.node(right, depth, steps, cell)?;
                if right == Val::Empty {
                    return Ok(Val::Empty);
                }
                arith(*op, left, right)
            }
            Expr::Compare(op, left, right) => {
                let left = self.node(left, depth, steps, cell)?;
                if left == Val::Empty {
                    return Ok(Val::Empty);
                }
                let right = self.node(right, depth, steps, cell)?;
                if right == Val::Empty {
                    return Ok(Val::Empty);
                }
                compare(*op, &left, &right)
            }
            Expr::And(left, right) => match self.node(left, depth, steps, cell)? {
                // Three-valued, Notion's rule: an empty operand does not decide
                // the conjunction, so the answer is empty too (and the right
                // side is not evaluated — an error there cannot change it).
                Val::Empty => Ok(Val::Empty),
                Val::Flag(false) => Ok(Val::Flag(false)),
                Val::Flag(true) => match self.node(right, depth, steps, cell)? {
                    Val::Flag(flag) => Ok(Val::Flag(flag)),
                    Val::Empty => Ok(Val::Empty),
                    other => Err(type_error("and", &other, "a boolean")),
                },
                other => Err(type_error("and", &other, "a boolean")),
            },
            Expr::Or(left, right) => match self.node(left, depth, steps, cell)? {
                Val::Empty => Ok(Val::Empty),
                Val::Flag(true) => Ok(Val::Flag(true)),
                Val::Flag(false) => match self.node(right, depth, steps, cell)? {
                    Val::Flag(flag) => Ok(Val::Flag(flag)),
                    Val::Empty => Ok(Val::Empty),
                    other => Err(type_error("or", &other, "a boolean")),
                },
                other => Err(type_error("or", &other, "a boolean")),
            },
            Expr::Not(inner) => match self.node(inner, depth, steps, cell)? {
                Val::Flag(flag) => Ok(Val::Flag(!flag)),
                Val::Empty => Ok(Val::Empty),
                other => Err(type_error("not", &other, "a boolean")),
            },
            Expr::If(cond, then, other) => match self.node(cond, depth, steps, cell)? {
                // The condition of an `if` on a blank cell: blank, rather than
                // guessing a branch.
                Val::Empty => Ok(Val::Empty),
                Val::Flag(true) => self.node(then, depth, steps, cell),
                Val::Flag(false) => self.node(other, depth, steps, cell),
                value => Err(type_error("if", &value, "a boolean condition")),
            },
            Expr::Length(inner) => match self.node(inner, depth, steps, cell)? {
                Val::Str(text) => Ok(Val::Num(text.chars().count() as f64)),
                Val::Empty => Ok(Val::Empty),
                other => Err(type_error("length", &other, "text")),
            },
            Expr::Round(inner) => match self.node(inner, depth, steps, cell)? {
                // Rust's `round` is half away from zero (`2.5 → 3`, `-2.5 → -3`),
                // which is the rule this build spells out rather than inherits
                // silently.
                Val::Num(num) => Ok(Val::Num(num.round())),
                Val::Empty => Ok(Val::Empty),
                other => Err(type_error("round", &other, "a number")),
            },
            Expr::Abs(inner) => match self.node(inner, depth, steps, cell)? {
                Val::Num(num) => Ok(Val::Num(num.abs())),
                Val::Empty => Ok(Val::Empty),
                other => Err(type_error("abs", &other, "a number")),
            },
            Expr::Extreme(op, left, right) => {
                let left = self.node(left, depth, steps, cell)?;
                if left == Val::Empty {
                    return Ok(Val::Empty);
                }
                let right = self.node(right, depth, steps, cell)?;
                if right == Val::Empty {
                    return Ok(Val::Empty);
                }
                extreme(*op, &left, &right)
            }
            Expr::Text(inner) => match self.node(inner, depth, steps, cell)? {
                // The one conversion, and the one place `Empty` stops being
                // contagious: asking a blank for its text is how a concatenation
                // gets to run.
                Val::Empty => Ok(Val::Str(String::new())),
                Val::Num(num) => Ok(Val::Str(format!("{num}"))),
                Val::Str(text) => Ok(Val::Str(text)),
                Val::Flag(true) => Ok(Val::Str("Yes".into())),
                Val::Flag(false) => Ok(Val::Str("No".into())),
                Val::Date(iso) => Ok(Val::Str(iso)),
            },
        }
    }
}

// ─── the evaluator's one arithmetic, comparison and extreme ─────────────────

/// `+ - * /` under the module header's table. `Empty` never reaches here (the
/// caller short-circuits), and a zero divisor is an error rather than an
/// infinity: a cell must paint a number or an `Error`, never `inf`.
pub(crate) fn arith(op: ArithOp, left: Val, right: Val) -> Result<Val, FormulaError> {
    match (op, &left, &right) {
        (ArithOp::Add, Val::Str(a), Val::Str(b)) => {
            let mut joined = String::with_capacity(a.len() + b.len());
            joined.push_str(a);
            joined.push_str(b);
            Ok(Val::Str(joined))
        }
        (ArithOp::Add, Val::Num(a), Val::Num(b)) => Ok(Val::Num(a + b)),
        (ArithOp::Sub, Val::Num(a), Val::Num(b)) => Ok(Val::Num(a - b)),
        (ArithOp::Mul, Val::Num(a), Val::Num(b)) => Ok(Val::Num(a * b)),
        (ArithOp::Div, Val::Num(_), Val::Num(0.0)) => {
            Err(FormulaError::Eval("division by zero".into()))
        }
        (ArithOp::Div, Val::Num(a), Val::Num(b)) => Ok(Val::Num(a / b)),
        // One sentence for both mixed and unsupported pairs: the table is the
        // spec, and the message can only name the two values it was handed.
        (op, a, b) => Err(FormulaError::Eval(format!(
            "{} cannot combine {} and {}",
            op.name(),
            a.type_name(),
            b.type_name()
        ))),
    }
}

/// Comparisons: numbers numerically, booleans by `false < true`, text and dates
/// by bytes. Both sides must be the *same* type, except text vs date — both are
/// byte strings, and ADR-0062 stores a date as one (the storage shape, not an
/// implicit conversion). A number against anything else is an error.
fn compare(op: Cmp, left: &Val, right: &Val) -> Result<Val, FormulaError> {
    let ordering = match (left, right) {
        (Val::Num(a), Val::Num(b)) => a.partial_cmp(b),
        (Val::Flag(a), Val::Flag(b)) => a.partial_cmp(b),
        (Val::Str(a), Val::Str(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
        (Val::Date(a), Val::Date(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
        // A date is stored as fixed-width ISO text, so comparing the two is
        // comparing the bytes each side already is. Chronological order follows
        // from the width (ADR-0062), not from a parse.
        (Val::Date(a), Val::Str(b)) | (Val::Str(a), Val::Date(b)) => {
            Some(a.as_bytes().cmp(b.as_bytes()))
        }
        (a, b) => {
            return Err(FormulaError::Eval(format!(
                "cannot compare {} and {}",
                a.type_name(),
                b.type_name()
            )))
        }
    };
    // A `NaN` can only arrive from a hand-written `db_values.num` (the write
    // path refuses to parse one, ADR-0069): it orders against nothing, so the
    // comparison is `false` — for `!=` too, which is the same answer SQL's
    // three-valued logic gives.
    let Some(ordering) = ordering else {
        return Ok(Val::Flag(false));
    };
    Ok(Val::Flag(match op {
        Cmp::Eq => ordering.is_eq(),
        Cmp::Ne => ordering.is_ne(),
        Cmp::Lt => ordering.is_lt(),
        Cmp::Le => ordering.is_le(),
        Cmp::Gt => ordering.is_gt(),
        Cmp::Ge => ordering.is_ge(),
    }))
}

/// `min` / `max` over one type family: all numbers, or all text/date by bytes.
pub(crate) fn extreme(op: ExtremeOp, left: &Val, right: &Val) -> Result<Val, FormulaError> {
    let keep_left = match (left, right) {
        (Val::Num(a), Val::Num(b)) => match op {
            ExtremeOp::Min => a <= b,
            ExtremeOp::Max => a >= b,
        },
        (Val::Str(a), Val::Str(b)) | (Val::Date(a), Val::Date(b)) => {
            let ordering = a.as_bytes().cmp(b.as_bytes());
            match op {
                ExtremeOp::Min => ordering.is_le(),
                ExtremeOp::Max => ordering.is_ge(),
            }
        }
        (Val::Date(a), Val::Str(b)) | (Val::Str(a), Val::Date(b)) => {
            let ordering = a.as_bytes().cmp(b.as_bytes());
            match op {
                ExtremeOp::Min => ordering.is_le(),
                ExtremeOp::Max => ordering.is_ge(),
            }
        }
        (a, b) => {
            return Err(FormulaError::Eval(format!(
                "{} cannot combine {} and {}",
                op.name(),
                a.type_name(),
                b.type_name()
            )))
        }
    };
    Ok(if keep_left {
        left.clone()
    } else {
        right.clone()
    })
}

impl ArithOp {
    pub(crate) fn name(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
        }
    }
}

impl ExtremeOp {
    pub(crate) fn name(self) -> &'static str {
        match self {
            ExtremeOp::Min => "min",
            ExtremeOp::Max => "max",
        }
    }
}

impl Val {
    /// How an error message names this value's type.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Val::Empty => "an empty value",
            Val::Num(_) => "a number",
            Val::Str(_) => "text",
            Val::Flag(_) => "a boolean",
            Val::Date(_) => "a date",
        }
    }
}

/// One sentence for every type mismatch: `<what> needs <wanted>`.
fn type_error(what: &str, value: &Val, wanted: &str) -> FormulaError {
    FormulaError::Eval(format!(
        "{what} needs {wanted}, not {}",
        value.type_name()
    ))
}

// ─── the lexer ──────────────────────────────────────────────────────────────

/// One token, with the byte offset it started at (the offset is what error
/// messages point at, so a long expression's mistake is findable).
#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    at: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Num(f64),
    Str(String),
    /// A bare word: a function name, `and` / `or` / `not` / `true` / `false`.
    Word(String),
    /// `[Column name]`: the text between the brackets, trimmed, unresolved.
    Column(String),
    Plus,
    Minus,
    Star,
    Slash,
    /// `==` (and its `=` alias), `!=`, `<`, `<=`, `>`, `>=`.
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LParen,
    RParen,
    Comma,
}

impl TokenKind {
    /// How an error message names this token.
    fn describe(&self) -> String {
        match self {
            TokenKind::Num(num) => format!("the number {num}"),
            TokenKind::Str(_) => "a text".into(),
            TokenKind::Word(word) => format!("\"{word}\""),
            TokenKind::Column(name) => format!("the column [{name}]"),
            TokenKind::Plus => "\"+\"".into(),
            TokenKind::Minus => "\"-\"".into(),
            TokenKind::Star => "\"*\"".into(),
            TokenKind::Slash => "\"/\"".into(),
            TokenKind::Eq => "\"==\"".into(),
            TokenKind::Ne => "\"!=\"".into(),
            TokenKind::Lt => "\"<\"".into(),
            TokenKind::Le => "\"<=\"".into(),
            TokenKind::Gt => "\">\"".into(),
            TokenKind::Ge => "\">=\"".into(),
            TokenKind::LParen => "\"(\"".into(),
            TokenKind::RParen => "\")\"".into(),
            TokenKind::Comma => "\",\"".into(),
        }
    }
}

/// Split `source` into tokens, or say where it stopped being an expression.
/// Bounded by [`FORMULA_MAX_TOKENS`] before anything else happens.
fn lex(source: &str) -> Result<Vec<Token>, FormulaError> {
    let bytes = source.as_bytes();
    let mut at = 0usize;
    let mut tokens: Vec<Token> = Vec::new();
    while at < bytes.len() {
        let byte = bytes[at];
        // Whitespace (including newlines: the editor is multi-line, and a
        // formula that wrapped across two lines is still one expression).
        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if tokens.len() >= FORMULA_MAX_TOKENS {
            return Err(FormulaError::Syntax(format!(
                "the formula has more than {FORMULA_MAX_TOKENS} tokens"
            )));
        }
        let start = at;
        let kind = match byte {
            b'+' => {
                at += 1;
                TokenKind::Plus
            }
            b'-' => {
                at += 1;
                TokenKind::Minus
            }
            b'*' => {
                at += 1;
                TokenKind::Star
            }
            b'/' => {
                at += 1;
                TokenKind::Slash
            }
            b'(' => {
                at += 1;
                TokenKind::LParen
            }
            b')' => {
                at += 1;
                TokenKind::RParen
            }
            b',' => {
                at += 1;
                TokenKind::Comma
            }
            b'=' => {
                at += 1;
                // `=` is accepted as `==`: one comparison spelled twice is
                // friendlier than a parse error about arithmetic assignment.
                if bytes.get(at) == Some(&b'=') {
                    at += 1;
                }
                TokenKind::Eq
            }
            b'!' => {
                at += 1;
                if bytes.get(at) == Some(&b'=') {
                    at += 1;
                    TokenKind::Ne
                } else {
                    return Err(FormulaError::Syntax(format!(
                        "expected \"!=\" at character {start}"
                    )));
                }
            }
            b'<' => {
                at += 1;
                if bytes.get(at) == Some(&b'=') {
                    at += 1;
                    TokenKind::Le
                } else {
                    TokenKind::Lt
                }
            }
            b'>' => {
                at += 1;
                if bytes.get(at) == Some(&b'=') {
                    at += 1;
                    TokenKind::Ge
                } else {
                    TokenKind::Gt
                }
            }
            b'"' => {
                at += 1;
                let mut text = String::new();
                loop {
                    match bytes.get(at) {
                        None => {
                            return Err(FormulaError::Syntax(format!(
                                "this text is never closed (it opens at character {start})"
                            )))
                        }
                        Some(b'"') => {
                            at += 1;
                            break;
                        }
                        // Exactly two escapes: a quote and a backslash. Anything
                        // else after a backslash keeps the backslash itself and
                        // the next character is handled by the loop — which is
                        // also what keeps a multi-byte character whole (a byte
                        // at a time would cut it in half).
                        Some(b'\\') => {
                            match bytes.get(at + 1) {
                                Some(byte @ (b'"' | b'\\')) => {
                                    text.push(*byte as char);
                                    at += 2;
                                }
                                Some(_) => {
                                    text.push('\\');
                                    at += 1;
                                }
                                None => {
                                    return Err(FormulaError::Syntax(format!(
                                        "this text is never closed (it opens at character {start})"
                                    )))
                                }
                            }
                        }
                        Some(_) => {
                            // Step one *character*, not one byte: a multi-byte
                            // character must not be cut in half.
                            let ch = source[at..].chars().next().unwrap_or('\u{fffd}');
                            text.push(ch);
                            at += ch.len_utf8();
                        }
                    }
                }
                TokenKind::Str(text)
            }
            b'[' => {
                at += 1;
                let close = source[at..].find(']').map(|i| at + i);
                let Some(close) = close else {
                    return Err(FormulaError::Syntax(format!(
                        "the column reference that opens at character {start} is never closed"
                    )));
                };
                let name = source[at..close].trim().to_string();
                at = close + 1;
                if name.is_empty() {
                    return Err(FormulaError::Syntax(format!(
                        "an empty column reference at character {start}"
                    )));
                }
                TokenKind::Column(name)
            }
            b'0'..=b'9' => {
                let digits_start = at;
                while matches!(bytes.get(at), Some(b'0'..=b'9')) {
                    at += 1;
                }
                if bytes.get(at) == Some(&b'.') {
                    at += 1;
                    while matches!(bytes.get(at), Some(b'0'..=b'9')) {
                        at += 1;
                    }
                }
                let text = &source[digits_start..at];
                let num = text.parse::<f64>().map_err(|_| {
                    FormulaError::Syntax(format!("\"{text}\" is not a number"))
                })?;
                if !num.is_finite() {
                    return Err(FormulaError::Syntax(format!(
                        "\"{text}\" is not a finite number"
                    )));
                }
                TokenKind::Num(num)
            }
            _ => {
                let ch = source[at..].chars().next().unwrap_or('\u{fffd}');
                if ch.is_alphabetic() || ch == '_' {
                    let word_start = at;
                    while let Some(next) = source[at..].chars().next() {
                        if next.is_alphanumeric() || next == '_' {
                            at += next.len_utf8();
                        } else {
                            break;
                        }
                    }
                    TokenKind::Word(source[word_start..at].to_string())
                } else {
                    return Err(FormulaError::Syntax(format!(
                        "\"{ch}\" is not part of an expression (character {start})"
                    )));
                }
            }
        };
        tokens.push(Token { kind, at: start });
    }
    Ok(tokens)
}

// ─── the parser ─────────────────────────────────────────────────────────────

/// Recursive descent over the grammar in the module header. `depth` is the
/// nesting guard ([`FORMULA_MAX_DEPTH`]), incremented on entry to a
/// parenthesised expression or an argument list — the two places a human's text
/// could nest without bound.
struct Parser<'a> {
    tokens: Vec<Token>,
    at: usize,
    depth: u32,
    /// Name → column id, the caller's schema.
    resolve: &'a dyn Fn(&str) -> Option<PropertyId>,
    /// Every column named, in the order the tree was built (a `BTreeSet`, so the
    /// order is by id and two parses of the same text agree).
    deps: BTreeSet<u64>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.tokens.get(self.at).map(|t| &t.kind)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        if token.is_some() {
            self.at += 1;
        }
        token
    }

    fn expect(&mut self, want: &TokenKind, what: &str) -> Result<(), FormulaError> {
        match self.peek() {
            Some(token) if &token.kind == want => {
                self.at += 1;
                Ok(())
            }
            Some(token) => Err(FormulaError::Syntax(format!(
                "expected {what} but found {} at character {}",
                token.kind.describe(),
                token.at
            ))),
            None => Err(FormulaError::Syntax(format!(
                "expected {what} but the formula ends"
            ))),
        }
    }

    fn expression(&mut self) -> Result<Expr, FormulaError> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.and()?;
        while self.word_is("or") {
            self.at += 1;
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.not()?;
        while self.word_is("and") {
            self.at += 1;
            let right = self.not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, FormulaError> {
        if self.word_is("not") {
            self.at += 1;
            let inner = self.not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr, FormulaError> {
        let left = self.sum()?;
        let op = match self.peek_kind() {
            Some(TokenKind::Eq) => Cmp::Eq,
            Some(TokenKind::Ne) => Cmp::Ne,
            Some(TokenKind::Lt) => Cmp::Lt,
            Some(TokenKind::Le) => Cmp::Le,
            Some(TokenKind::Gt) => Cmp::Gt,
            Some(TokenKind::Ge) => Cmp::Ge,
            _ => return Ok(left),
        };
        self.at += 1;
        let right = self.sum()?;
        // One comparison per expression, not a chain: `a < b < c` is a question
        // about booleans, and refusing it is kinder than answering "false".
        if let Some(token) = self.peek() {
            if matches!(
                token.kind,
                TokenKind::Eq
                    | TokenKind::Ne
                    | TokenKind::Lt
                    | TokenKind::Le
                    | TokenKind::Gt
                    | TokenKind::Ge
            ) {
                return Err(FormulaError::Syntax(format!(
                    "two comparisons in a row at character {} — use \"and\"",
                    token.at
                )));
            }
        }
        Ok(Expr::Compare(op, Box::new(left), Box::new(right)))
    }

    fn sum(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.product()?;
        loop {
            let op = match self.peek_kind() {
                Some(TokenKind::Plus) => ArithOp::Add,
                Some(TokenKind::Minus) => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.product()?;
            left = Expr::Arith(op, Box::new(left), Box::new(right));
        }
    }

    fn product(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek_kind() {
                Some(TokenKind::Star) => ArithOp::Mul,
                Some(TokenKind::Slash) => ArithOp::Div,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.unary()?;
            left = Expr::Arith(op, Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, FormulaError> {
        if matches!(self.peek_kind(), Some(TokenKind::Minus)) {
            self.at += 1;
            let inner = self.unary()?;
            return Ok(Expr::Neg(Box::new(inner)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, FormulaError> {
        let Some(token) = self.next() else {
            return Err(FormulaError::Syntax(
                "the formula ends where a value should be".into(),
            ));
        };
        match token.kind {
            TokenKind::Num(num) => Ok(Expr::Num(num)),
            TokenKind::Str(text) => Ok(Expr::Str(text)),
            TokenKind::Column(name) => {
                let Some(id) = (self.resolve)(&name) else {
                    return Err(FormulaError::Syntax(format!(
                        "there is no column named \"{name}\" (character {})",
                        token.at
                    )));
                };
                self.deps.insert(id.as_u64());
                Ok(Expr::Ref(id))
            }
            TokenKind::Word(word) => match word.as_str() {
                "true" => Ok(Expr::Flag(true)),
                "false" => Ok(Expr::Flag(false)),
                "and" | "or" | "not" => Err(FormulaError::Syntax(format!(
                    "\"{word}\" needs a value on both sides (character {})",
                    token.at
                ))),
                // A function call: a parenthesised, comma-separated argument
                // list whose arity is checked against `FUNCTIONS` — the one list
                // of what this build can compute.
                name => {
                    let Some((_, least, most)) = FUNCTIONS.iter().find(|(n, _, _)| *n == name)
                    else {
                        return Err(FormulaError::Syntax(format!(
                            "there is no function named \"{name}\" (character {})",
                            token.at
                        )));
                    };
                    self.expect(&TokenKind::LParen, "an opening parenthesis")?;
                    self.depth += 1;
                    if self.depth > FORMULA_MAX_DEPTH {
                        return Err(FormulaError::Syntax(format!(
                            "the formula nests more than {FORMULA_MAX_DEPTH} deep"
                        )));
                    }
                    let mut arguments: Vec<Expr> = Vec::new();
                    if !matches!(self.peek_kind(), Some(TokenKind::RParen)) {
                        loop {
                            arguments.push(self.expression()?);
                            match self.peek_kind() {
                                Some(TokenKind::Comma) => {
                                    self.at += 1;
                                }
                                _ => break,
                            }
                        }
                    }
                    self.expect(&TokenKind::RParen, "a closing parenthesis")?;
                    self.depth -= 1;
                    let least = *least;
                    if arguments.len() < least || most.is_some_and(|most| arguments.len() > most) {
                        return Err(FormulaError::Syntax(format!(
                            "\"{name}\" takes {} argument(s), not {}",
                            arity_words(least, *most),
                            arguments.len()
                        )));
                    }
                    Ok(build_call(name, arguments))
                }
            },
            TokenKind::LParen => {
                self.depth += 1;
                if self.depth > FORMULA_MAX_DEPTH {
                    return Err(FormulaError::Syntax(format!(
                        "the formula nests more than {FORMULA_MAX_DEPTH} deep"
                    )));
                }
                let inner = self.expression()?;
                self.expect(&TokenKind::RParen, "a closing parenthesis")?;
                self.depth -= 1;
                Ok(inner)
            }
            other => Err(FormulaError::Syntax(format!(
                "{} is not a value (character {})",
                other.describe(),
                token.at
            ))),
        }
    }

    /// Whether the next token is the bare word `word` — the parser's one way to
    /// look for `and` / `or` / `not` without treating them as function names.
    fn word_is(&self, word: &str) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Word(w)) if w.as_str() == word)
    }
}

/// Turn a checked argument list into the node the evaluator walks. `if` and the
/// conversions are not variadic, so their argument positions are named here;
/// `min` / `max` fold left.
fn build_call(name: &str, mut arguments: Vec<Expr>) -> Expr {
    match name {
        "if" => {
            let other = arguments.pop().expect("arity checked");
            let then = arguments.pop().expect("arity checked");
            let cond = arguments.pop().expect("arity checked");
            Expr::If(Box::new(cond), Box::new(then), Box::new(other))
        }
        "length" => Expr::Length(Box::new(arguments.pop().expect("arity checked"))),
        "round" => Expr::Round(Box::new(arguments.pop().expect("arity checked"))),
        "abs" => Expr::Abs(Box::new(arguments.pop().expect("arity checked"))),
        "text" => Expr::Text(Box::new(arguments.pop().expect("arity checked"))),
        "min" | "max" => {
            let op = if name == "min" {
                ExtremeOp::Min
            } else {
                ExtremeOp::Max
            };
            let mut iter = arguments.drain(..);
            let mut folded = iter.next().expect("arity checked");
            for argument in iter {
                folded = Expr::Extreme(op, Box::new(folded), Box::new(argument));
            }
            folded
        }
        // Unreachable: `primary` only builds calls whose name it found in
        // `FUNCTIONS`. Written out rather than `unreachable!()` so a future
        // function added to the list without a node is a compile-time-complete
        // match away from a panic.
        _ => Expr::Flag(false),
    }
}

/// How an arity error describes what a function takes.
fn arity_words(least: usize, most: Option<usize>) -> String {
    match most {
        Some(most) if most == least => least.to_string(),
        Some(most) => format!("{least} to {most}"),
        None => format!("{least} or more"),
    }
}

// ─── the save-time cycle check (ADR-0082's 「保存时做」) ─────────────────────

/// Would making `for_property` depend on `direct` create a cycle?
///
/// This is SPEC's 「relation 环检测在保存时做，不在渲染时做」 applied to the engine
/// this build does have: a formula may name another *formula* column (compose),
/// and a chain that comes back to where it started has no value to compute —
/// there is no fixed point to iterate to and no order in which to evaluate the
/// members. The write path asks this **before** the change is stored, so a cycle
/// is refused with a message while the user is looking at the expression that
/// would create it; the render path's only defence is
/// [`FORMULA_MAX_DEPTH`] (for a document that got in another way), which paints
/// `Error` instead of hanging.
///
/// `deps_of` answers "what does this column read directly": `Some(set)` for a
/// formula column (its parsed dependencies, the new text included for
/// `for_property`), `None` for a stored column (a leaf — nothing below it can
/// close a loop). The walk is iterative with a visited set, so it is bounded by
/// the number of formula columns rather than by any depth.
pub fn would_cycle(
    for_property: PropertyId,
    direct: &BTreeSet<u64>,
    deps_of: impl Fn(PropertyId) -> Option<BTreeSet<u64>>,
) -> bool {
    let target = for_property.as_u64();
    let mut visited: BTreeSet<u64> = BTreeSet::new();
    let mut stack: Vec<u64> = direct.iter().copied().collect();
    while let Some(id) = stack.pop() {
        if id == target {
            return true;
        }
        if !visited.insert(id) {
            continue;
        }
        if let Some(next) = deps_of(PropertyId(id)) {
            stack.extend(next);
        }
    }
    false
}

// ─── the expression's home in the column's `config` (ADR-0061/0062/0074) ────

/// The expression a formula column holds, out of its `config` document — the
/// `"formula"` key ADR-0082 puts there. `None` for a config that is not a
/// document, a document without the key, or a key that is not a text (every one
/// of which is "this column has no expression", the same fold the option list
/// makes).
///
/// **The value is not stored**: SPEC's 「不存值，投影时现算」 (ADR-0062) means the
/// config holds the *expression* and every cell's value is computed on the way
/// out of SQL — the same discipline `created time` follows with its two columns
/// (ADR-0068), and the reason a formula's cell is never written by any path.
pub fn config_formula(config: &str) -> Option<String> {
    let document = Json::parse(config).ok()?;
    let value = document.get("formula")?.as_str()?;
    if value.trim().is_empty() {
        return None;
    }
    Some(value.to_string())
}

/// `config` with its `"formula"` key set to `expression`, or with the key
/// **removed** for an empty expression (a column with no expression has no key —
/// one representation of "nothing", the same rule ADR-0062 makes for a cell).
///
/// The rest of the document is preserved **key for key and in order**: a config
/// this build does not understand (a later build's settings, a hand-edited
/// number format) survives a formula edit, which is ADR-0074's read-edit-write
/// discipline applied to a column instead of a view. A `config` that is not a
/// document is replaced by one, because there is nothing to preserve and an
/// expression has to live somewhere.
pub fn config_set_formula(config: &str, expression: &str) -> String {
    let trimmed = expression.trim();
    // Not a document (or not an object): there is nothing to preserve and an
    // expression has to live somewhere, so the column gets a fresh one.
    let mut document = match Json::parse(config) {
        Ok(Json::Object(fields)) => Json::Object(fields),
        _ => Json::Object(Vec::new()),
    };
    let Json::Object(fields) = &mut document else {
        unreachable!("just built an object")
    };
    fields.retain(|(key, _)| key != "formula");
    if !trimmed.is_empty() {
        fields.push(("formula".into(), Json::Text(trimmed.to_string())));
    }
    document.to_text()
}

// ─── the notes a reader of this file will want to test ─────────────────────
//
// What a future test suite should pin (the unified test's list, D6's §10 in
// `docs/REPORT_TRACK3.md`, is the authority — this is the short version):
//
// 1. **Every function and operator has one evaluation test**, including the
//    refusals: `"Total: " + [Points]` is an error (no implicit conversion),
//    `length([Points])` is an error, `1 / 0` is an error rather than `inf`,
//    `min("a", 1)` is an error.
// 2. **Empty is contagious** except through `if` and `text`, on a row whose
//    referenced cell has no value.
// 3. **`[Name]` resolution**: an unknown name is a syntax error at parse time
//    (and the save is refused), and the name is matched exactly (case included).
// 4. **The budgets**: a 2 049-token formula is refused; a 33-deep nesting is
//    refused; `FORMULA_MAX_STEPS` is reached by a wide expression (the exact
//    composition is the test's business, not this comment's).
// 5. **`would_cycle`** says yes for A→B→A and for A→A, no for A→B→C and for a
//    reference to a stored column.
// 6. **`config_set_formula`** keeps the keys it does not own, removes the key
//    for an empty expression, and round-trips through `config_formula`.
// 7. **`val_of`**'s two folds: a select cell and a list cell are `Empty`, a
//    date cell is `Val::Date`, an empty cell is `Empty` whatever the kind.

#[cfg(test)]
mod perf {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    /// A representative, non-trivial expression: two references, arithmetic, a
    /// comparison folded through `if`, and a nested function — the shape a real
    /// formula column takes, and the work `open the editor` and `recompute one
    /// cell` do.
    const SOURCE: &str = "if([Done], [Points] * 2, [Points] + length(\"pending\")) - 1";

    fn resolve(name: &str) -> Option<PropertyId> {
        match name {
            "Done" => Some(PropertyId(1)),
            "Points" => Some(PropertyId(2)),
            _ => None,
        }
    }

    fn cell(id: PropertyId, _depth: u32) -> Result<Val, FormulaError> {
        Ok(match id {
            PropertyId(1) => Val::Flag(true),
            PropertyId(2) => Val::Num(42.0),
            _ => Val::Empty,
        })
    }

    /// SPEC §三十九's 「打开公式编辑器的耗时」 and ADR-0083's recompute unit, in
    /// one pure-Rust number: opening the editor parses the stored expression
    /// once; recomputing a cell evaluates the window's formula cells.
    #[test]
    #[ignore = "prints a measurement; run with --release --lib -- --ignored --nocapture"]
    fn a_formula_parses_and_evaluates_in_nanoseconds() {
        const PARSES: usize = 50_000;
        const EVALS: usize = 500_000;

        let started = Instant::now();
        for _ in 0..PARSES {
            let parsed = Program::parse(SOURCE, resolve).unwrap();
            black_box(parsed.deps().len());
        }
        let parse_ns = started.elapsed().as_nanos() as f64 / PARSES as f64;

        let program = Program::parse(SOURCE, resolve).unwrap();
        assert_eq!(program.deps().len(), 2, "the two named columns");
        let started = Instant::now();
        for _ in 0..EVALS {
            let mut give = cell;
            let value = program.eval(&mut give).unwrap();
            black_box(&value);
        }
        let eval_ns = started.elapsed().as_nanos() as f64 / EVALS as f64;

        // ADR-0083's contract, stated as the arithmetic the projection runs:
        // opening a 10 000-row database paints the *window* (D0's 31 rows), so
        // one visible formula column costs `window × eval`, and a table ten
        // thousand rows deep pays the *same* number. Three formula columns is
        // the figure a busy view approaches.
        let window = 31.0;
        let one_column_us = window * eval_ns / 1_000.0;
        let three_columns_us = 3.0 * one_column_us;
        let full_table_us = 10_000.0 * eval_ns / 1_000.0;

        println!(
            "formula probe: expr = {SOURCE:?}\n  parse {parse_ns:.0} ns \
             (open-the-editor work)   eval {eval_ns:.0} ns (one painted cell)"
        );
        println!(
            "  recompute the window ({} rows × 1 formula column) = {one_column_us:.2} µs; \
             3 columns = {three_columns_us:.2} µs",
            window as usize
        );
        println!(
            "  the forbidden shape (10 000 rows recomputed) = {full_table_us:.1} µs — \
             {:.0}× the window, and what the projection never does",
            full_table_us / three_columns_us
        );
        println!(
            "{{\"label\":\"track3-d8-formula\",\"date\":\"2026-09-22\",\
             \"harness\":\"cargo test --release --lib -- --ignored --nocapture\",\
             \"parse_ns\":{parse_ns:.0},\"eval_ns\":{eval_ns:.0},\
             \"window_rows\":{window:.0},\"window_one_column_us\":{one_column_us:.3},\
             \"window_three_columns_us\":{three_columns_us:.3},\
             \"full_table_10000_us\":{full_table_us:.1}}}"
        );
    }
}
