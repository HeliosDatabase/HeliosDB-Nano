//! The stored records of row-level security and their keys.
//!
//! ```text
//! meta:policy:{table-id}:{policy}   -> PolicyRecord
//! meta:rowsec:{table-id}            -> TableSecurity   (ENABLE / FORCE)
//! meta:relowner:{table-id}          -> role name       (who created the table)
//! meta:tenant:{uuid}                -> TenantRecord
//! ```
//!
//! `{table-id}` is the table's incarnation id as 16 hex digits: the id a
//! `CREATE TABLE` mints, `RENAME` moves and `DROP` deletes. A policy therefore
//! follows its table through a rename, and a table created later under a
//! dropped table's name does not inherit the dropped table's policies. A policy
//! made through the library API for a name that is not a table (yet) is stored
//! under `@{name}` and applies to whichever table has that name.
//!
//! Every value is a four-byte magic, a format version byte and a JSON body, so
//! a later format is detected and a field can be added without rewriting the
//! records already on disk.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Key family of the policies.
pub const POLICY_PREFIX: &str = "meta:policy:";
/// Key family of the per-table ENABLE / FORCE switches.
pub const ROWSEC_PREFIX: &str = "meta:rowsec:";
/// Key family of the table owners.
pub const OWNER_PREFIX: &str = "meta:relowner:";
/// Key family of the tenant registry.
pub const TENANT_PREFIX: &str = "meta:tenant:";

const POLICY_MAGIC: &[u8; 4] = b"RLP1";
const ROWSEC_MAGIC: &[u8; 4] = b"RLT1";
const TENANT_MAGIC: &[u8; 4] = b"TNT1";
const FORMAT_VERSION: u8 = 1;

/// The command a policy is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicyCommand {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl PolicyCommand {
    /// The keyword, as `pg_policies.cmd` shows it.
    pub fn keyword(self) -> &'static str {
        match self {
            PolicyCommand::All => "ALL",
            PolicyCommand::Select => "SELECT",
            PolicyCommand::Insert => "INSERT",
            PolicyCommand::Update => "UPDATE",
            PolicyCommand::Delete => "DELETE",
        }
    }

    /// The letter `pg_policy.polcmd` holds.
    pub fn letter(self) -> &'static str {
        match self {
            PolicyCommand::All => "*",
            PolicyCommand::Select => "r",
            PolicyCommand::Insert => "a",
            PolicyCommand::Update => "w",
            PolicyCommand::Delete => "d",
        }
    }

    /// Does a policy for `self` govern the statement kind `cmd`?
    pub fn governs(self, cmd: PolicyCommand) -> bool {
        self == PolicyCommand::All || self == cmd
    }

    /// From a statement keyword (`"select"`, `"INSERT"`, …).
    pub fn from_keyword(cmd: &str) -> Option<Self> {
        if cmd.eq_ignore_ascii_case("SELECT") {
            Some(PolicyCommand::Select)
        } else if cmd.eq_ignore_ascii_case("INSERT") {
            Some(PolicyCommand::Insert)
        } else if cmd.eq_ignore_ascii_case("UPDATE") {
            Some(PolicyCommand::Update)
        } else if cmd.eq_ignore_ascii_case("DELETE") {
            Some(PolicyCommand::Delete)
        } else if cmd.eq_ignore_ascii_case("ALL") {
            Some(PolicyCommand::All)
        } else {
            None
        }
    }
}

/// One policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecord {
    /// Policy name, unique per table.
    pub name: String,
    /// The table's name when the policy was written (the table is identified
    /// by the key; this is what a reader of the record sees).
    pub table: String,
    /// PERMISSIVE (the default) or RESTRICTIVE.
    pub permissive: bool,
    /// The command it governs.
    pub command: PolicyCommand,
    /// `TO` roles; empty means PUBLIC.
    #[serde(default)]
    pub roles: Vec<String>,
    /// `USING (…)`, as SQL text.
    #[serde(default)]
    pub using: Option<String>,
    /// `WITH CHECK (…)`, as SQL text.
    #[serde(default)]
    pub with_check: Option<String>,
    /// A stable number for `pg_policy.oid`.
    #[serde(default)]
    pub oid: u32,
}

