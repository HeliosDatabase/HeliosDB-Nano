//! Who a statement runs as, and which policies of a table that makes apply.
//!
//! PostgreSQL's rules:
//!
//! * a superuser and a role with BYPASSRLS are exempt from every policy;
//! * the table's owner is exempt unless the table is `FORCE`d;
//! * with row security enabled and no policy for the command and the role,
//!   nothing is visible and nothing may be written (default deny);
//! * permissive policies are OR'd, restrictive policies are AND'd with that;
//!   with no permissive policy nothing passes, whatever the restrictive say.
//!
//! One rule is Nano's own and predates the SQL statements: a session bound to
//! a tenant whose row security is on is subject to the policies of every table
//! that has any, whether or not the table was `ENABLE`d and whatever its role.
//! For such a table a command no policy speaks about is left alone (it is not
//! default deny), exactly as before.

use std::sync::Arc;

use super::record::{PolicyCommand, PolicyRecord};
use super::store::{PolicyStore, TableEntry};
use crate::tenant::TenantContext;
use crate::Result;

/// Start of the error for a policy that reads its own table.
pub const INFINITE_RECURSION: &str = "infinite recursion detected in policy for relation";

/// Who a statement runs as, for row-level security. Taken once and passed
/// down, so every decision of one rewrite is made for the same principal.
#[derive(Debug, Clone)]
pub struct RlsIdentity {
    /// The login role; `None` for the embedded API and the shell, which run
    /// as the bootstrap superuser.
    pub role: Option<Arc<str>>,
    /// No role, a bootstrap role, SUPERUSER or BYPASSRLS.
    pub exempt: bool,
    /// The tenant the session is bound to.
    pub tenant: Option<TenantContext>,
    /// That tenant has row security on.
    pub tenant_rls: bool,
}

impl RlsIdentity {
    /// A value that differs whenever two identities could get different rows:
    /// what a cache of filtered plans or rows must be keyed by.
    pub fn cache_salt(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.role.as_deref().hash(&mut hasher);
        self.exempt.hash(&mut hasher);
        self.tenant_rls.hash(&mut hasher);
        match &self.tenant {
            Some(context) => {
                context.tenant_id.hash(&mut hasher);
                context.user_id.hash(&mut hasher);
            }
            None => 0u8.hash(&mut hasher),
        }
        hasher.finish()
    }

    /// Is this identity subject to no policy of any table?
    pub fn exempt_everywhere(&self) -> bool {
        self.exempt && !self.tenant_rls
    }
}

/// The policies that apply to one command on one table for one identity.
#[derive(Debug, Clone)]
pub struct Applicable {
    pub entry: Arc<TableEntry>,
    /// OR'd. Empty: nothing passes.
    pub permissive: Vec<Arc<PolicyRecord>>,
    /// AND'd with the permissive result.
    pub restrictive: Vec<Arc<PolicyRecord>>,
    /// The table's own switch makes them apply (PostgreSQL's rule), not only
    /// the tenant binding.
    pub by_table_switch: bool,
}

impl Applicable {
    /// The names of the policies, for a message.
    pub fn policy_names(&self) -> Vec<String> {
        self.permissive
            .iter()
            .chain(&self.restrictive)
            .map(|p| p.name.clone())
            .collect()
    }
}

/// What row-level security means for one command on one table.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Nothing: the statement runs unfiltered.
    Off,
    /// These policies apply.
    Apply(Applicable),
}

