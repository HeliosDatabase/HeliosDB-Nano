//! Plan-time name resolution for column references (GH#29).
//!
//! PostgreSQL resolves every column reference against the *range table* of the
//! query level being planned — the FROM clause's tables, aliases, derived
//! tables, CTEs and views — and refuses at plan time what it cannot resolve:
//! `42703 undefined_column` for a column no range entry carries, `42P01
//! undefined_table` (`missing FROM-clause entry`) for a qualifier that names no
//! range entry, `42712 duplicate_alias` for a name declared twice at one level
//! and `42702 ambiguous_column` for a reference two entries could satisfy.
//!
//! Through v4.31.1 the planner emitted every reference blind and left
//! resolution to the evaluator, PER ROW. Three fail-open behaviours followed:
//! an unknown column on an EMPTY table was not an error at all (the evaluator
//! never ran), `SELECT bogus.*` expanded to the WHOLE row when the qualifier
//! matched nothing, and an unquoted mixed-case alias (`FROM t AS T1`) could not
//! be referenced because the alias was stored raw while the reference was
//! case-folded. This module is the one place the planner now asks "what does
//! this name refer to?", and the answer is exact: every comparison is `==` on
//! identifiers already normalised by `Planner::normalize_ident` — `"T1"` and
//! `"t1"` are two different relations, exactly as in PostgreSQL.
//!
//! # Where it deliberately does NOT refuse
//!
//! * **No scope pushed ⇒ [`Resolution::Unscoped`] ⇒ the planner lowers the
//!   reference exactly as before.** Trigger `WHEN` conditions, `CHECK`
//!   constraints, index expressions, tenant/RLS predicates, `ON CONFLICT`
//!   targets and the catalog-less planner used by unit tests never push a
//!   scope. A refusal we cannot prove correct would be broken, not fail-closed.
//! * **The real table name stays accepted while an alias is in scope**
//!   (`SELECT t.id FROM t AS t1`; PostgreSQL raises 42P01). The extra spelling
//!   can only ever name the SAME relation, never a different column or a wider
//!   row, and `tests/gh_issue_29.rs::item2_alias_spellings_matrix` pins it.
//!   It is second-tier: an alias always wins over a real name, and a real name
//!   that two aliased entries share is not usable (42P01, as in PostgreSQL).
//! * **An entry whose columns are unknown at plan time** (a system view the
//!   registry describes with an empty schema) accepts any column name — the
//!   evaluator keeps refusing per row as it always did.
//!
//! # Runtime qualifier
//!
//! A resolved reference is REWRITTEN to the qualifier the executor's schema
//! actually carries, so the per-row lookup cannot miss where the plan-time one
//! hit: `Some(alias-or-table)` for a base table, CTE or system view
//! (`scan::handle_scan` stamps `source_table = alias.unwrap_or(table_name)`),
//! `Some(alias-or-function)` for a table function, `Some(<resolved key>)` for
//! a DML target (`Schema::with_source_table_name`), and — since candidate 2 —
//! `Some(alias)` for a derived table or an expanded view as well: the
//! planner stamps the sub-plan's root `Project` with `source_alias` and the
//! executor's `SourceAliasOperator` tags its output columns with it, so
//! `s.id` next to a base table that also has an `id` resolves at runtime
//! (`FROM t JOIN (SELECT id FROM t) s ON s.id = t.id`, the SQLAlchemy
//! `anon_1` / Prisma `_count` shape). The one residual is an UNALIASED
//! sub-select, which has no alias to stamp (`None`): its references are
//! rewritten to the bare column name, and a reference another entry could
//! also satisfy is refused with 42702 rather than resolved by position,
//! because `JoinReorderingRule` may swap inner-join sides after planning.
//! A sub-select whose output carries one name TWICE is stamped like any
//! other (candidate 3, `Planner::stamp_derived_plan`); whether a written
//! reference to that name is 42702 is decided by
//! [`RangeEntry::duplicate_is_ambiguous`], not by the stamp.

use crate::{Error, Schema};
use std::cell::RefCell;
use std::sync::Arc;

