// Rollups (SPEC §三十九 「需计算」's second third, ADR-0089).
//
// A rollup column aggregates **one column of the records a relation points
// at**. Its two inputs and one choice live in the column's own `config`
// (ADR-0061's one document per column, the same home ADR-0082 gave a formula's
// expression):
//
//     {"relation": 7, "column": 12, "aggregate": "sum"}
//
// `relation` names a relation column **of this rollup's own database** — the
// rollup's row supplies the targets, so the rollup writes none of its own.
// `column` names a property **of that relation's target database**, and
// `aggregate` is one of six words.
//
// Three things this module owns:
//
//   * **the six words** and their labels, in one list, because the editor's
//     menu, the config parser and the fold all have to agree about what can be
//     said.
//   * **the fold itself**, which is deliberately *not* a second implementation
//     of arithmetic: `sum` is `database_formula::arith` and `min` / `max` are
//     `database_formula::extreme`, so a rollup and a formula cannot disagree
//     about what the minimum of two dates is. The only thing added here is the
//     *strictness* — `sum` and `average` demand numbers, where the formula
//     language's `+` would happily concatenate two strings, because
//     "sum of a text column" is a configuration mistake and a painted `Error`
//     is a better answer than a surprise glued string.
//   * **the configuration check** (`check_config`), which is what makes a
//     dependency cycle unrepresentable rather than refused after the fact.
//
// What is deliberately *not* here: any value storage (ADR-0062 — a rollup's
// value is computed at projection time and stored nowhere), any cache across
// refreshes (ADR-0083's decision, inherited whole), and any knowledge of the
// store. The read that feeds `fold` is `AppState`'s, batched over the window.

use super::database::{DatabaseId, PropertyId, PropertyKind};
use super::database_formula::{self, arith, extreme, ArithOp, ExtremeOp, FormulaError, Val};

/// The `config` key naming the relation column whose targets are aggregated.
pub const RELATION_KEY: &str = "relation";
/// The `config` key naming the column of the related records that is aggregated.
pub const COLUMN_KEY: &str = "column";
/// The `config` key naming the fold.
pub const AGGREGATE_KEY: &str = "aggregate";

/// The one absence in the six: a rollup with no fold paints nothing, which is
/// what a rollup *is* before its two columns are picked. It is a word in the
/// list rather than "no word" for the same reason ADR-0062 makes `Empty` a
/// `CellValue` arm: a user clearing their choice and a user never having made
/// one should be the same state, and a state with a name is one the editor can
/// come back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Aggregate {
    #[default]
    None,
    Count,
    Sum,
    Min,
    Max,
    Average,
}

impl Aggregate {
    /// The six, in the order the editor offers them — `none` first, because it
    /// is the state a new rollup is in.
    pub const ALL: [Aggregate; 6] = [
        Aggregate::None,
        Aggregate::Count,
        Aggregate::Sum,
        Aggregate::Average,
        Aggregate::Min,
        Aggregate::Max,
    ];

    /// The stored word (ADR-0061's short stable string, like `blocks.kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            Aggregate::None => "none",
            Aggregate::Count => "count",
            Aggregate::Sum => "sum",
            Aggregate::Average => "average",
            Aggregate::Min => "min",
            Aggregate::Max => "max",
        }
    }

    pub fn try_from_str(word: &str) -> Option<Aggregate> {
        Aggregate::ALL.iter().copied().find(|a| a.as_str() == word)
    }

    /// The word the editor's menu shows. Capitalised, and the only place the
    /// *label* is spelled — the stored word is `as_str`, and the two are
    /// different questions ("what does the file say" and "what does the menu
    /// say").
    pub fn label(self) -> &'static str {
        match self {
            Aggregate::None => "None",
            Aggregate::Count => "Count",
            Aggregate::Sum => "Sum",
            Aggregate::Average => "Average",
            Aggregate::Min => "Min",
            Aggregate::Max => "Max",
        }
    }

    /// Whether this fold reads the target column at all. `none` and `count`
    /// do not, which is what lets a rollup of either kind work before its
    /// second column has been picked.
    pub fn reads_column(self) -> bool {
        matches!(
            self,
            Aggregate::Sum | Aggregate::Average | Aggregate::Min | Aggregate::Max
        )
    }
}

/// A rollup column's own settings (ADR-0089), read out of its `config`. Both
/// halves are optional for the same reason a relation's are: an unconfigured
/// rollup is a real state, and "no key" is how it is written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RollupConfig {
    pub relation: Option<PropertyId>,
    pub column: Option<PropertyId>,
    pub aggregate: Aggregate,
}

