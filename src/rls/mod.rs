//! Row-level security.
//!
//! PostgreSQL's model: a table carries an ENABLE / FORCE switch and a set of
//! policies; a statement run by a role that is not exempt sees and writes only
//! the rows its policies allow. Everything lives with the database:
//!
//! * [`record`]: the stored records (policy, table switches, table owner,
//!   tenant) and their key families;
//! * [`store`]: the catalog of those records, loaded from the storage engine
//!   and refreshed whenever the schema generation moves (so a physical standby
//!   that applies the primary's bytes sees the same policies);
//! * [`command`]: `CREATE / ALTER / DROP POLICY` and the `ALTER TABLE …
//!   ROW LEVEL SECURITY` switches, from the parsed statement to the stored
//!   record;
//! * [`decide`]: who the statement runs as, and what that means for one table
//!   and one command (nothing, every row refused, or a predicate);
//! * [`rewrite`]: the plan rewrite for reads and the row checks for writes;
//! * [`derived`]: when a policy can be applied to the stored rows of a
//!   materialized view, and the reason when it cannot;
//! * [`catalog`]: `pg_policies`, `pg_policy` and the `pg_class` flags;
//! * [`dump`]: the statements a dump carries.
//!
//! The tenant model (`crate::tenant`) is a user of the same store: a policy
//! made through `TenantManager::create_rls_policy` or `\tenant rls create` is
//! a stored policy.

pub mod catalog;
pub mod command;
pub mod decide;
pub mod derived;
pub mod dump;
pub mod record;
pub mod rewrite;
pub mod store;

pub use command::RlsCommand;
pub use decide::{Decision, RlsIdentity};
pub use record::{PolicyCommand, PolicyRecord, TableSecurity};
pub use store::PolicyStore;

/// The SQLSTATE of an error this module raises as `Error::QueryExecution`,
/// by its wording. (`42501` errors are `Error::ConstraintViolation` with
/// "row-level security" in the text, which the wire already maps.)
pub fn sqlstate_for_message(message: &str) -> Option<&'static str> {
    if message.starts_with("policy \"") && message.ends_with("already exists") {
        return Some("42710");
    }
    if message.starts_with("policy \"") && message.ends_with("does not exist") {
        return Some("42704");
    }
    if message.starts_with(decide::INFINITE_RECURSION) {
        return Some("42P17");
    }
    if message.contains(UNSUPPORTED_MARKER) {
        return Some("0A000");
    }
    None
}

/// Part of every "not supported" refusal of this module (SQLSTATE 0A000).
pub const UNSUPPORTED_MARKER: &str = "is not supported with row-level security";