impl PolicyRecord {
    /// Does the policy name `role` (or everyone)?
    pub fn applies_to_role(&self, role: Option<&str>) -> bool {
        if self.roles.is_empty() {
            return true;
        }
        self.roles
            .iter()
            .any(|r| r.eq_ignore_ascii_case("public") || role.is_some_and(|role| role == r))
    }
}

/// A table's row-security switches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableSecurity {
    /// `ENABLE ROW LEVEL SECURITY`.
    pub enabled: bool,
    /// `FORCE ROW LEVEL SECURITY`: the owner is subject too.
    pub forced: bool,
}

/// A tenant, as the registry stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantRecord {
    pub id: String,
    pub name: String,
    pub plan_id: String,
    /// `shared_schema`, `database_per_tenant` or `schema_per_tenant`.
    pub isolation_mode: String,
    pub max_storage_bytes: u64,
    pub max_connections: u64,
    pub max_qps: u64,
    pub rls_enabled: bool,
    pub created_at: String,
}

/// Which table a policy key names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TableId {
    /// The table with this incarnation id.
    Incarnation(u64),
    /// Whichever table has this name.
    Name(String),
}

impl TableId {
    fn segment(&self) -> String {
        match self {
            TableId::Incarnation(id) => format!("{id:016x}"),
            TableId::Name(name) => format!("@{name}"),
        }
    }

    fn parse(segment: &str) -> Option<Self> {
        if let Some(name) = segment.strip_prefix('@') {
            return (!name.is_empty()).then(|| TableId::Name(name.to_string()));
        }
        if segment.len() == 16 {
            return u64::from_str_radix(segment, 16).ok().map(TableId::Incarnation);
        }
        None
    }
}

/// `meta:policy:{table-id}:{policy}`.
pub fn policy_key(table: &TableId, policy: &str) -> Vec<u8> {
    format!("{POLICY_PREFIX}{}:{policy}", table.segment()).into_bytes()
}

/// The table and policy a `meta:policy:` key suffix (the part after the
/// prefix) names. A table name holds no `:`, so the first one ends the table.
pub fn split_policy_suffix(suffix: &str) -> Option<(TableId, String)> {
    let (table, policy) = suffix.split_once(':')?;
    if policy.is_empty() {
        return None;
    }
    Some((TableId::parse(table)?, policy.to_string()))
}

/// `meta:rowsec:{table-id}`.
pub fn rowsec_key(incarnation: u64) -> Vec<u8> {
    format!("{ROWSEC_PREFIX}{incarnation:016x}").into_bytes()
}

/// `meta:relowner:{table-id}`.
pub fn owner_key(incarnation: u64) -> Vec<u8> {
    format!("{OWNER_PREFIX}{incarnation:016x}").into_bytes()
}

/// `meta:tenant:{uuid}`.
pub fn tenant_key(id: &str) -> Vec<u8> {
    format!("{TENANT_PREFIX}{id}").into_bytes()
}

/// The incarnation id in a `meta:rowsec:` / `meta:relowner:` key suffix.
pub fn parse_incarnation(suffix: &str) -> Option<u64> {
    (suffix.len() == 16)
        .then(|| u64::from_str_radix(suffix, 16).ok())
        .flatten()
}

fn encode<T: Serialize>(magic: &[u8; 4], value: &T, what: &str) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(value).map_err(|e| Error::storage(format!("cannot encode a {what} record: {e}")))?;
    let mut out = Vec::with_capacity(magic.len() + 1 + body.len());
    out.extend_from_slice(magic);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Loud on purpose: a record that cannot be read is an error, never a policy
/// that silently stopped applying.
fn decode<T: for<'de> Deserialize<'de>>(magic: &[u8; 4], bytes: &[u8], what: &str) -> Result<T> {
    let body = bytes
        .strip_prefix(magic.as_slice())
        .ok_or_else(|| Error::storage(format!("{what} record is not in a recognised format")))?;
    let (version, body) = body
        .split_first()
        .ok_or_else(|| Error::storage(format!("{what} record is truncated")))?;
    if *version != FORMAT_VERSION {
        return Err(Error::storage(format!(
            "{what} record has format version {version}, this binary reads version {FORMAT_VERSION}"
        )));
    }
    serde_json::from_slice(body).map_err(|e| Error::storage(format!("cannot decode a {what} record: {e}")))
}