impl RollupConfig {
    /// Whether the rollup can compute anything at all: a fold and the relation
    /// that supplies its targets, with the target column demanded only by the
    /// folds that read one.
    pub fn is_configured(self) -> bool {
        self.relation.is_some() && !(self.aggregate.reads_column() && self.column.is_none())
    }
}

/// The rollup settings a `config` document holds. Never fails, and an
/// unreadable setting folds the way every unreadable setting does (ADR-0069):
/// an unknown `aggregate` word is `None` rather than an error, so a document
/// written by a build that knows more folds to "no fold" and the cell paints
/// nothing instead of the column failing to load.
pub fn config_rollup(config: &str) -> RollupConfig {
    use super::database_property::json::Json;
    let Ok(document) = Json::parse(config) else {
        return RollupConfig::default();
    };
    RollupConfig {
        relation: document
            .get(RELATION_KEY)
            .and_then(Json::as_u64)
            .map(PropertyId),
        column: document
            .get(COLUMN_KEY)
            .and_then(Json::as_u64)
            .map(PropertyId),
        aggregate: document
            .get(AGGREGATE_KEY)
            .and_then(Json::as_str)
            .and_then(Aggregate::try_from_str)
            .unwrap_or_default(),
    }
}

/// `config` with the three rollup keys set, or with a key removed when its
/// half is absent. The rest of the document survives key for key and in order
/// (ADR-0074's read-edit-write discipline, ADR-0082's implementation of it for
/// a column), and a `config` that is not a document is replaced by one.
pub fn config_set_rollup(
    config: &str,
    relation: Option<PropertyId>,
    column: Option<PropertyId>,
    aggregate: Aggregate,
) -> String {
    use super::database_property::json::Json;
    let mut document = match Json::parse(config) {
        Ok(Json::Object(fields)) => Json::Object(fields),
        _ => Json::Object(Vec::new()),
    };
    let Json::Object(fields) = &mut document else {
        unreachable!("just built an object")
    };
    fields.retain(|(key, _)| key != RELATION_KEY && key != COLUMN_KEY && key != AGGREGATE_KEY);
    if let Some(relation) = relation {
        fields.push((RELATION_KEY.into(), Json::Number(relation.as_u64() as f64)));
    }
    if let Some(column) = column {
        fields.push((COLUMN_KEY.into(), Json::Number(column.as_u64() as f64)));
    }
    // `none` is written down rather than omitted: it is one of the six, and a
    // user who chose it has made a decision a later reader should see.
    fields.push((
        AGGREGATE_KEY.into(),
        Json::Text(aggregate.as_str().to_string()),
    ));
    document.to_text()
}

// ─── the configuration check (ADR-0089) ─────────────────────────────────────

/// What the save path must be able to say about the relation column a rollup
/// names, and about the column it aggregates. `core` holds no store, so the
/// caller reads these facts out of the catalog it already has (`ColumnFacts`'s
/// shape, applied to the rollup's two names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RollupFacts {
    /// The relation column's kind. Must be `relation` — a rollup over a
    /// multi-select would be aggregating option ids.
    pub relation_kind: PropertyKind,
    /// The database the relation column points at.
    pub relation_target: Option<DatabaseId>,
    /// The target column's kind.
    pub column_kind: PropertyKind,
    /// Whether the target column is a column *of that database*.
    pub column_is_in_target: bool,
}

/// Why a rollup configuration was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigRefusal {
    /// The first name is not a relation column of this database.
    NotARelation,
    /// The relation column has no target, so there is nothing to aggregate.
    NoTarget,
    /// The second name is not a column of the database the relation points at.
    NotAColumnOfTheTarget,
    /// The second name is a computed column. **This is the one that makes a
    /// dependency cycle unrepresentable**: a rollup's only two inputs are a
    /// stored relation cell and a column of the related records, so the single
    /// shape that could close a loop is a rollup reading a rollup, and it is
    /// refused here — while the user is looking, which is ADR-0084's rule.
    TargetIsComputed,
    /// The second name is a relation column: aggregating ids is not a thing a
    /// user asked for, and every fold would be a type error at paint time.
    TargetIsRelation,
}

impl ConfigRefusal {
    pub fn message(self) -> &'static str {
        match self {
            ConfigRefusal::NotARelation => "rollups aggregate over a relation column",
            ConfigRefusal::NoTarget => "that relation column has no target database yet",
            ConfigRefusal::NotAColumnOfTheTarget => {
                "that column is not a column of the related database"
            }
            ConfigRefusal::TargetIsComputed => {
                "a rollup cannot aggregate another computed column"
            }
            ConfigRefusal::TargetIsRelation => "a rollup cannot aggregate a relation column",
        }
    }
}