/// Marker for the 42P01 refusal of a qualifier that names no range entry.
/// The full message also contains `relation "…" does not exist`, which the
/// wire classifier already maps to 42P01; the marker is PostgreSQL's wording.
pub(crate) const MISSING_FROM_CLAUSE_ENTRY: &str = "missing FROM-clause entry for table";
/// Prefix of the 42712 duplicate_alias message (`table name "a" specified
/// more than once`). The classifier anchors on [`is_duplicate_range_entry`],
/// which requires BOTH this prefix and [`DUPLICATE_RANGE_ENTRY`]: the trailing
/// phrase alone is a substring of the CTAS refusal `column "x" specified more
/// than once`, which must stay `XX000`-class, not become 42712 (GH#29 c2, 4c).
pub(crate) const DUPLICATE_RANGE_ENTRY_PREFIX: &str = "table name \"";
/// Trailing phrase of the 42712 duplicate_alias message; see
/// [`DUPLICATE_RANGE_ENTRY_PREFIX`].
pub(crate) const DUPLICATE_RANGE_ENTRY: &str = "\" specified more than once";
/// Marker the wire classifier anchors on for 42702 ambiguous_column
/// (`column reference "c" is ambiguous`).
pub(crate) const AMBIGUOUS_COLUMN_REFERENCE: &str = "column reference \"";
/// Marker the wire classifier anchors on for 42883 undefined_function —
/// PostgreSQL's own wording for an operator with no matching signature
/// (`operator does not exist: text + integer`, GH#29 c5).
pub(crate) const UNDEFINED_OPERATOR: &str = "operator does not exist: ";
/// Marker the wire classifier anchors on for 42P10 invalid_column_reference:
/// a column-alias list longer than the sub-select's output (PostgreSQL's own
/// wording, `table "s" has 1 columns available but 2 columns specified`).
pub(crate) const DERIVED_COLUMN_LIST_TOO_LONG: &str = "columns available but";
/// Marker the wire classifier anchors on for 0A000 feature_not_supported: a
/// CORRELATED scalar subquery inside a JOIN's ON condition (GH#29 c6, m1).
/// The ON condition is materialized once, before either join input is
/// built, so there is no outer row to resolve the correlation against;
/// standing NULL in for the subquery — which every other materialization
/// path does, for drizzle's introspection queries — would silently drop
/// join rows instead of saying so. Only a reference the subquery's own
/// scopes cannot resolve earns this refusal (GH#29 c7, M5): every other
/// failure keeps its own message and SQLSTATE.
pub(crate) const CORRELATED_JOIN_SUBQUERY_UNSUPPORTED: &str = "correlated subquery in JOIN ... ON is not supported";

/// Marker the wire classifier anchors on for 0A000 feature_not_supported: a
/// materialized view whose plan references a sub-select / view column
/// through its alias while another entry of the same join carries the same
/// bare name (`SELECT s.id FROM t JOIN (SELECT id FROM t) s …`). The stored
/// plan is bincode and cannot carry the alias stamp
/// (`LogicalPlan::Project::source_alias` is not persisted); every
/// UNshadowed reference is rewritten to the bare name before the plan is
/// stored (`sql::mv_destamp`, GH#29 c3), so only this shape is refused —
/// up front, with the workaround in the message.
pub(crate) const MATERIALIZED_VIEW_DERIVED_ALIAS_UNSUPPORTED: &str =
    "materialized view cannot reference a sub-select or view by its alias";

/// `Column "q"."c" does not exist` / `Column "c" does not exist` — the shape
/// `sqlstate_for_query_execution_message` classifies as 42703.
pub(crate) fn undefined_column(qualifier: Option<&str>, name: &str) -> Error {
    match qualifier {
        Some(q) => Error::query_execution(format!("Column \"{q}\".\"{name}\" does not exist")),
        None => Error::query_execution(format!("Column \"{name}\" does not exist")),
    }
}

/// `relation "q" does not exist (missing FROM-clause entry for table "q")` —
/// classified as 42P01 by the existing `relation … does not exist` arm.
pub(crate) fn missing_from_clause_entry(qualifier: &str) -> Error {
    Error::query_execution(format!(
        "relation \"{qualifier}\" does not exist ({MISSING_FROM_CLAUSE_ENTRY} \"{qualifier}\")"
    ))
}

/// `table name "q" specified more than once` — 42712 via [`is_duplicate_range_entry`].
pub(crate) fn duplicate_range_entry(qualifier: &str) -> Error {
    Error::query_execution(format!(
        "{DUPLICATE_RANGE_ENTRY_PREFIX}{qualifier}{DUPLICATE_RANGE_ENTRY}"
    ))
}