pub fn encode_policy(policy: &PolicyRecord) -> Result<Vec<u8>> {
    encode(POLICY_MAGIC, policy, "row-level security policy")
}

pub fn decode_policy(bytes: &[u8]) -> Result<PolicyRecord> {
    decode(POLICY_MAGIC, bytes, "row-level security policy")
}

pub fn encode_table_security(flags: &TableSecurity) -> Result<Vec<u8>> {
    encode(ROWSEC_MAGIC, flags, "table row-security")
}

pub fn decode_table_security(bytes: &[u8]) -> Result<TableSecurity> {
    decode(ROWSEC_MAGIC, bytes, "table row-security")
}

pub fn encode_tenant(tenant: &TenantRecord) -> Result<Vec<u8>> {
    encode(TENANT_MAGIC, tenant, "tenant")
}

pub fn decode_tenant(bytes: &[u8]) -> Result<TenantRecord> {
    decode(TENANT_MAGIC, bytes, "tenant")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn policy() -> PolicyRecord {
        PolicyRecord {
            name: "own:rows".to_string(),
            table: "docs".to_string(),
            permissive: false,
            command: PolicyCommand::Update,
            roles: vec!["alice".to_string()],
            using: Some("owner = current_user AND a || b <> 'x|y'".to_string()),
            with_check: None,
            oid: 7,
        }
    }

    #[test]
    fn a_policy_record_round_trips() {
        let bytes = encode_policy(&policy()).unwrap();
        assert_eq!(decode_policy(&bytes).unwrap(), policy());
    }

    #[test]
    fn a_record_of_another_version_or_shape_is_an_error() {
        let mut bytes = encode_policy(&policy()).unwrap();
        bytes[4] = 9;
        assert!(decode_policy(&bytes).unwrap_err().to_string().contains("format version 9"));
        assert!(decode_policy(b"junk").is_err());
        assert!(decode_table_security(&encode_policy(&policy()).unwrap()).is_err());
    }

    #[test]
    fn policy_keys_name_the_table_by_id_or_by_name() {
        let by_id = policy_key(&TableId::Incarnation(0xAB), "own:rows");
        assert_eq!(by_id, b"meta:policy:00000000000000ab:own:rows".to_vec());
        let suffix = std::str::from_utf8(&by_id[POLICY_PREFIX.len()..]).unwrap();
        assert_eq!(
            split_policy_suffix(suffix),
            Some((TableId::Incarnation(0xAB), "own:rows".to_string()))
        );
        let by_name = policy_key(&TableId::Name("s.docs".to_string()), "p");
        let suffix = std::str::from_utf8(&by_name[POLICY_PREFIX.len()..]).unwrap();
        assert_eq!(
            split_policy_suffix(suffix),
            Some((TableId::Name("s.docs".to_string()), "p".to_string()))
        );
        assert_eq!(split_policy_suffix("nonsense"), None);
        assert_eq!(parse_incarnation("00000000000000ab"), Some(0xAB));
        assert_eq!(parse_incarnation("ab"), None);
    }

    #[test]
    fn a_policy_names_roles_or_everyone() {
        let mut p = policy();
        assert!(p.applies_to_role(Some("alice")));
        assert!(!p.applies_to_role(Some("bob")));
        assert!(!p.applies_to_role(None));
        p.roles = vec!["PUBLIC".to_string()];
        assert!(p.applies_to_role(Some("bob")));
        p.roles.clear();
        assert!(p.applies_to_role(None));
    }

    #[test]
    fn commands_govern_as_postgres_does() {
        assert!(PolicyCommand::All.governs(PolicyCommand::Delete));
        assert!(PolicyCommand::Select.governs(PolicyCommand::Select));
        assert!(!PolicyCommand::Select.governs(PolicyCommand::Update));
        assert_eq!(PolicyCommand::from_keyword("update"), Some(PolicyCommand::Update));
        assert_eq!(PolicyCommand::from_keyword("merge"), None);
    }
}