/// Whether a rollup may be configured to aggregate `column` through `relation`
/// (ADR-0089). Checked in the order the cheapest answer comes first, and every
/// refusal is a state a user can be in rather than an internal invariant.
///
/// A rollup of `none` or `count` reads no target column, so the caller passes
/// the column's facts only when there is a column to check — this function
/// takes the whole story and answers for it, rather than trusting the caller to
/// have skipped the right parts.
pub fn check_config(facts: &RollupFacts) -> Result<(), ConfigRefusal> {
    if facts.relation_kind != PropertyKind::Relation {
        return Err(ConfigRefusal::NotARelation);
    }
    if facts.relation_target.is_none() {
        return Err(ConfigRefusal::NoTarget);
    }
    if !facts.column_is_in_target {
        return Err(ConfigRefusal::NotAColumnOfTheTarget);
    }
    if facts.column_kind == PropertyKind::Relation {
        return Err(ConfigRefusal::TargetIsRelation);
    }
    if facts.column_kind.is_computed() {
        return Err(ConfigRefusal::TargetIsComputed);
    }
    Ok(())
}

// ─── the fold ───────────────────────────────────────────────────────────────

/// Fold the values a rollup read, or the error one of them could not be folded
/// with. `related` is the number of records the relation names — which is what
/// `count` answers — and it is deliberately **not** `values.len()`: a related
/// record whose target cell is empty is still a related record, so
/// `count` counts it while the other four folds skip it.
///
/// The four reading folds answer [`Val::Empty`] over nothing to fold rather
/// than inventing a zero: ADR-0062's rule that empty is the absence of a value
/// and never `0`, applied to an aggregate. `count` over nothing *is* `0`, and
/// honestly so — "how many related records" has a true answer even when it is
/// none.
pub fn fold(
    aggregate: Aggregate,
    values: &[Val],
    related: usize,
) -> Result<Val, FormulaError> {
    match aggregate {
        Aggregate::None => Ok(Val::Empty),
        Aggregate::Count => Ok(Val::Num(related as f64)),
        Aggregate::Sum | Aggregate::Average => {
            let mut sum = Val::Num(0.0);
            for value in values {
                if !matches!(value, Val::Num(_)) {
                    return Err(FormulaError::Eval(format!(
                        "{} needs numbers, and this column holds {}",
                        aggregate.as_str(),
                        value_kind_name(value)
                    )));
                }
                sum = arith(ArithOp::Add, sum, value.clone())?;
            }
            if values.is_empty() {
                return Ok(Val::Empty);
            }
            if aggregate == Aggregate::Average {
                return arith(
                    ArithOp::Div,
                    sum,
                    Val::Num(values.len() as f64),
                );
            }
            Ok(sum)
        }
        Aggregate::Min | Aggregate::Max => {
            let op = if aggregate == Aggregate::Min {
                ExtremeOp::Min
            } else {
                ExtremeOp::Max
            };
            let mut folded: Option<Val> = None;
            for value in values {
                folded = Some(match folded {
                    Some(acc) => extreme(op, &acc, value)?,
                    None => value.clone(),
                });
            }
            Ok(folded.unwrap_or(Val::Empty))
        }
    }
}

/// How a fold's refusal names the value it could not use. `Val::type_name` is
/// the evaluator's own wording, so a rollup's error and a formula's error read
/// the same way.
fn value_kind_name(value: &Val) -> &'static str {
    match value {
        Val::Empty => "an empty value",
        other => other.type_name(),
    }
}