/// Is `message` the 42712 refusal built by [`duplicate_range_entry`]? Both
/// halves are required so the CTAS `column "x" specified more than once`
/// cannot be reclassified (GH#29 c2, 4c).
pub(crate) fn is_duplicate_range_entry(message: &str) -> bool {
    message.contains(DUPLICATE_RANGE_ENTRY_PREFIX) && message.contains(DUPLICATE_RANGE_ENTRY)
}

/// `correlated subquery in JOIN ... ON is not supported` — 0A000 via
/// [`CORRELATED_JOIN_SUBQUERY_UNSUPPORTED`].
pub(crate) fn correlated_join_subquery_unsupported() -> Error {
    Error::query_execution(format!(
        "{CORRELATED_JOIN_SUBQUERY_UNSUPPORTED}; rewrite it as a join or a subquery in WHERE"
    ))
}

/// `column reference "c" is ambiguous` — 42702 via [`AMBIGUOUS_COLUMN_REFERENCE`].
pub(crate) fn ambiguous_column(name: &str) -> Error {
    Error::query_execution(format!("{AMBIGUOUS_COLUMN_REFERENCE}{name}\" is ambiguous"))
}

/// `cannot expand "q".*: …` — an entry whose columns are unknown at plan time
/// cannot be expanded to a column list without guessing. XX000-class on
/// purpose: it is an engine limitation, not a user error.
pub(crate) fn wildcard_over_opaque_entry(qualifier: &str) -> Error {
    Error::query_execution(format!(
        "cannot expand \"{qualifier}\".*: the columns of \"{qualifier}\" are not known at plan time"
    ))
}

/// 42P10: `table "s" has 1 columns available but 2 columns specified` — see
/// [`DERIVED_COLUMN_LIST_TOO_LONG`].
pub(crate) fn derived_column_list_too_long(alias: &str, available: usize, specified: usize) -> Error {
    Error::query_execution(format!(
        "table \"{alias}\" has {available} {DERIVED_COLUMN_LIST_TOO_LONG} {specified} columns specified"
    ))
}

/// 0A000: a materialized view whose `alias.column` names a stamped
/// sub-select / view while another entry of the same join also carries
/// `column` — see [`MATERIALIZED_VIEW_DERIVED_ALIAS_UNSUPPORTED`].
pub(crate) fn materialized_view_derived_alias_shadowed(alias: &str, column: &str) -> Error {
    Error::query_execution(format!(
        "{MATERIALIZED_VIEW_DERIVED_ALIAS_UNSUPPORTED} (\"{alias}\".\"{column}\") when another FROM entry also carries \"{column}\": its stored plan cannot carry the alias, so alias the column inside the sub-select (… {column} AS {alias}_{column} …) and reference it unqualified"
    ))
}

/// One range-table entry (a PostgreSQL RTE) at the current query level.
#[derive(Debug, Clone)]
pub(crate) struct RangeEntry {
    /// The entry's own name(s), normalised: the alias when there is one,
    /// otherwise the resolved table key plus its bare last component (so both
    /// `s.t.c` and `t.c` reach `FROM s.t`). A derived table has only its alias.
    pub names: Vec<String>,
    /// Second-tier names accepted while an alias is in scope: the real table
    /// key and bare component of an aliased base table / CTE / view, or the
    /// function name of an aliased table function. Only consulted when no
    /// entry at the level matches by [`Self::names`]; two entries sharing a
    /// lenient name make it unusable (42P01), never ambiguous-by-luck.
    pub lenient_names: Vec<String>,
    /// What `Column::source_table` carries AT RUNTIME for this entry's columns
    /// (see the module docs); `None` only for an unaliased sub-select, whose
    /// columns cannot be qualified anyway.
    pub runtime_qualifier: Option<String>,
    /// The entry's output schema — `factor_plan.schema()`, an `Arc`, no copy.
    pub schema: Arc<Schema>,
    /// GH#29 (c11, M1): was this entry's output column LIST produced by a
    /// wildcard (`SELECT *`, `SELECT a.*`) rather than written out column by
    /// column? Only a sub-select, CTE or view can answer anything but `false`.
    ///
    /// It decides whether a name the entry carries TWICE is ambiguous. A
    /// select list that NAMES two columns of one name (`SELECT a.id, b.id …`)
    /// really is two different columns, and PostgreSQL refuses `s.id` over it
    /// — [`Self::duplicate_is_ambiguous`] keeps that 42702. A list the engine
    /// expanded is different: we do NOT merge the shared output column of a
    /// `NATURAL` / `USING` join (we project `(id, a, id, b)` where PostgreSQL
    /// projects `(id, a, b)` — sprinter 781f55ba534d), so the duplicate is an
    /// artifact of OUR output shape, PostgreSQL's `j` carries `id` once, and
    /// `SELECT j.id FROM j` is ordinary working SQL that v4.31.1 answered.
    /// Refusing it would be a regression against the shipped release, so the
    /// reference resolves to the FIRST of the two slots, which is what the
    /// sub-select itself returns.
    pub wildcard_output: bool,
}