impl PolicyStore {
    /// See the module documentation.
    pub fn decide(&self, table: &str, command: PolicyCommand, identity: &RlsIdentity) -> Result<Decision> {
        if !self.active() {
            return Ok(Decision::Off);
        }
        let Some(entry) = self.table(table)? else {
            return Ok(Decision::Off);
        };
        let role = identity.role.as_deref();
        let owner_exempt =
            !entry.security.forced && role.is_some() && entry.owner.as_deref() == role;
        let by_table_switch = entry.security.enabled && !identity.exempt && !owner_exempt;
        let by_tenant = identity.tenant_rls && !entry.policies.is_empty();
        if !by_table_switch && !by_tenant {
            return Ok(Decision::Off);
        }
        let mut permissive = Vec::new();
        let mut restrictive = Vec::new();
        for policy in &entry.policies {
            if !policy.command.governs(command) || !policy.applies_to_role(role) {
                continue;
            }
            if policy.permissive {
                permissive.push(Arc::clone(policy));
            } else {
                restrictive.push(Arc::clone(policy));
            }
        }
        if permissive.is_empty() && restrictive.is_empty() && !by_table_switch {
            return Ok(Decision::Off);
        }
        Ok(Decision::Apply(Applicable {
            entry,
            permissive,
            restrictive,
            by_table_switch,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::record::TableSecurity;
    use super::*;

    fn policy(name: &str, permissive: bool, command: PolicyCommand, roles: &[&str]) -> PolicyRecord {
        PolicyRecord {
            name: name.to_string(),
            table: "t".to_string(),
            permissive,
            command,
            roles: roles.iter().map(|r| (*r).to_string()).collect(),
            using: Some("true".to_string()),
            with_check: None,
            oid: 0,
        }
    }

    fn role(name: &str) -> RlsIdentity {
        RlsIdentity {
            role: Some(Arc::from(name)),
            exempt: false,
            tenant: None,
            tenant_rls: false,
        }
    }

    fn enabled(store: &PolicyStore, forced: bool) {
        store
            .set_table_security("t", TableSecurity { enabled: true, forced })
            .unwrap();
    }

    fn names(decision: &Decision) -> (Vec<String>, Vec<String>) {
        match decision {
            Decision::Off => panic!("expected policies to apply"),
            Decision::Apply(a) => (
                a.permissive.iter().map(|p| p.name.clone()).collect(),
                a.restrictive.iter().map(|p| p.name.clone()).collect(),
            ),
        }
    }

    #[test]
    fn a_table_without_the_switch_is_left_alone() {
        let store = PolicyStore::new();
        store.put_policy("t", policy("p", true, PolicyCommand::All, &[])).unwrap();
        assert!(matches!(
            store.decide("t", PolicyCommand::Select, &role("alice")).unwrap(),
            Decision::Off
        ));
    }

    #[test]
    fn enabled_with_no_policy_denies_by_default() {
        let store = PolicyStore::new();
        enabled(&store, false);
        let decision = store.decide("t", PolicyCommand::Select, &role("alice")).unwrap();
        assert_eq!(names(&decision), (vec![], vec![]));
    }

    #[test]
    fn policies_are_chosen_by_command_and_role() {
        let store = PolicyStore::new();
        enabled(&store, false);
        store.put_policy("t", policy("all", true, PolicyCommand::All, &[])).unwrap();
        store.put_policy("t", policy("sel_bob", true, PolicyCommand::Select, &["bob"])).unwrap();
        store.put_policy("t", policy("upd", false, PolicyCommand::Update, &["public"])).unwrap();
        let alice_select = store.decide("t", PolicyCommand::Select, &role("alice")).unwrap();
        assert_eq!(names(&alice_select), (vec!["all".to_string()], vec![]));
        let bob_select = store.decide("t", PolicyCommand::Select, &role("bob")).unwrap();
        assert_eq!(names(&bob_select).0, ["all", "sel_bob"]);
        let alice_update = store.decide("t", PolicyCommand::Update, &role("alice")).unwrap();
        assert_eq!(names(&alice_update), (vec!["all".to_string()], vec!["upd".to_string()]));
    }

    #[test]
    fn an_exempt_role_is_never_subject_to_the_table_switch() {
        let store = PolicyStore::new();
        enabled(&store, true);
        let mut superuser = role("postgres");
        superuser.exempt = true;
        assert!(matches!(
            store.decide("t", PolicyCommand::Select, &superuser).unwrap(),
            Decision::Off
        ));
    }

    #[test]
    fn a_tenant_binding_applies_policies_without_the_switch_and_without_default_deny() {
        let store = PolicyStore::new();
        store.put_policy("t", policy("sel", true, PolicyCommand::Select, &[])).unwrap();
        let bound = RlsIdentity {
            role: None,
            exempt: true,
            tenant: None,
            tenant_rls: true,
        };
        let select = store.decide("t", PolicyCommand::Select, &bound).unwrap();
        assert_eq!(names(&select).0, ["sel"]);
        assert!(matches!(
            store.decide("t", PolicyCommand::Delete, &bound).unwrap(),
            Decision::Off
        ));
        assert!(matches!(
            store.decide("other", PolicyCommand::Select, &bound).unwrap(),
            Decision::Off
        ));
    }

    #[test]
    fn identities_that_can_see_different_rows_have_different_salts() {
        let alice = role("alice");
        let bob = role("bob");
        assert_ne!(alice.cache_salt(), bob.cache_salt());
        assert_eq!(alice.cache_salt(), role("alice").cache_salt());
        let mut exempt_alice = role("alice");
        exempt_alice.exempt = true;
        assert_ne!(alice.cache_salt(), exempt_alice.cache_salt());
    }
}
