//! Who a statement runs as.
//!
//! Three cases, decided in one place ([`current_principal`]):
//!
//! * [`Principal::Unrestricted`]: an in-process caller with no session identity
//!   (the embedded API, the REPL) and the engine's own background work. The
//!   process owns the data files; checking it would protect nothing.
//! * [`Principal::Anonymous`]: a statement that arrived on a network surface
//!   whose session carries no authenticated role. Refused for everything that
//!   needs a privilege.
//! * [`Principal::Role`]: a named role. Checked.
//!
//! The session's own half of this state ([`SessionAccess`]) lives on
//! `SessionScopedState`, which every statement entry point already installs as
//! a thread-local, so the planner, the executor and the expression evaluator
//! all read the same answer without a session handle.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use crate::storage::StorageEngine;

/// The bootstrap superuser. An object with no recorded owner belongs to it.
pub const BOOTSTRAP_SUPERUSER: &str = "postgres";

/// The role name recorded as grantor for a grant issued without a role
/// identity (the embedded API, an operator account).
pub const SERVICE_GRANTOR: &str = "helios";

const LOGIN_UNCLASSIFIED: u8 = 0;
const LOGIN_CATALOG_ROLE: u8 = 1;
const LOGIN_OPERATOR: u8 = 2;

/// The principal a privilege decision is made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// In-process use with no session identity, and engine-internal work.
    Unrestricted,
    /// A network surface with no authenticated role.
    Anonymous,
    /// A named role.
    Role {
        /// The role's name.
        name: Arc<str>,
        /// The login names no role in the catalog: it is the server's own
        /// account (the account given at start, or a name a trust listener
        /// accepted) and is treated as the bootstrap superuser. Decided once
        /// per login, so dropping a role does not promote its open sessions.
        operator: bool,
    },
}

impl Principal {
    /// The role name, when there is one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Principal::Role { name, .. } => Some(name),
            _ => None,
        }
    }
}

/// Per-session privilege state. One per connection (and one for the embedded
/// handle), reached through `SessionScopedState::access`.
#[derive(Debug, Default)]
pub struct SessionAccess {
    /// The session belongs to a network listener. Without a login identity it
    /// is [`Principal::Anonymous`], never unrestricted.
    network: AtomicBool,
    /// `SET ROLE` target.
    role: parking_lot::RwLock<Option<Arc<str>>>,
    /// `SET SESSION AUTHORIZATION` target.
    authorization: parking_lot::RwLock<Option<Arc<str>>>,
    /// Lock-free mirror of "either override is set".
    overridden: AtomicBool,
    /// Whether the login names a catalog role; see [`Principal::Role`].
    login_kind: AtomicU8,
}

impl SessionAccess {
    /// Mark the session as belonging to a network listener.
    pub fn mark_network(&self) {
        self.network.store(true, Ordering::Release);
    }

    /// True for a session a network listener created.
    pub fn is_network(&self) -> bool {
        self.network.load(Ordering::Acquire)
    }

    /// The login identity changed: classify it again on its next statement,
    /// and drop any role the previous identity had switched to.
    pub fn reset_login(&self) {
        self.login_kind.store(LOGIN_UNCLASSIFIED, Ordering::Release);
        self.clear_overrides();
    }

    /// The `SET ROLE` target, if any.
    pub fn role(&self) -> Option<Arc<str>> {
        if !self.overridden.load(Ordering::Acquire) {
            return None;
        }
        self.role.read().clone()
    }

    /// The `SET SESSION AUTHORIZATION` target, if any.
    pub fn authorization(&self) -> Option<Arc<str>> {
        if !self.overridden.load(Ordering::Acquire) {
            return None;
        }
        self.authorization.read().clone()
    }

    /// True while `SET ROLE` or `SET SESSION AUTHORIZATION` is in effect.
    pub fn is_overridden(&self) -> bool {
        self.overridden.load(Ordering::Acquire)
    }

    pub(crate) fn set_role(&self, role: Option<&str>) {
        *self.role.write() = role.map(Arc::<str>::from);
        self.refresh_overridden();
    }

    pub(crate) fn set_authorization(&self, role: Option<&str>) {
        *self.authorization.write() = role.map(Arc::<str>::from);
        // PostgreSQL: changing the session authorization resets the role.
        *self.role.write() = None;
        self.refresh_overridden();
    }

    fn clear_overrides(&self) {
        *self.role.write() = None;
        *self.authorization.write() = None;
        self.overridden.store(false, Ordering::Release);
    }