impl RangeEntry {
    /// Build an entry, de-duplicating names and dropping from `lenient_names`
    /// anything already in `names`.
    pub(crate) fn new(
        names: Vec<String>,
        lenient_names: Vec<String>,
        runtime_qualifier: Option<String>,
        schema: Arc<Schema>,
    ) -> Self {
        let mut primary: Vec<String> = Vec::with_capacity(names.len());
        for n in names {
            if !n.is_empty() && !primary.contains(&n) {
                primary.push(n);
            }
        }
        let mut lenient: Vec<String> = Vec::with_capacity(lenient_names.len());
        for n in lenient_names {
            if !n.is_empty() && !primary.contains(&n) && !lenient.contains(&n) {
                lenient.push(n);
            }
        }
        Self {
            names: primary,
            lenient_names: lenient,
            runtime_qualifier,
            schema,
            wildcard_output: false,
        }
    }

    /// Mark (or unmark) this entry's output list as wildcard-expanded — see
    /// [`Self::wildcard_output`]. A builder rather than a `new` argument so
    /// the eleven call sites that cannot be wildcard-expanded (base tables,
    /// table functions, DML targets) stay unchanged.
    pub(crate) fn with_wildcard_output(mut self, wildcard_output: bool) -> Self {
        self.wildcard_output = wildcard_output;
        self
    }

    /// An entry whose columns are not known at plan time (empty schema). It
    /// accepts every column name; the evaluator keeps refusing per row.
    pub(crate) fn is_opaque(&self) -> bool {
        self.schema.columns.is_empty()
    }

    /// Does this entry carry `column` (exact, normalised name)?
    pub(crate) fn has_column(&self, column: &str) -> bool {
        self.is_opaque() || self.schema.get_column_index(column).is_some()
    }

    /// Does this entry carry `column` MORE THAN ONCE — a sub-select whose
    /// output repeats a name (`(SELECT a.id, b.id FROM a JOIN b …) s`)? The
    /// runtime lookup by alias and name can only answer with the first slot,
    /// so `s.*` — which would emit the name twice and read that one slot for
    /// both — is refused rather than guessed (GH#29 c3, m3), whatever built
    /// the list. Whether a WRITTEN `s.id` is refused depends on how the list
    /// was built: see [`Self::duplicate_is_ambiguous`]. Every other column of
    /// the entry keeps resolving through the alias.
    pub(crate) fn has_duplicate_column(&self, column: &str) -> bool {
        self.schema.columns.iter().filter(|c| c.name == column).count() > 1
    }

    /// Is a WRITTEN reference to `column` — `s.id` or the bare `id` — refused
    /// with 42702 because this entry carries the name twice? Only when the
    /// entry's select list NAMED both of them: see [`Self::wildcard_output`]
    /// for why a wildcard-expanded duplicate resolves to the first slot
    /// instead (GH#29 c11, M1).
    ///
    /// ACCEPTED LENIENCY, stated: a wildcard over a join whose two same-named
    /// columns were never equated (`SELECT * FROM a JOIN b ON a.x = b.x`, both
    /// carrying `id`) also resolves to the first slot, where PostgreSQL calls
    /// it ambiguous. That is v4.31.1's behaviour and the engine-wide
    /// unqualified-name rule; distinguishing it needs the merged output column
    /// (sprinter 781f55ba534d), which is a separate contract move.
    pub(crate) fn duplicate_is_ambiguous(&self, column: &str) -> bool {
        !self.wildcard_output && self.has_duplicate_column(column)
    }

