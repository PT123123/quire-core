// Relations between databases (SPEC §三十九 「需计算」, ADR-0088).
//
// A relation column holds **the ids of the records this one points at**, one
// `db_value_items` row per target — the same list mechanism multi-select and
// files use (ADR-0062), which is why building the feature needed no migration
// and no new storage. What makes it a *relation* rather than a list of opaque
// strings is entirely in the column's `config` (ADR-0061's one document per
// column):
//
//     {"target": 4, "mirror": 19}
//
// `target` is the database this column points at. `mirror`, when present, is
// the property id of the **back-pointer column in the target database** — the
// other half of a two-way relation.
//
// Two things this module owns and nothing else does:
//
//   * **the pairing check** (`check_pair`). A two-way relation is not a graph
//     to walk; it is an involution on columns. Writing a forward change touches
//     the direct mirror and never descends, so the write terminates by
//     construction — but only if the pairing is legal, and "legal" here means
//     exactly `mirror(mirror(a)) == a` and `mirror(a) != a`. Refusing anything
//     else at save time is what makes a relation cycle *unrepresentable*
//     rather than something the read path has to survive.
//
//   * **the degradation word**. A target that no longer exists — deleted, or an
//     id naming no row in this library at all — is not an error: the stored
//     value is the fact "the user picked this", and what the target is called
//     now is a separate question the store may answer with "nothing"
//     (ADR-0051's rule, restated for records).
//
// What is deliberately *not* here: the title lookup itself. Reading a record's
// live title is `SqliteRepository::record_title` — ADR-0063's `COALESCE`, the
// one place a record's title already lives — and this module never sees the
// store.

use super::database::{DatabaseId, PropertyId, PropertyKind};

/// The `config` key naming the database a relation points at.
pub const TARGET_KEY: &str = "target";
/// The `config` key naming the back-pointer column in that database, when the
/// relation is two-way.
pub const MIRROR_KEY: &str = "mirror";

/// What a target that no longer exists paints as. Spelled the way ADR-0051
/// spelled its dangling mention (`(deleted page)`), for the thing a relation
/// actually points at.
pub const DELETED_RECORD_LABEL: &str = "(deleted record)";

/// A relation column's own settings, read out of its `config` (ADR-0088).
/// Both halves are optional because both are *absent* in the honest sense:
/// a relation nobody configured has no target, a one-way relation has no
/// mirror, and "no key" is how each is written down (the same
/// one-representation-of-nothing rule ADR-0062 makes for cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RelationConfig {
    pub target: Option<DatabaseId>,
    pub mirror: Option<PropertyId>,
}

impl RelationConfig {
    /// Whether a pick can be written at all. A relation with no readable target
    /// is a column with nothing to point *at* — the picker has no list to show,
    /// so the cell refuses rather than storing ids nothing can name.
    pub fn is_configured(self) -> bool {
        self.target.is_some()
    }
}

/// The relation settings a `config` document holds. Never fails: a config that
/// is not a document, a document without the keys, or a key of the wrong shape
/// all fold to "not configured" — ADR-0069's rule that an unreadable setting is
/// not an error, applied to the two settings a relation has.
pub fn config_relation(config: &str) -> RelationConfig {
    use super::database_property::json::Json;
    let Ok(document) = Json::parse(config) else {
        return RelationConfig::default();
    };
    RelationConfig {
        target: document
            .get(TARGET_KEY)
            .and_then(Json::as_u64)
            .map(DatabaseId),
        mirror: document
            .get(MIRROR_KEY)
            .and_then(Json::as_u64)
            .map(PropertyId),
    }
}

/// `config` with the two relation keys set to `target` / `mirror`, or with a
/// key **removed** when its half is `None` — a one-way relation stores no
/// `mirror` key, and an unconfigured one stores no `target` either.
///
/// The rest of the document is preserved key for key and in order, which is
/// ADR-0074's read-edit-write discipline applied to a column: a setting this
/// build does not understand survives a relation edit. A `config` that is not
/// a document is replaced by one, because there is nothing to preserve and
/// these two settings have to live somewhere.
pub fn config_set_relation(
    config: &str,
    target: Option<DatabaseId>,
    mirror: Option<PropertyId>,
) -> String {
    use super::database_property::json::Json;
    let mut document = match Json::parse(config) {
        Ok(Json::Object(fields)) => Json::Object(fields),
        _ => Json::Object(Vec::new()),
    };
    let Json::Object(fields) = &mut document else {
        unreachable!("just built an object")
    };
    fields.retain(|(key, _)| key != TARGET_KEY && key != MIRROR_KEY);
    if let Some(target) = target {
        fields.push((TARGET_KEY.into(), Json::Number(target.as_u64() as f64)));
    }
    if let Some(mirror) = mirror {
        fields.push((MIRROR_KEY.into(), Json::Number(mirror.as_u64() as f64)));
    }
    document.to_text()
}