/// One rollup cell's painted form — the same `display` a formula's cell uses,
/// because a computed value has one painted shape in this app and a rollup is a
/// computed value (ADR-0089).
pub fn paint(value: &Val) -> String {
    database_formula::display(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_round_trips_keeps_foreign_keys_and_folds_an_unknown_fold_to_none() {
        let set = config_set_rollup(
            "",
            Some(PropertyId(7)),
            Some(PropertyId(12)),
            Aggregate::Sum,
        );
        assert_eq!(
            config_rollup(&set),
            RollupConfig {
                relation: Some(PropertyId(7)),
                column: Some(PropertyId(12)),
                aggregate: Aggregate::Sum,
            }
        );
        // A build that knows a seventh fold writes it down; this one paints
        // nothing rather than refusing the column (ADR-0069).
        let future = r#"{"aggregate":"median"}"#;
        assert_eq!(config_rollup(future).aggregate, Aggregate::None);
        // ADR-0074's discipline.
        let kept = config_set_rollup(r#"{"oracle":1}"#, None, None, Aggregate::Count);
        assert_eq!(config_rollup(&kept).aggregate, Aggregate::Count);
        assert!(kept.contains("oracle"), "{kept}");
        assert_eq!(config_rollup(&kept).relation, None);
        // The fold is written down even when it *is* "no fold": a user who
        // chose it has made a decision a later reader should be able to see.
        assert!(kept.contains("\"count\""), "{kept}");
        let cleared = config_set_rollup("", None, None, Aggregate::None);
        assert!(cleared.contains("\"none\""), "{cleared}");
    }

    #[test]
    fn a_rollup_is_configured_only_when_its_fold_has_what_it_reads() {
        // `count` needs no target column; `sum` does.
        assert!(RollupConfig {
            relation: Some(PropertyId(7)),
            column: None,
            aggregate: Aggregate::Count,
        }
        .is_configured());
        assert!(!RollupConfig {
            relation: Some(PropertyId(7)),
            column: None,
            aggregate: Aggregate::Sum,
        }
        .is_configured());
        assert!(!RollupConfig {
            relation: None,
            column: Some(PropertyId(12)),
            aggregate: Aggregate::Count,
        }
        .is_configured());
    }

    #[test]
    fn a_configuration_is_refused_for_the_shapes_that_would_report_wrong_or_cycle() {
        let ok = RollupFacts {
            relation_kind: PropertyKind::Relation,
            relation_target: Some(DatabaseId(2)),
            column_kind: PropertyKind::Number,
            column_is_in_target: true,
        };
        assert_eq!(check_config(&ok), Ok(()));
        assert_eq!(
            check_config(&RollupFacts {
                relation_kind: PropertyKind::MultiSelect,
                ..ok
            }),
            Err(ConfigRefusal::NotARelation)
        );
        assert_eq!(
            check_config(&RollupFacts {
                relation_target: None,
                ..ok
            }),
            Err(ConfigRefusal::NoTarget)
        );
        assert_eq!(
            check_config(&RollupFacts {
                column_is_in_target: false,
                ..ok
            }),
            Err(ConfigRefusal::NotAColumnOfTheTarget)
        );
        assert_eq!(
            check_config(&RollupFacts {
                column_kind: PropertyKind::Relation,
                ..ok
            }),
            Err(ConfigRefusal::TargetIsRelation)
        );
        // The one that makes a cycle unrepresentable.
        for kind in [PropertyKind::Formula, PropertyKind::Rollup] {
            assert_eq!(
                check_config(&RollupFacts {
                    column_kind: kind,
                    ..ok
                }),
                Err(ConfigRefusal::TargetIsComputed),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn the_five_reading_folds_fold_and_none_paints_nothing() {
        let numbers = [Val::Num(4.0), Val::Num(6.0), Val::Num(2.0)];
        let fold_of = |aggregate, values: &[Val], related| fold(aggregate, values, related).unwrap();

        assert_eq!(fold_of(Aggregate::None, &numbers, 3), Val::Empty);
        assert_eq!(fold_of(Aggregate::Count, &numbers, 3), Val::Num(3.0));
        // `count` counts the *related records*, not the values that had one:
        // a record with an empty cell is still related.
        assert_eq!(fold_of(Aggregate::Count, &[], 5), Val::Num(5.0));
        assert_eq!(fold_of(Aggregate::Sum, &numbers, 3), Val::Num(12.0));
        assert_eq!(fold_of(Aggregate::Average, &numbers, 3), Val::Num(4.0));
        assert_eq!(fold_of(Aggregate::Min, &numbers, 3), Val::Num(2.0));
        assert_eq!(fold_of(Aggregate::Max, &numbers, 3), Val::Num(6.0));

        // Nothing to fold is Empty, never 0 (ADR-0062), except `count`, whose
        // answer over nothing is truthfully 0.
        for aggregate in [
            Aggregate::Sum,
            Aggregate::Average,
            Aggregate::Min,
            Aggregate::Max,
        ] {
            assert_eq!(fold_of(aggregate, &[], 0), Val::Empty, "{aggregate:?}");
        }

        // Min / max work on the other two families, through the evaluator's own
        // `extreme` — so a rollup and a formula agree.
        let dates = [
            Val::Date("2026-01-02".into()),
            Val::Date("2025-12-31".into()),
        ];
        assert_eq!(
            fold_of(Aggregate::Min, &dates, 2),
            Val::Date("2025-12-31".into())
        );
        assert_eq!(
            fold_of(Aggregate::Max, &dates, 2),
            Val::Date("2026-01-02".into())
        );
    }

    #[test]
    fn sum_and_average_refuse_text_rather_than_gluing_it() {
        // The formula language's `+` concatenates two strings; a *sum* must not.
        let text = [Val::Num(1.0), Val::Str("two".into())];
        assert!(fold(Aggregate::Sum, &text, 2).is_err());
        assert!(fold(Aggregate::Average, &text, 2).is_err());
        // …and `min` over mixed families is the evaluator's own refusal.
        assert!(fold(Aggregate::Min, &text, 2).is_err());
    }
}