    fn names_contain(&self, qualifier: &str) -> bool {
        self.names.iter().any(|n| n == qualifier)
    }

    fn lenient_names_contain(&self, qualifier: &str) -> bool {
        self.lenient_names.iter().any(|n| n == qualifier)
    }
}

/// The range table of ONE query level.
#[derive(Debug, Default, Clone)]
pub(crate) struct NameScope {
    pub entries: Vec<RangeEntry>,
}

/// What a reference resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// Emit `Column { table: qualifier, name }`.
    Resolved { qualifier: Option<String> },
    /// 42703.
    UndefinedColumn,
    /// 42P01.
    MissingFromClauseEntry,
    /// 42712.
    DuplicateAlias,
    /// 42702.
    Ambiguous,
    /// No scope on this planning path: lower the reference as before.
    Unscoped,
}

/// Outcome of looking a QUALIFIER up (no column involved).
#[derive(Debug, Clone)]
pub(crate) enum EntryLookup {
    Unscoped,
    Missing,
    Duplicate,
    /// `level` indexes the stack (0 = outermost), `position` the entry within
    /// that level's FROM order.
    Found {
        level: usize,
        position: usize,
    },
}

/// The stack of query levels, innermost LAST. Held by the planner in a
/// `RefCell` (the idiom `cte_schemas` / `named_windows` already use) so no
/// signature has to change across the planner's expression call sites.
#[derive(Debug, Default)]
pub(crate) struct ScopeStack {
    levels: Vec<NameScope>,
}

impl ScopeStack {
    pub(crate) fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    pub(crate) fn push(&mut self) {
        self.levels.push(NameScope::default());
    }

    pub(crate) fn pop(&mut self) {
        self.levels.pop();
    }

    /// Forget the innermost level's entries (each side of a UNION sees only
    /// its own FROM). No-op when nothing is pushed.
    pub(crate) fn clear_top(&mut self) {
        if let Some(top) = self.levels.last_mut() {
            top.entries.clear();
        }
    }

    /// Add an entry to the innermost level. No-op when nothing is pushed.
    pub(crate) fn record(&mut self, entry: RangeEntry) {
        if let Some(top) = self.levels.last_mut() {
            top.entries.push(entry);
        }
    }

    pub(crate) fn entry(&self, level: usize, position: usize) -> Option<&RangeEntry> {
        self.levels.get(level).and_then(|scope| scope.entries.get(position))
    }