/// Everything the save path can say about the column a pairing names, without
/// holding a store. `core` cannot open the catalog (ADR-0061's split: this
/// module decides, `AppState` answers), so the caller reads these four facts
/// out of the catalog it already has and hands them over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnFacts {
    pub kind: PropertyKind,
    /// The database the candidate column lives in.
    pub db: DatabaseId,
    /// The target the candidate already declares, if any.
    pub target: Option<DatabaseId>,
    /// The partner the candidate already declares, if any.
    pub mirror: Option<PropertyId>,
}

/// Why a pairing was refused. Each is a state a user can be in — not an
/// internal invariant — so each has a sentence, and the sentence is what the
/// save shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRefusal {
    /// A column cannot be its own other half.
    SelfMirror,
    /// The named column is not a relation.
    NotARelation,
    /// The named column is a relation, but not *this* relation's other half:
    /// it does not live in this column's target database, or it does not point
    /// back here.
    NotABackPointer,
    /// The named column is already somebody else's other half.
    AlreadyPaired,
}

impl PairRefusal {
    pub fn message(self) -> &'static str {
        match self {
            PairRefusal::SelfMirror => "a column cannot be its own back-pointer",
            PairRefusal::NotARelation => "the back-pointer must be a relation column",
            PairRefusal::NotABackPointer => {
                "that column does not point back at this database"
            }
            PairRefusal::AlreadyPaired => "that column already points back at another column",
        }
    }
}

/// Whether `candidate` may be the back-pointer of the relation `forward` —
/// the whole of ADR-0088's save-time check, and the reason a relation cycle is
/// not representable rather than merely refused.
///
/// The four refusals, in the order they are asked (each is a fact about the
/// candidate alone until the last, so the cheapest answer comes first):
///
/// 1. **A column is not its own mirror.** A self-relation *is* allowed — a
///    database may point at itself — but a column paired with itself would make
///    one write trigger itself forever.
/// 2. **The partner must be a relation.** Pairing with a text column would
///    store back-pointers nowhere.
/// 3. **It must point back — or be free to be pointed back.** The candidate has
///    to live in this column's target database, and it has to either already
///    declare this column's database as its target or declare no target at all.
///    The second half is not a loophole: the accepted declaration *is* what
///    writes the candidate's `target` (one batch, both sides), so a brand-new
///    back-pointer column arrives with nothing declared and leaves paired. A
///    candidate declaring a *third* database is the coincidence of ids this
///    refusal is for — pairing it would leave a column whose own list shows one
///    database while its back-pointer names another.
/// 4. **It must be free.** Because the map is an involution, accepting a
///    pairing is symmetric: this function accepting it is also what makes
///    `mirror(mirror(a)) == a` true, and a candidate that already answers to
///    somebody else would break that equation for them.
pub fn check_pair(
    forward: PropertyId,
    forward_db: DatabaseId,
    forward_target: DatabaseId,
    candidate: PropertyId,
    facts: &ColumnFacts,
) -> Result<(), PairRefusal> {
    if candidate == forward {
        return Err(PairRefusal::SelfMirror);
    }
    if facts.kind != PropertyKind::Relation {
        return Err(PairRefusal::NotARelation);
    }
    if facts.db != forward_target {
        return Err(PairRefusal::NotABackPointer);
    }
    if matches!(facts.target, Some(declared) if declared != forward_db) {
        return Err(PairRefusal::NotABackPointer);
    }
    match facts.mirror {
        None => Ok(()),
        Some(partner) if partner == forward => Ok(()),
        Some(_) => Err(PairRefusal::AlreadyPaired),
    }
}