    fn refresh_overridden(&self) {
        let any = self.role.read().is_some() || self.authorization.read().is_some();
        self.overridden.store(any, Ordering::Release);
    }

    /// Whether `login` is an operator account (names no catalog role). Decided
    /// on the first statement after login and remembered.
    fn login_is_operator(&self, login: &str, storage: &StorageEngine) -> bool {
        match self.login_kind.load(Ordering::Acquire) {
            LOGIN_CATALOG_ROLE => false,
            LOGIN_OPERATOR => true,
            _ => {
                let operator = !names_a_role(storage, login);
                let kind = if operator { LOGIN_OPERATOR } else { LOGIN_CATALOG_ROLE };
                self.login_kind.store(kind, Ordering::Release);
                operator
            }
        }
    }
}

/// True if `name` is a built-in role or a role in the catalog. A catalog that
/// cannot be read answers true, so the name is checked as an ordinary role
/// (and denied) instead of being promoted.
fn names_a_role(storage: &StorageEngine, name: &str) -> bool {
    if crate::storage::is_reserved_role_name(name) {
        return true;
    }
    storage.catalog().role_exists(name).unwrap_or(true)
}

/// The session's identity before `SET ROLE`: what `session_user` answers and
/// what decides which roles the session may switch to.
pub fn session_principal(storage: &StorageEngine) -> Principal {
    let scoped = crate::session_scoped_state_tls();
    let access = scoped.as_ref().map(|state| state.access());
    if let Some(authorization) = access.and_then(SessionAccess::authorization) {
        return Principal::Role {
            name: authorization,
            operator: false,
        };
    }
    login_principal(storage, access)
}

/// The authenticated identity, ignoring `SET ROLE` and
/// `SET SESSION AUTHORIZATION`.
pub fn login_principal(storage: &StorageEngine, access: Option<&SessionAccess>) -> Principal {
    match crate::session_login_user_tls() {
        Some(name) => {
            let operator = match access {
                Some(access) => access.login_is_operator(&name, storage),
                None => !names_a_role(storage, &name),
            };
            Principal::Role { name, operator }
        }
        None if access.is_some_and(SessionAccess::is_network) => Principal::Anonymous,
        None => Principal::Unrestricted,
    }
}

/// Who the statement running on this thread is checked as.
pub fn current_principal(storage: &StorageEngine) -> Principal {
    let scoped = crate::session_scoped_state_tls();
    let access = scoped.as_ref().map(|state| state.access());
    if let Some(access) = access {
        if access.is_overridden() {
            if let Some(role) = access.role() {
                return Principal::Role {
                    name: role,
                    operator: false,
                };
            }
            if let Some(authorization) = access.authorization() {
                return Principal::Role {
                    name: authorization,
                    operator: false,
                };
            }
        }
    }
    login_principal(storage, access)
}

/// True when nothing on this thread names a role or a network session: the
/// statement is unrestricted and no catalog needs to be read. The cheap first
/// question every entry point asks.
pub fn nothing_to_check() -> bool {
    if !crate::session_login_user_is(None) {
        return false;
    }
    // Borrowed, not cloned: this runs per statement and per sequence call.
    crate::SESSION_SCOPED_STATE.with(|slot| match slot.borrow().as_ref() {
        Some(state) => {
            let access = state.access();
            !access.is_network() && !access.is_overridden()
        }
        None => true,
    })
}

/// What `current_user` / `current_role` / `user` answer.
pub fn current_user_name() -> String {
    let scoped = crate::session_scoped_state_tls();
    if let Some(access) = scoped.as_ref().map(|state| state.access()) {
        if let Some(role) = access.role().or_else(|| access.authorization()) {
            return role.to_string();
        }
    }
    crate::session_login_user_tls().map_or_else(|| "heliosdb".to_string(), |login| login.to_string())
}

/// What `session_user` and `current_setting('session_authorization')` answer.
pub fn session_user_name() -> String {
    let scoped = crate::session_scoped_state_tls();
    if let Some(authorization) = scoped.as_ref().and_then(|state| state.access().authorization()) {
        return authorization.to_string();
    }
    crate::session_login_user_tls().map_or_else(|| "heliosdb".to_string(), |login| login.to_string())
}

/// True while the running statement's role differs from its login: the shared
/// result cache, which tags entries by login, is bypassed.
pub fn role_override_active() -> bool {
    crate::SESSION_SCOPED_STATE.with(|slot| slot.borrow().as_ref().is_some_and(|state| state.access().is_overridden()))
}