    /// Resolve a qualifier, innermost level first, walking outward for
    /// correlated references. Within one level an entry's own names win; the
    /// lenient real-table names are consulted only when no own name matched.
    pub(crate) fn find_entry(&self, qualifier: &str) -> EntryLookup {
        if self.levels.is_empty() {
            return EntryLookup::Unscoped;
        }
        for (level, scope) in self.levels.iter().enumerate().rev() {
            let own: Vec<usize> = scope
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.names_contain(qualifier))
                .map(|(position, _)| position)
                .collect();
            match (own.len(), own.first()) {
                (0, _) => {}
                (1, Some(position)) => {
                    return EntryLookup::Found {
                        level,
                        position: *position,
                    }
                }
                _ => return EntryLookup::Duplicate,
            }
            let lenient: Vec<usize> = scope
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.lenient_names_contain(qualifier))
                .map(|(position, _)| position)
                .collect();
            match (lenient.len(), lenient.first()) {
                (0, _) => {}
                (1, Some(position)) => {
                    return EntryLookup::Found {
                        level,
                        position: *position,
                    }
                }
                // Two aliased entries over the same real table: the real name
                // is not usable, exactly as in PostgreSQL (42P01).
                _ => return EntryLookup::Missing,
            }
        }
        EntryLookup::Missing
    }

    /// First candidate spelling that names an entry (or is duplicated);
    /// `Missing` when none does. Lets a wildcard accept both `t.*` and
    /// `public.t.*` without guessing.
    pub(crate) fn find_entry_any(&self, candidates: &[String]) -> EntryLookup {
        if self.levels.is_empty() {
            return EntryLookup::Unscoped;
        }
        for candidate in candidates {
            match self.find_entry(candidate) {
                EntryLookup::Missing => continue,
                other => return other,
            }
        }
        EntryLookup::Missing
    }

    /// Would rewriting a reference to entry `(level, position)` into the
    /// UNQUALIFIED `column` be resolvable by name alone at runtime? False when
    /// any other entry at the same level, or any entry at an inner level (a
    /// correlated reference is resolved inner-first by the executor), carries
    /// `column`. Position-independent on purpose: `JoinReorderingRule` may
    /// swap the sides of an inner join after planning.
    pub(crate) fn unqualified_is_unique(&self, level: usize, position: usize, column: &str) -> bool {
        for (l, scope) in self.levels.iter().enumerate().skip(level) {
            for (p, entry) in scope.entries.iter().enumerate() {
                if l == level && p == position {
                    continue;
                }
                if entry.has_column(column) {
                    return false;
                }
            }
        }
        true
    }

    /// Resolve `qualifier.column` / `column` against the stack.
    pub(crate) fn resolve(&self, qualifier: Option<&str>, column: &str, extra_names: &[String]) -> Resolution {
        if self.levels.is_empty() {
            return Resolution::Unscoped;
        }
        match qualifier {
            Some(q) => match self.find_entry(q) {
                EntryLookup::Unscoped => Resolution::Unscoped,
                EntryLookup::Missing => Resolution::MissingFromClauseEntry,
                EntryLookup::Duplicate => Resolution::DuplicateAlias,
                EntryLookup::Found { level, position } => {
                    let Some(entry) = self.entry(level, position) else {
                        return Resolution::MissingFromClauseEntry;
                    };
                    if !entry.has_column(column) {
                        return Resolution::UndefinedColumn;
                    }
                    if entry.duplicate_is_ambiguous(column) {
                        return Resolution::Ambiguous;
                    }
                    match entry.runtime_qualifier.as_ref() {
                        Some(rq) => Resolution::Resolved {
                            qualifier: Some(rq.clone()),
                        },
                        None => {
                            if self.unqualified_is_unique(level, position, column) {
                                Resolution::Resolved { qualifier: None }
                            } else {
                                Resolution::Ambiguous
                            }
                        }
                    }
                }
            },
            None => {
                if extra_names.iter().any(|n| n == column) {
                    return Resolution::Resolved { qualifier: None };
                }
                for scope in self.levels.iter().rev() {
                    if let Some(entry) = scope.entries.iter().find(|e| e.has_column(column)) {
                        // The entry's select list NAMES the column twice (a
                        // sub-select whose output repeats it): the bare
                        // spelling is as ambiguous as the qualified one —
                        // PostgreSQL 42702, never the first slot. A duplicate
                        // a WILDCARD expanded is our own un-merged output and
                        // resolves to the first slot (c11, M1).
                        if entry.duplicate_is_ambiguous(column) {
                            return Resolution::Ambiguous;
                        }
                        return Resolution::Resolved { qualifier: None };
                    }
                }
                Resolution::UndefinedColumn
            }
        }
    }
}

/// Pops the level it pushed when dropped, so `?` early returns and panics
/// unwind the stack cleanly. `active == false` pushed nothing (catalog-less
/// planner) and pops nothing.
pub(crate) struct ScopeGuard<'s> {
    stack: &'s RefCell<ScopeStack>,
    active: bool,
}

impl<'s> ScopeGuard<'s> {
    pub(crate) fn push(stack: &'s RefCell<ScopeStack>, active: bool) -> Self {
        if active {
            stack.borrow_mut().push();
        }
        Self { stack, active }
    }
}