/// A relation cell's painted form: every target through `name_of`, or the
/// degradation word when nothing can name it. The shape is `paint_list`'s
/// (ADR-0069's comma-joined list), deliberately — a relation cell and a
/// multi-select cell are both "a list of things" on screen, and giving them two
/// join rules would be two answers to one question.
///
/// `name_of` is the caller's, and the caller is `AppState` reading
/// `record_title`: this module is not allowed to know the store (see the
/// header), and the *live* title is the whole point — a renamed target shows
/// its new name with no write anywhere.
///
/// The two answers `name_of` can give are kept apart, and that distinction is
/// the whole reason this is a function rather than a `join`: `None` is "no such
/// record" and paints the degradation word, while `Some("")` is a record that
/// exists and has no title yet — which paints its own emptiness, because
/// inventing a word for it would make "this row has no name" and "this row is
/// gone" look alike.
pub fn paint_targets(ids: &[String], name_of: impl Fn(&str) -> Option<String>) -> String {
    ids.iter()
        .map(|target| name_of(target).unwrap_or_else(|| DELETED_RECORD_LABEL.to_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(kind: PropertyKind, db: u64, target: Option<u64>, mirror: Option<u64>) -> ColumnFacts {
        ColumnFacts {
            kind,
            db: DatabaseId(db),
            target: target.map(DatabaseId),
            mirror: mirror.map(PropertyId),
        }
    }

    #[test]
    fn a_config_round_trips_and_keeps_the_keys_it_does_not_own() {
        let set = config_set_relation("", Some(DatabaseId(4)), Some(PropertyId(19)));
        assert_eq!(
            config_relation(&set),
            RelationConfig {
                target: Some(DatabaseId(4)),
                mirror: Some(PropertyId(19)),
            }
        );
        // A one-way relation stores no mirror key, and clearing the target
        // leaves neither behind.
        let one_way = config_set_relation(&set, Some(DatabaseId(4)), None);
        assert_eq!(config_relation(&one_way).mirror, None);
        let cleared = config_set_relation(&one_way, None, None);
        assert_eq!(cleared, "{}");
        // ADR-0074's discipline: a key this build does not write survives.
        let kept = config_set_relation(r#"{"oracle":true}"#, Some(DatabaseId(4)), None);
        assert_eq!(config_relation(&kept).target, Some(DatabaseId(4)));
        assert!(kept.contains("oracle"), "{kept}");
    }

    #[test]
    fn an_unreadable_config_is_a_relation_with_no_target_rather_than_an_error() {
        for bad in ["", "not json", "[]", r#"{"target":"four"}"#, r#"{"target":null}"#] {
            let parsed = config_relation(bad);
            assert_eq!(parsed.target, None, "{bad}");
            assert!(!parsed.is_configured(), "{bad}");
        }
    }

    #[test]
    fn a_pairing_is_accepted_only_when_it_is_an_involution() {
        let forward = PropertyId(7);
        let (forward_db, forward_target) = (DatabaseId(1), DatabaseId(2));
        let candidate = PropertyId(19);

        // The legal shape: a free relation in the target database, pointing back.
        assert_eq!(
            check_pair(
                forward,
                forward_db,
                forward_target,
                candidate,
                &facts(PropertyKind::Relation, 2, Some(1), None)
            ),
            Ok(())
        );
        // Idempotence: re-declaring the same pair is not a second refusal.
        assert_eq!(
            check_pair(
                forward,
                forward_db,
                forward_target,
                candidate,
                &facts(PropertyKind::Relation, 2, Some(1), Some(7))
            ),
            Ok(())
        );
        // A brand-new back-pointer column: it declares no target yet, and the
        // accepted declaration is what writes one. Refusing this would make the
        // one-shot pairing — the gesture a user actually makes — impossible.
        assert_eq!(
            check_pair(
                forward,
                forward_db,
                forward_target,
                candidate,
                &facts(PropertyKind::Relation, 2, None, None)
            ),
            Ok(()),
            "a free relation in the target database is free to be the back-pointer"
        );
        // The four refusals.
        assert_eq!(
            check_pair(
                forward,
                forward_db,
                forward_target,
                forward,
                &facts(PropertyKind::Relation, 1, Some(2), None)
            ),
            Err(PairRefusal::SelfMirror)
        );
        assert_eq!(
            check_pair(
                forward,
                forward_db,
                forward_target,
                candidate,
                &facts(PropertyKind::Text, 2, None, None)
            ),
            Err(PairRefusal::NotARelation)
        );
        assert_eq!(
            check_pair(
                forward,
                forward_db,
                forward_target,
                candidate,
                &facts(PropertyKind::Relation, 3, Some(1), None)
            ),
            Err(PairRefusal::NotABackPointer),
            "a relation in a third database is not this column's other half"
        );
        assert_eq!(
            check_pair(
                forward,
                forward_db,
                forward_target,
                candidate,
                &facts(PropertyKind::Relation, 2, Some(9), None)
            ),
            Err(PairRefusal::NotABackPointer),
            "pointing back at a third database is not pointing back here"
        );
        assert_eq!(
            check_pair(
                forward,
                forward_db,
                forward_target,
                candidate,
                &facts(PropertyKind::Relation, 2, Some(1), Some(31))
            ),
            Err(PairRefusal::AlreadyPaired)
        );
    }

    #[test]
    fn a_self_relation_may_pair_with_its_own_database() {
        // A database pointing at itself: the target and the column's own
        // database are the same, and a free relation there is legal.
        assert_eq!(
            check_pair(
                PropertyId(7),
                DatabaseId(1),
                DatabaseId(1),
                PropertyId(19),
                &facts(PropertyKind::Relation, 1, Some(1), None)
            ),
            Ok(())
        );
    }

    #[test]
    fn a_target_that_cannot_be_named_degrades_and_still_counts() {
        let ids = vec!["4".to_string(), "5".to_string(), "6".to_string()];
        let painted = paint_targets(&ids, |id| match id {
            "4" => Some("Atlas".to_string()),
            // A record with an empty title is still a record: it paints its
            // own (empty) name, not the degradation word.
            "5" => Some(String::new()),
            _ => None,
        });
        assert_eq!(painted, "Atlas, , (deleted record)");
    }
}