impl Drop for ScopeGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.stack.borrow_mut().pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, DataType};

    fn schema(cols: &[&str]) -> Arc<Schema> {
        Arc::new(Schema::new(
            cols.iter().map(|c| Column::new(*c, DataType::Int4)).collect(),
        ))
    }

    fn base(alias: Option<&str>, table: &str, cols: &[&str]) -> RangeEntry {
        let names = match alias {
            Some(a) => vec![a.to_string()],
            None => vec![table.to_string()],
        };
        let lenient = match alias {
            Some(_) => vec![table.to_string()],
            None => vec![],
        };
        RangeEntry::new(names, lenient, Some(alias.unwrap_or(table).to_string()), schema(cols))
    }

    fn derived(alias: &str, cols: &[&str]) -> RangeEntry {
        RangeEntry::new(vec![alias.to_string()], vec![], None, schema(cols))
    }

    #[test]
    fn empty_stack_is_unscoped() {
        let stack = ScopeStack::default();
        assert_eq!(stack.resolve(Some("t"), "id", &[]), Resolution::Unscoped);
        assert_eq!(stack.resolve(None, "id", &[]), Resolution::Unscoped);
    }

    #[test]
    fn alias_resolves_to_runtime_qualifier_and_real_name_is_lenient() {
        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(base(Some("t1"), "t", &["id", "v"]));
        assert_eq!(
            stack.resolve(Some("t1"), "id", &[]),
            Resolution::Resolved {
                qualifier: Some("t1".to_string())
            }
        );
        assert_eq!(
            stack.resolve(Some("t"), "id", &[]),
            Resolution::Resolved {
                qualifier: Some("t1".to_string())
            }
        );
        assert_eq!(stack.resolve(Some("t1"), "nope", &[]), Resolution::UndefinedColumn);
        assert_eq!(
            stack.resolve(Some("T1"), "id", &[]),
            Resolution::MissingFromClauseEntry,
            "case is exact: \"T1\" is not the alias t1"
        );
        assert_eq!(
            stack.resolve(Some("bogus"), "id", &[]),
            Resolution::MissingFromClauseEntry
        );
    }

    #[test]
    fn unqualified_reference_uses_scope_then_extra_names() {
        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(base(None, "t", &["id"]));
        assert_eq!(stack.resolve(None, "id", &[]), Resolution::Resolved { qualifier: None });
        assert_eq!(stack.resolve(None, "nosuch", &[]), Resolution::UndefinedColumn);
        assert_eq!(
            stack.resolve(None, "total", &["total".to_string()]),
            Resolution::Resolved { qualifier: None }
        );
    }

    #[test]
    fn duplicate_alias_and_shared_real_name() {
        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(base(Some("a"), "t", &["id"]));
        stack.record(base(Some("b"), "t", &["id"]));
        assert_eq!(
            stack.resolve(Some("t"), "id", &[]),
            Resolution::MissingFromClauseEntry,
            "the real name of two aliased entries is not usable"
        );
        stack.record(base(Some("a"), "u", &["id"]));
        assert_eq!(stack.resolve(Some("a"), "id", &[]), Resolution::DuplicateAlias);
    }

    #[test]
    fn derived_table_rewrites_to_unqualified_unless_shadowed() {
        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(derived("s", &["x"]));
        assert_eq!(
            stack.resolve(Some("s"), "x", &[]),
            Resolution::Resolved { qualifier: None }
        );
        stack.record(base(None, "t", &["x", "id"]));
        assert_eq!(
            stack.resolve(Some("s"), "x", &[]),
            Resolution::Ambiguous,
            "another entry at the same level carries x"
        );
        // An inner level shadows an outer derived table too.
        stack.push();
        stack.record(base(None, "u", &["x"]));
        assert_eq!(stack.resolve(Some("s"), "x", &[]), Resolution::Ambiguous);
    }

    /// GH#29 (c11, M1). A name the entry carries TWICE is 42702 only when the
    /// entry's select list NAMED both of them. The SAME schema, marked
    /// wildcard-expanded, resolves to the first slot instead — that is the
    /// whole rule, and it is decided here, not by the shape of the join.
    #[test]
    fn a_duplicate_is_ambiguous_only_when_the_select_list_wrote_it_twice() {
        let written = derived("s", &["id", "a", "id", "b"]);
        let expanded = derived("j", &["id", "a", "id", "b"]).with_wildcard_output(true);
        assert!(written.has_duplicate_column("id"));
        assert!(expanded.has_duplicate_column("id"), "the SCHEMA is identical");
        assert!(written.duplicate_is_ambiguous("id"));
        assert!(
            !expanded.duplicate_is_ambiguous("id"),
            "a wildcard-expanded duplicate is our own un-merged output, not two named columns"
        );
        // A name carried ONCE is never ambiguous either way.
        assert!(!written.duplicate_is_ambiguous("a"));
        assert!(!expanded.duplicate_is_ambiguous("a"));

        // …and through `resolve`, which is what the planner calls.
        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(written);
        assert_eq!(stack.resolve(Some("s"), "id", &[]), Resolution::Ambiguous);
        assert_eq!(stack.resolve(None, "id", &[]), Resolution::Ambiguous);

        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(expanded);
        assert_eq!(
            stack.resolve(Some("j"), "id", &[]),
            Resolution::Resolved { qualifier: None },
            "an UNALIASED sub-select has no runtime qualifier; the first slot is read by bare name"
        );
        assert_eq!(stack.resolve(None, "id", &[]), Resolution::Resolved { qualifier: None });
    }

    /// The same, for a STAMPED entry (a CTE, view or aliased derived table):
    /// the reference keeps its runtime qualifier and the executor's qualified
    /// lookup answers with the first slot of that name.
    #[test]
    fn a_wildcard_expanded_duplicate_resolves_through_a_runtime_qualifier() {
        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(
            RangeEntry::new(
                vec!["j".to_string()],
                vec![],
                Some("j".to_string()),
                schema(&["id", "a", "id", "b"]),
            )
            .with_wildcard_output(true),
        );
        assert_eq!(
            stack.resolve(Some("j"), "id", &[]),
            Resolution::Resolved {
                qualifier: Some("j".to_string())
            }
        );
        assert_eq!(stack.resolve(Some("j"), "nope", &[]), Resolution::UndefinedColumn);
    }

    #[test]
    fn correlated_reference_walks_outward() {
        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(base(Some("o"), "t", &["id"]));
        stack.push();
        stack.record(base(Some("i"), "t", &["id"]));
        assert_eq!(
            stack.resolve(Some("o"), "id", &[]),
            Resolution::Resolved {
                qualifier: Some("o".to_string())
            }
        );
        assert_eq!(
            stack.resolve(Some("i"), "id", &[]),
            Resolution::Resolved {
                qualifier: Some("i".to_string())
            }
        );
    }

    #[test]
    fn opaque_entry_accepts_any_column() {
        let mut stack = ScopeStack::default();
        stack.push();
        stack.record(base(None, "sysview", &[]));
        assert_eq!(
            stack.resolve(Some("sysview"), "anything", &[]),
            Resolution::Resolved {
                qualifier: Some("sysview".to_string())
            }
        );
        assert_eq!(
            stack.resolve(None, "anything", &[]),
            Resolution::Resolved { qualifier: None }
        );
    }

    #[test]
    fn guard_pops_on_drop() {
        let cell = RefCell::new(ScopeStack::default());
        {
            let _guard = ScopeGuard::push(&cell, true);
            assert!(!cell.borrow().is_empty());
            let _inactive = ScopeGuard::push(&cell, false);
            assert_eq!(cell.borrow().levels.len(), 1);
        }
        assert!(cell.borrow().is_empty());
    }

    #[test]
    fn error_wording_carries_the_classifier_shapes() {
        let col = undefined_column(Some("q"), "c").to_string();
        assert!(col.contains("Column \"q\".\"c\" does not exist"), "{col}");
        let rel = missing_from_clause_entry("q").to_string();
        assert!(rel.contains("relation \"q\" does not exist"), "{rel}");
        assert!(rel.contains(MISSING_FROM_CLAUSE_ENTRY), "{rel}");
        assert!(!rel.to_ascii_lowercase().contains("column \""), "{rel}");
        let dup = duplicate_range_entry("a").to_string();
        assert!(dup.contains("table name \"a\" specified more than once"), "{dup}");
        assert!(is_duplicate_range_entry(&dup), "{dup}");
        // GH#29 c2 (4c): the CTAS duplicate-column refusal shares the trailing
        // phrase and must NOT be taken for a duplicate alias.
        assert!(!is_duplicate_range_entry("column \"x\" specified more than once"));
        let long = derived_column_list_too_long("s", 1, 2).to_string();
        assert!(long.contains(DERIVED_COLUMN_LIST_TOO_LONG), "{long}");
        let mv = materialized_view_derived_alias_shadowed("s", "id").to_string();
        assert!(mv.contains(MATERIALIZED_VIEW_DERIVED_ALIAS_UNSUPPORTED), "{mv}");
        assert!(mv.contains("alias the column inside the sub-select"), "{mv}");
        let amb = ambiguous_column("c").to_string();
        assert!(amb.contains(AMBIGUOUS_COLUMN_REFERENCE), "{amb}");
    }
}
