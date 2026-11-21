//! The access-control statements: `GRANT`, `REVOKE`, role membership,
//! `SET ROLE`, `SET SESSION AUTHORIZATION`, `ALTER … OWNER TO`,
//! `ALTER ROLE … RENAME TO`.
//!
//! The SQL parser the engine uses has no grammar for most of these (a grant
//! on a function, a schema or a database, a role grant, `GRANTED BY`), so
//! they are parsed here, before it, into an [`AccessStatement`]. The plan
//! carries the statement; [`execute`] runs it, deciding first whether the
//! current role may.

use serde::{Deserialize, Serialize};

use crate::storage::{Catalog, StorageEngine};
use crate::{Error, Result};

use super::access::{
    current_principal, login_principal, session_principal, Principal, BOOTSTRAP_SUPERUSER, SERVICE_GRANTOR,
};
use super::catalog::{
    self, column_object, kind_object, privileges_of, Snapshot, COLUMN, COLUMN_PRIVILEGES, DATABASE, FUNCTION,
    MEMBER, NODEFAULT, PUBLIC, SCHEMA, SEQUENCE, TABLE,
};
use super::gate::{self, denied_to, Gate};

/// Marker of the refusal for `REVOKE … RESTRICT` with dependent grants
/// (SQLSTATE 2BP01).
pub const DEPENDENT_PRIVILEGES_EXIST: &str = "dependent privileges exist";
/// Marker of the refusal for a role grant that would close a cycle (42P17).
pub const ROLE_MEMBERSHIP_CYCLE: &str = "is a member of role";

/// One privilege of a grant, with the columns it is limited to (empty = the
/// whole object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegeSpec {
    /// Upper-case privilege name, or `ALL`.
    pub name: String,
    /// Column names, normalized. Empty for an object-level privilege.
    pub columns: Vec<String>,
}

/// What a grant is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantTarget {
    /// Named objects of one kind (resolved keys).
    Objects {
        /// `table`, `sequence`, `schema`, `function` or `database`.
        kind: String,
        /// Resolved object keys.
        names: Vec<String>,
    },
    /// `ALL TABLES | SEQUENCES | FUNCTIONS IN SCHEMA …`.
    AllInSchema {
        /// `table`, `sequence` or `function`.
        kind: String,
        /// Schema names.
        schemas: Vec<String>,
    },
}

impl GrantTarget {
    fn kind(&self) -> &str {
        match self {
            GrantTarget::Objects { kind, .. } | GrantTarget::AllInSchema { kind, .. } => kind,
        }
    }
}

/// `GRANT privileges ON target TO grantees`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantSpec {
    /// The privileges granted.
    pub privileges: Vec<PrivilegeSpec>,
    /// What they are granted on.
    pub target: GrantTarget,
    /// Role names; `public` for PUBLIC.
    pub grantees: Vec<String>,
    /// `WITH GRANT OPTION`.
    pub with_grant_option: bool,
    /// `GRANTED BY role`.
    pub granted_by: Option<String>,
}

/// `REVOKE privileges ON target FROM grantees`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeSpec {
    /// The privileges revoked.
    pub privileges: Vec<PrivilegeSpec>,
    /// What they are revoked on.
    pub target: GrantTarget,
    /// Role names; `public` for PUBLIC.
    pub grantees: Vec<String>,
    /// `GRANT OPTION FOR`: keep the privilege, remove the right to grant it.
    pub grant_option_only: bool,
    /// `CASCADE`: also revoke what the grantees granted on.
    pub cascade: bool,
    /// `GRANTED BY role`.
    pub granted_by: Option<String>,
}

/// A statement about who may do what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessStatement {
    /// `GRANT … ON …`.
    Grant(GrantSpec),
    /// `REVOKE … ON …`.
    Revoke(RevokeSpec),
    /// `GRANT role TO member`.
    GrantRole {
        /// The roles granted.
        roles: Vec<String>,
        /// The roles receiving them.
        members: Vec<String>,
        /// `WITH ADMIN OPTION`.
        admin_option: bool,
        /// `GRANTED BY role`.
        granted_by: Option<String>,
    },
    /// `REVOKE role FROM member`.
    RevokeRole {
        /// The roles revoked.
        roles: Vec<String>,
        /// The roles losing them.
        members: Vec<String>,
        /// `ADMIN OPTION FOR`: keep the membership, remove the admin option.
        admin_option_only: bool,
    },
    /// `SET ROLE role` (`None`: `SET ROLE NONE` / `RESET ROLE`).
    SetRole(Option<String>),
    /// `SET SESSION AUTHORIZATION role` (`None`: `DEFAULT` / `RESET`).
    SetSessionAuthorization(Option<String>),
    /// `ALTER <kind> name OWNER TO role`.
    AlterOwner {
        /// `table`, `sequence`, `schema`, `function` or `database`.
        kind: String,
        /// Resolved object key.
        name: String,
        /// The word for messages (`table`, `view`, `procedure`, …).
        kind_word: String,
        /// The new owner.
        new_owner: String,
    },
    /// `ALTER ROLE name RENAME TO new_name`.
    RenameRole {
        /// The role to rename.
        name: String,
        /// Its new name.
        new_name: String,
    },
}

// ---------------------------------------------------------------------------
// Parsing.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Quoted(String),
    Punct(char),
}

/// Split a statement into words, quoted names and punctuation. `None` for
/// text that holds anything else (an operator, `@`, `*`): such a statement is
/// not one of ours.
fn tokenize(sql: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut value = String::new();
            loop {
                match chars.next()? {
                    '"' if chars.peek() == Some(&'"') => {
                        chars.next();
                        value.push('"');
                    }
                    '"' => break,
                    other => value.push(other),
                }
            }
            tokens.push(Token::Quoted(value));
        } else if c == '\'' {
            // A quoted role name (`SET ROLE 'app'`).
            chars.next();
            let mut value = String::new();
            loop {
                match chars.next()? {
                    '\'' if chars.peek() == Some(&'\'') => {
                        chars.next();
                        value.push('\'');
                    }
                    '\'' => break,
                    other => value.push(other),
                }
            }
            tokens.push(Token::Quoted(value));
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let mut value = String::new();
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '_' || next == '$' {
                    value.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(Token::Word(value));
        } else if matches!(c, '(' | ')' | ',' | '.' | ';' | '[' | ']') {
            tokens.push(Token::Punct(c));
            chars.next();
        } else {
            return None;
        }
    }
    while tokens.last() == Some(&Token::Punct(';')) {
        tokens.pop();
    }
    Some(tokens)
}

fn syntax(message: impl Into<String>) -> Error {
    Error::SqlParse(message.into())
}

struct Cursor {
    tokens: Vec<Token>,
    position: usize,
}

impl Cursor {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.position + offset)
    }

    fn at_end(&self) -> bool {
        self.position >= self.tokens.len()
    }

    fn is_kw_at(&self, offset: usize, keyword: &str) -> bool {
        matches!(self.peek_at(offset), Some(Token::Word(word)) if word.eq_ignore_ascii_case(keyword))
    }

    fn is_kw(&self, keyword: &str) -> bool {
        self.is_kw_at(0, keyword)
    }

    fn eat_kw(&mut self, keyword: &str) -> bool {
        if self.is_kw(keyword) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn eat_kws(&mut self, keywords: &[&str]) -> bool {
        if keywords
            .iter()
            .enumerate()
            .all(|(offset, keyword)| self.is_kw_at(offset, keyword))
        {
            self.position += keywords.len();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, keyword: &str) -> Result<()> {
        if self.eat_kw(keyword) {
            Ok(())
        } else {
            Err(syntax(format!("expected {keyword}{}", self.found())))
        }
    }

    fn found(&self) -> String {
        match self.peek() {
            Some(Token::Word(word)) => format!(", found \"{word}\""),
            Some(Token::Quoted(word)) => format!(", found \"{word}\""),
            Some(Token::Punct(c)) => format!(", found \"{c}\""),
            None => ", found the end of the statement".to_string(),
        }
    }

    fn eat_punct(&mut self, punct: char) -> bool {
        if self.peek() == Some(&Token::Punct(punct)) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    /// One identifier: unquoted names fold to lower case.
    fn ident(&mut self) -> Result<String> {
        match self.peek().cloned() {
            Some(Token::Word(word)) => {
                self.position += 1;
                Ok(word.to_lowercase())
            }
            Some(Token::Quoted(word)) => {
                self.position += 1;
                Ok(word)
            }
            _ => Err(syntax(format!("expected a name{}", self.found()))),
        }
    }

    /// A possibly qualified name, re-spelled as written (quotes kept) for the
    /// planner's own name resolution.
    fn raw_name(&mut self) -> Result<String> {
        let mut raw = String::new();
        loop {
            match self.peek().cloned() {
                Some(Token::Word(word)) => raw.push_str(&word),
                Some(Token::Quoted(word)) => {
                    raw.push('"');
                    raw.push_str(&word.replace('"', "\"\""));
                    raw.push('"');
                }
                _ => return Err(syntax(format!("expected a name{}", self.found()))),
            }
            self.position += 1;
            if self.eat_punct('.') {
                raw.push('.');
            } else {
                return Ok(raw);
            }
        }
    }

    /// The last part of a possibly qualified name, normalized.
    fn last_name_part(&mut self) -> Result<String> {
        let mut last = self.ident()?;
        while self.eat_punct('.') {
            last = self.ident()?;
        }
        Ok(last)
    }

    /// Skip a parenthesized argument-type list after a routine name.
    fn skip_parenthesized(&mut self) -> Result<()> {
        if !self.eat_punct('(') {
            return Ok(());
        }
        let mut depth = 1usize;
        while depth > 0 {
            match self.peek() {
                Some(Token::Punct('(')) => depth += 1,
                Some(Token::Punct(')')) => depth -= 1,
                Some(_) => {}
                None => return Err(syntax("unterminated argument list")),
            }
            self.position += 1;
        }
        Ok(())
    }

    fn role_name(&mut self) -> Result<String> {
        self.eat_kw("GROUP");
        if self.is_kw("PUBLIC") {
            self.position += 1;
            return Ok(PUBLIC.to_string());
        }
        if self.is_kw("CURRENT_USER") || self.is_kw("CURRENT_ROLE") {
            self.position += 1;
            return Ok(super::access::current_user_name());
        }
        if self.is_kw("SESSION_USER") {
            self.position += 1;
            return Ok(super::access::session_user_name());
        }
        self.ident()
    }

    fn role_list(&mut self) -> Result<Vec<String>> {
        let mut roles = vec![self.role_name()?];
        while self.eat_punct(',') {
            roles.push(self.role_name()?);
        }
        Ok(roles)
    }

    fn ident_list(&mut self) -> Result<Vec<String>> {
        let mut names = vec![self.ident()?];
        while self.eat_punct(',') {
            names.push(self.ident()?);
        }
        Ok(names)
    }

    fn expect_end(&self) -> Result<()> {
        if self.at_end() {
            Ok(())
        } else {
            Err(syntax(format!("unexpected text at the end of the statement{}", self.found())))
        }
    }
}

/// How a statement's names are resolved to catalog keys.
pub(crate) trait Resolver {
    /// A relation name as written, to its key.
    fn relation(&self, raw: &str) -> String;
}

const PRIVILEGE_WORDS: [&str; 13] = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "USAGE",
    "CREATE",
    "CONNECT",
    "TEMPORARY",
    "TEMP",
    "EXECUTE",
];

/// The privilege list of a GRANT/REVOKE, or `None` when the list is not one
/// (a role grant: the words are role names).
fn privilege_list(cursor: &mut Cursor) -> Result<Option<Vec<PrivilegeSpec>>> {
    let start = cursor.position;
    let mut privileges = Vec::new();
    loop {
        let name = match cursor.peek() {
            Some(Token::Word(word)) if word.eq_ignore_ascii_case("ALL") => {
                cursor.position += 1;
                cursor.eat_kw("PRIVILEGES");
                "ALL".to_string()
            }
            Some(Token::Word(word)) if PRIVILEGE_WORDS.iter().any(|known| word.eq_ignore_ascii_case(known)) => {
                let upper = word.to_ascii_uppercase();
                cursor.position += 1;
                if upper == "TEMP" {
                    "TEMPORARY".to_string()
                } else {
                    upper
                }
            }
            _ => {
                cursor.position = start;
                return Ok(None);
            }
        };
        let mut columns = Vec::new();
        if cursor.eat_punct('(') {
            columns = cursor.ident_list()?;
            if !cursor.eat_punct(')') {
                return Err(syntax(format!("expected \")\" after the column list{}", cursor.found())));
            }
        }
        privileges.push(PrivilegeSpec { name, columns });
        if !cursor.eat_punct(',') {
            break;
        }
    }
    if cursor.is_kw("ON") {
        Ok(Some(privileges))
    } else {
        cursor.position = start;
        Ok(None)
    }
}

fn grant_target(cursor: &mut Cursor, resolver: &dyn Resolver) -> Result<GrantTarget> {
    if cursor.eat_kw("ALL") {
        let kind = if cursor.eat_kw("TABLES") {
            TABLE
        } else if cursor.eat_kw("SEQUENCES") {
            SEQUENCE
        } else if cursor.eat_kw("FUNCTIONS") || cursor.eat_kw("PROCEDURES") || cursor.eat_kw("ROUTINES") {
            FUNCTION
        } else {
            return Err(syntax(format!(
                "expected TABLES, SEQUENCES, FUNCTIONS, PROCEDURES or ROUTINES after ALL{}",
                cursor.found()
            )));
        };
        cursor.expect_kw("IN")?;
        cursor.expect_kw("SCHEMA")?;
        return Ok(GrantTarget::AllInSchema {
            kind: kind.to_string(),
            schemas: cursor.ident_list()?,
        });
    }
    let kind = if cursor.eat_kw("TABLE") {
        TABLE
    } else if cursor.eat_kw("SEQUENCE") {
        SEQUENCE
    } else if cursor.eat_kw("SCHEMA") {
        SCHEMA
    } else if cursor.eat_kw("DATABASE") {
        DATABASE
    } else if cursor.eat_kw("FUNCTION") || cursor.eat_kw("PROCEDURE") || cursor.eat_kw("ROUTINE") {
        FUNCTION
    } else if ["DOMAIN", "LANGUAGE", "TYPE", "TABLESPACE", "PARAMETER", "FOREIGN", "LARGE"]
        .iter()
        .any(|word| cursor.is_kw(word))
    {
        let word = match cursor.peek() {
            Some(Token::Word(word)) => word.to_ascii_uppercase(),
            _ => String::new(),
        };
        return Err(Error::query_execution(format!(
            "GRANT/REVOKE ON {word} {}",
            crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
        )));
    } else {
        TABLE
    };
    let mut names = Vec::new();
    loop {
        let name = match kind {
            TABLE => resolver.relation(&cursor.raw_name()?),
            FUNCTION => {
                let name = gate::routine_key(&cursor.last_name_part()?);
                cursor.skip_parenthesized()?;
                name
            }
            SEQUENCE => cursor.last_name_part()?,
            _ => cursor.ident()?,
        };
        names.push(name);
        if !cursor.eat_punct(',') {
            break;
        }
    }
    Ok(GrantTarget::Objects {
        kind: kind.to_string(),
        names,
    })
}

fn granted_by(cursor: &mut Cursor) -> Result<Option<String>> {
    if cursor.eat_kws(&["GRANTED", "BY"]) {
        Ok(Some(cursor.role_name()?))
    } else {
        Ok(None)
    }
}

fn parse_grant(cursor: &mut Cursor, resolver: &dyn Resolver) -> Result<AccessStatement> {
    if let Some(privileges) = privilege_list(cursor)? {
        cursor.expect_kw("ON")?;
        let target = grant_target(cursor, resolver)?;
        cursor.expect_kw("TO")?;
        let grantees = cursor.role_list()?;
        let with_grant_option = cursor.eat_kws(&["WITH", "GRANT", "OPTION"]);
        let granted_by = granted_by(cursor)?;
        cursor.expect_end()?;
        return Ok(AccessStatement::Grant(GrantSpec {
            privileges,
            target,
            grantees,
            with_grant_option,
            granted_by,
        }));
    }
    let roles = cursor.role_list()?;
    cursor.expect_kw("TO")?;
    let members = cursor.role_list()?;
    let mut admin_option = false;
    if cursor.eat_kw("WITH") {
        cursor.expect_kw("ADMIN")?;
        admin_option = if cursor.eat_kw("OPTION") || cursor.eat_kw("TRUE") {
            true
        } else if cursor.eat_kw("FALSE") {
            false
        } else {
            return Err(syntax(format!("expected OPTION after WITH ADMIN{}", cursor.found())));
        };
    }
    let granted_by = granted_by(cursor)?;
    cursor.expect_end()?;
    Ok(AccessStatement::GrantRole {
        roles,
        members,
        admin_option,
        granted_by,
    })
}

fn parse_revoke(cursor: &mut Cursor, resolver: &dyn Resolver) -> Result<AccessStatement> {
    let grant_option_only = cursor.eat_kws(&["GRANT", "OPTION", "FOR"]);
    if let Some(privileges) = privilege_list(cursor)? {
        cursor.expect_kw("ON")?;
        let target = grant_target(cursor, resolver)?;
        cursor.expect_kw("FROM")?;
        let grantees = cursor.role_list()?;
        let granted_by = granted_by(cursor)?;
        let cascade = cursor.eat_kw("CASCADE");
        if !cascade {
            cursor.eat_kw("RESTRICT");
        }
        cursor.expect_end()?;
        return Ok(AccessStatement::Revoke(RevokeSpec {
            privileges,
            target,
            grantees,
            grant_option_only,
            cascade,
            granted_by,
        }));
    }
    if grant_option_only {
        return Err(syntax("REVOKE GRANT OPTION FOR requires a privilege list and ON"));
    }
    let admin_option_only = cursor.eat_kws(&["ADMIN", "OPTION", "FOR"]);
    let roles = cursor.role_list()?;
    cursor.expect_kw("FROM")?;
    let members = cursor.role_list()?;
    let _ = granted_by(cursor)?;
    if !cursor.eat_kw("CASCADE") {
        cursor.eat_kw("RESTRICT");
    }
    cursor.expect_end()?;
    Ok(AccessStatement::RevokeRole {
        roles,
        members,
        admin_option_only,
    })
}

fn parse_set(cursor: &mut Cursor) -> Result<Option<AccessStatement>> {
    let local = cursor.eat_kw("LOCAL");
    if !local {
        cursor.eat_kw("SESSION");
    }
    let refuse_local = |what: &str| {
        Error::query_execution(format!(
            "SET LOCAL {what} {}",
            crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
        ))
    };
    // `SET SESSION AUTHORIZATION` — the SESSION above was its first word.
    if cursor.is_kw("AUTHORIZATION") && !local {
        cursor.position += 1;
        let target = if cursor.eat_kw("DEFAULT") {
            None
        } else {
            Some(cursor.ident()?)
        };
        cursor.expect_end()?;
        return Ok(Some(AccessStatement::SetSessionAuthorization(target)));
    }
    if cursor.eat_kws(&["SESSION", "AUTHORIZATION"]) {
        if local {
            return Err(refuse_local("SESSION AUTHORIZATION"));
        }
        let target = if cursor.eat_kw("DEFAULT") {
            None
        } else {
            Some(cursor.ident()?)
        };
        cursor.expect_end()?;
        return Ok(Some(AccessStatement::SetSessionAuthorization(target)));
    }
    if cursor.eat_kw("ROLE") {
        if local {
            return Err(refuse_local("ROLE"));
        }
        if cursor.at_end() || cursor.is_kw("TO") {
            return Ok(None);
        }
        let target = if cursor.eat_kw("NONE") {
            None
        } else {
            Some(cursor.ident()?)
        };
        cursor.expect_end()?;
        return Ok(Some(AccessStatement::SetRole(target)));
    }
    Ok(None)
}

fn parse_alter(cursor: &mut Cursor, resolver: &dyn Resolver) -> Result<Option<AccessStatement>> {
    if cursor.eat_kws(&["DEFAULT", "PRIVILEGES"]) {
        return Err(Error::query_execution(format!(
            "ALTER DEFAULT PRIVILEGES {}",
            crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
        )));
    }
    if cursor.is_kw("ROLE") || cursor.is_kw("USER") {
        cursor.position += 1;
        let Ok(name) = cursor.ident() else {
            return Ok(None);
        };
        if cursor.eat_kws(&["RENAME", "TO"]) {
            let new_name = cursor.ident()?;
            cursor.expect_end()?;
            return Ok(Some(AccessStatement::RenameRole { name, new_name }));
        }
        return Ok(None);
    }
    let (kind, kind_word) = if cursor.eat_kw("TABLE") {
        (TABLE, "table")
    } else if cursor.eat_kws(&["MATERIALIZED", "VIEW"]) {
        (TABLE, "materialized view")
    } else if cursor.eat_kw("VIEW") {
        (TABLE, "view")
    } else if cursor.eat_kw("SEQUENCE") {
        (SEQUENCE, "sequence")
    } else if cursor.eat_kw("SCHEMA") {
        (SCHEMA, "schema")
    } else if cursor.eat_kw("DATABASE") {
        (DATABASE, "database")
    } else if cursor.eat_kw("FUNCTION") || cursor.eat_kw("ROUTINE") {
        (FUNCTION, "function")
    } else if cursor.eat_kw("PROCEDURE") {
        (FUNCTION, "procedure")
    } else {
        return Ok(None);
    };
    cursor.eat_kws(&["IF", "EXISTS"]);
    cursor.eat_kw("ONLY");
    let name = match kind {
        TABLE => match cursor.raw_name() {
            Ok(raw) => resolver.relation(&raw),
            Err(_) => return Ok(None),
        },
        FUNCTION => {
            let Ok(name) = cursor.last_name_part() else {
                return Ok(None);
            };
            if cursor.skip_parenthesized().is_err() {
                return Ok(None);
            }
            gate::routine_key(&name)
        }
        _ => match cursor.last_name_part() {
            Ok(name) => name,
            Err(_) => return Ok(None),
        },
    };
    if !cursor.eat_kws(&["OWNER", "TO"]) {
        return Ok(None);
    }
    let new_owner = cursor.role_name()?;
    cursor.expect_end()?;
    Ok(Some(AccessStatement::AlterOwner {
        kind: kind.to_string(),
        name,
        kind_word: kind_word.to_string(),
        new_owner,
    }))
}

/// Parse `sql` if it is one of the access-control statements. `Ok(None)` for
/// anything else, which is then parsed as before.
pub(crate) fn parse(sql: &str, resolver: &dyn Resolver) -> Result<Option<AccessStatement>> {
    let head = sql.trim_start();
    let first: String = head
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    if !matches!(first.as_str(), "GRANT" | "REVOKE" | "SET" | "RESET" | "ALTER") {
        return Ok(None);
    }
    let stripped = crate::sql::Parser::strip_sql_comments(sql);
    let Some(tokens) = tokenize(&stripped) else {
        return Ok(None);
    };
    let mut cursor = Cursor { tokens, position: 1 };
    match first.as_str() {
        "GRANT" => parse_grant(&mut cursor, resolver).map(Some),
        "REVOKE" => parse_revoke(&mut cursor, resolver).map(Some),
        "SET" => parse_set(&mut cursor),
        "RESET" => {
            if cursor.eat_kw("ROLE") && cursor.at_end() {
                Ok(Some(AccessStatement::SetRole(None)))
            } else if cursor.eat_kws(&["SESSION", "AUTHORIZATION"]) && cursor.at_end() {
                Ok(Some(AccessStatement::SetSessionAuthorization(None)))
            } else {
                Ok(None)
            }
        }
        _ => parse_alter(&mut cursor, resolver),
    }
}

/// True if `sql` is one of the access-control statements (whether or not it
/// is well formed: a malformed one is reported when it is planned).
pub fn is_access_statement(sql: &str) -> bool {
    struct AsWritten;
    impl Resolver for AsWritten {
        fn relation(&self, raw: &str) -> String {
            raw.to_string()
        }
    }
    !matches!(parse(sql, &AsWritten), Ok(None))
}

// ---------------------------------------------------------------------------
// Execution.
// ---------------------------------------------------------------------------

/// Run an access-control statement as the current role.
pub(crate) fn execute(storage: &StorageEngine, statement: &AccessStatement) -> Result<()> {
    match statement {
        AccessStatement::Grant(spec) => exec_grant(storage, spec),
        AccessStatement::Revoke(spec) => exec_revoke(storage, spec),
        AccessStatement::GrantRole {
            roles,
            members,
            admin_option,
            granted_by,
        } => exec_grant_role(storage, roles, members, *admin_option, granted_by.as_deref()),
        AccessStatement::RevokeRole {
            roles,
            members,
            admin_option_only,
        } => exec_revoke_role(storage, roles, members, *admin_option_only),
        AccessStatement::SetRole(target) => exec_set_role(storage, target.as_deref()),
        AccessStatement::SetSessionAuthorization(target) => exec_set_authorization(storage, target.as_deref()),
        AccessStatement::AlterOwner {
            kind,
            name,
            kind_word,
            new_owner,
        } => exec_alter_owner(storage, kind, name, kind_word, new_owner),
        AccessStatement::RenameRole { name, new_name } => exec_rename_role(storage, name, new_name),
    }
}

fn kind_word(storage: &StorageEngine, kind: &str, object: &str) -> &'static str {
    match kind {
        TABLE => gate::relation_word(storage, object),
        SEQUENCE => "sequence",
        SCHEMA => "schema",
        FUNCTION => "function",
        DATABASE => "database",
        _ => "object",
    }
}

fn routine_exists(storage: &StorageEngine, name: &str) -> Result<bool> {
    Ok(catalog::routine_names(storage)?.contains(name))
}

pub(crate) fn object_exists(storage: &StorageEngine, kind: &str, object: &str) -> Result<bool> {
    match kind {
        TABLE => Ok(gate::relation_exists(storage, object)),
        SEQUENCE => storage.catalog().sequence_exists(object),
        SCHEMA => Ok(object == PUBLIC || storage.catalog().schema_exists(object)?),
        FUNCTION => routine_exists(storage, object),
        DATABASE => Ok(true),
        other => Err(Error::query_execution(format!(
            "GRANT/REVOKE on object type '{other}' {}",
            crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
        ))),
    }
}

fn missing_object(kind: &str, object: &str) -> Error {
    match kind {
        TABLE => Error::query_execution(format!("Table '{object}' does not exist")),
        SCHEMA => Error::query_execution(format!("schema \"{object}\" does not exist")),
        FUNCTION => Error::query_execution(format!("function {object} does not exist")),
        _ => Error::query_execution(format!("{kind} \"{object}\" does not exist")),
    }
}

/// The objects a target names.
fn expand_target(storage: &StorageEngine, target: &GrantTarget) -> Result<Vec<String>> {
    match target {
        GrantTarget::Objects { names, .. } => Ok(names.clone()),
        GrantTarget::AllInSchema { kind, schemas } => {
            let catalog = storage.catalog();
            let mut objects = Vec::new();
            for schema in schemas {
                if schema != PUBLIC && !catalog.schema_exists(schema)? {
                    return Err(missing_object(SCHEMA, schema));
                }
                let in_schema = |key: &str| match key.split_once('.') {
                    Some((key_schema, _)) => key_schema == schema,
                    None => schema == PUBLIC,
                };
                match kind.as_str() {
                    TABLE => {
                        objects.extend(catalog.list_tables()?.into_iter().filter(|key| in_schema(key)));
                        if schema == PUBLIC {
                            objects.extend(storage.view_catalog().list_views()?);
                        }
                    }
                    SEQUENCE => objects.extend(
                        catalog
                            .list_sequences()?
                            .into_iter()
                            .map(|sequence| sequence.name)
                            .filter(|key| in_schema(key)),
                    ),
                    _ => {
                        if schema == PUBLIC {
                            for prefix in ["meta:function:", "meta:procedure:"] {
                                objects.extend(
                                    storage
                                        .meta_blobs_with_prefix(prefix)?
                                        .into_iter()
                                        .map(|(name, _)| name.to_ascii_lowercase()),
                                );
                            }
                        }
                    }
                }
            }
            objects.sort();
            objects.dedup();
            Ok(objects)
        }
    }
}

/// Expand `ALL` and validate every privilege for the object kind. Returns
/// `(privilege, columns)` pairs.
fn resolve_privileges(privileges: &[PrivilegeSpec], kind: &str) -> Result<Vec<(String, Vec<String>)>> {
    if privileges.is_empty() {
        return Err(Error::query_execution("GRANT/REVOKE requires at least one privilege"));
    }
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for spec in privileges {
        if !spec.columns.is_empty() && kind != TABLE {
            return Err(Error::query_execution(format!(
                "column privileges are only valid for relations, not for a {kind}"
            )));
        }
        if spec.name == "ALL" {
            let all: &[&str] = if spec.columns.is_empty() {
                privileges_of(kind)
            } else {
                &COLUMN_PRIVILEGES
            };
            out.extend(all.iter().map(|name| ((*name).to_string(), spec.columns.clone())));
            continue;
        }
        if !privileges_of(kind).contains(&spec.name.as_str()) {
            return Err(Error::query_execution(format!(
                "privilege {} is not valid for a {kind}",
                spec.name
            )));
        }
        if !spec.columns.is_empty() && !COLUMN_PRIVILEGES.contains(&spec.name.as_str()) {
            return Err(Error::query_execution(format!(
                "privilege {} cannot be granted on columns",
                spec.name
            )));
        }
        out.push((spec.name.clone(), spec.columns.clone()));
    }
    Ok(out)
}

fn grantee_exists(snapshot: &Snapshot, grantee: &str) -> bool {
    grantee == PUBLIC || snapshot.role_exists(grantee)
}

/// The role recorded as grantor: the current role, or the service account for
/// a caller without a role identity.
fn grantor_name(gate: Option<&Gate<'_>>, granted_by: Option<&str>) -> Result<String> {
    let own = match gate.map(Gate::principal) {
        Some(Principal::Role { name, operator: false }) => name.to_string(),
        _ => SERVICE_GRANTOR.to_string(),
    };
    match granted_by {
        None => Ok(own),
        Some(role) => {
            let superuser = gate.is_none_or(Gate::is_superuser);
            if superuser || role == own {
                Ok(role.to_string())
            } else {
                Err(denied_to(&format!(
                    "grant privileges as role \"{role}\": the grantor must be the current user"
                )))
            }
        }
    }
}

/// May the current role grant `privilege` (on `columns`, or the whole object)?
fn may_grant(gate: &Gate<'_>, kind: &str, object: &str, privilege: &str, columns: &[String]) -> bool {
    if gate.is_superuser() || gate.owns(kind, object) {
        return true;
    }
    let identities = gate.identities();
    if gate.snapshot().object_grant(kind, object, privilege, identities) == Some(true) {
        return true;
    }
    if columns.is_empty() || kind != TABLE {
        return false;
    }
    let grantable = gate.snapshot().granted_columns(object, privilege, identities, true);
    columns.iter().all(|column| grantable.contains(column))
}

fn check_columns(storage: &StorageEngine, table: &str, columns: &[String]) -> Result<()> {
    if columns.is_empty() {
        return Ok(());
    }
    let known = gate::relation_columns(storage, table).unwrap_or_default();
    for column in columns {
        if !known.contains(&column.to_ascii_lowercase()) {
            return Err(Error::query_execution(format!(
                "Column \"{column}\" of relation \"{table}\" does not exist"
            )));
        }
    }
    Ok(())
}

fn exec_grant(storage: &StorageEngine, spec: &GrantSpec) -> Result<()> {
    let catalog = storage.catalog();
    let lenient = storage.config().authentication.legacy_acl_noop;
    let gate = Gate::current(storage)?;
    let kind = spec.target.kind().to_string();
    let privileges = resolve_privileges(&spec.privileges, &kind)?;
    let grantor = grantor_name(gate.as_ref(), spec.granted_by.as_deref())?;
    let snapshot = catalog::snapshot(storage)?;
    let mut grantees = Vec::new();
    for grantee in &spec.grantees {
        if grantee_exists(&snapshot, grantee) {
            grantees.push(grantee.as_str());
        } else if !lenient {
            return Err(catalog::no_such_role(grantee));
        }
    }
    let named = matches!(spec.target, GrantTarget::Objects { .. });
    for object in expand_target(storage, &spec.target)? {
        if !object_exists(storage, &kind, &object)? {
            if lenient || !named {
                continue;
            }
            return Err(missing_object(&kind, &object));
        }
        for (privilege, columns) in &privileges {
            check_columns(storage, &object, columns)?;
            if let Some(gate) = gate.as_ref() {
                if !may_grant(gate, &kind, &object, privilege, columns) {
                    return Err(gate::denied(kind_word(storage, &kind, &object), &object));
                }
            }
        }
        for grantee in &grantees {
            for (privilege, columns) in &privileges {
                let one = std::slice::from_ref(privilege);
                if columns.is_empty() {
                    catalog.grant_privileges(&kind, &object, grantee, &grantor, one, spec.with_grant_option)?;
                    if *grantee == PUBLIC {
                        // Granting PUBLIC its default back removes the mark that it was revoked.
                        catalog.revoke_privileges(NODEFAULT, &kind_object(&kind, &object), PUBLIC, one)?;
                    }
                } else {
                    for column in columns {
                        catalog.grant_privileges(
                            COLUMN,
                            &column_object(&object, &column.to_ascii_lowercase()),
                            grantee,
                            &grantor,
                            one,
                            spec.with_grant_option,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// One stored grant that a revoke removes or narrows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    object_type: String,
    object_name: String,
    grantee: String,
    grantor: String,
    privilege: String,
    grantable: bool,
}

/// The stored grants of `privilege` to `grantee` on `(kind, object)`: the
/// object-level one and, for a relation, the column ones (`columns` empty =
/// every column).
fn held_grants(
    catalog: &Catalog<'_>,
    kind: &str,
    object: &str,
    grantee: &str,
    privilege: &str,
    columns: &[String],
) -> Result<Vec<Held>> {
    let mut held = Vec::new();
    for record in catalog.list_acls()? {
        if record.grantee != grantee {
            continue;
        }
        let about = match record.object_type.as_str() {
            COLUMN if kind == TABLE => record.object_name.split_once(catalog::SEP).is_some_and(|(table, column)| {
                table == object && (columns.is_empty() || columns.iter().any(|c| c.eq_ignore_ascii_case(column)))
            }),
            other => other == kind && record.object_name == object && columns.is_empty(),
        };
        if !about {
            continue;
        }
        for (name, grantable) in &record.privileges {
            if name == privilege {
                held.push(Held {
                    object_type: record.object_type.clone(),
                    object_name: record.object_name.clone(),
                    grantee: record.grantee.clone(),
                    grantor: record.grantor.clone(),
                    privilege: name.clone(),
                    grantable: *grantable,
                });
            }
        }
    }
    Ok(held)
}

/// Grants of the same privilege on the same object that `grantor` issued.
fn dependents(catalog: &Catalog<'_>, kind: &str, object: &str, grantor: &str, privilege: &str) -> Result<Vec<Held>> {
    let mut out = Vec::new();
    for record in catalog.list_acls()? {
        if record.grantor != grantor || record.grantee == grantor {
            continue;
        }
        let about = match record.object_type.as_str() {
            COLUMN if kind == TABLE => record
                .object_name
                .split_once(catalog::SEP)
                .is_some_and(|(table, _)| table == object),
            other => other == kind && record.object_name == object,
        };
        if !about {
            continue;
        }
        for (name, grantable) in &record.privileges {
            if name == privilege {
                out.push(Held {
                    object_type: record.object_type.clone(),
                    object_name: record.object_name.clone(),
                    grantee: record.grantee.clone(),
                    grantor: record.grantor.clone(),
                    privilege: name.clone(),
                    grantable: *grantable,
                });
            }
        }
    }
    Ok(out)
}

fn remove_held(catalog: &Catalog<'_>, held: &Held, grant_option_only: bool) -> Result<()> {
    let one = std::slice::from_ref(&held.privilege);
    catalog.revoke_privileges(&held.object_type, &held.object_name, &held.grantee, one)?;
    if grant_option_only {
        catalog.grant_privileges(
            &held.object_type,
            &held.object_name,
            &held.grantee,
            &held.grantor,
            one,
            false,
        )?;
    }
    Ok(())
}

fn exec_revoke(storage: &StorageEngine, spec: &RevokeSpec) -> Result<()> {
    let catalog = storage.catalog();
    let lenient = storage.config().authentication.legacy_acl_noop;
    let gate = Gate::current(storage)?;
    let kind = spec.target.kind().to_string();
    let privileges = resolve_privileges(&spec.privileges, &kind)?;
    let revoker = grantor_name(gate.as_ref(), spec.granted_by.as_deref())?;
    let snapshot = catalog::snapshot(storage)?;
    let mut grantees = Vec::new();
    for grantee in &spec.grantees {
        if grantee_exists(&snapshot, grantee) {
            grantees.push(grantee.as_str());
        } else if !lenient {
            return Err(catalog::no_such_role(grantee));
        }
    }
    let named = matches!(spec.target, GrantTarget::Objects { .. });
    for object in expand_target(storage, &spec.target)? {
        if !object_exists(storage, &kind, &object)? {
            if lenient || !named {
                continue;
            }
            return Err(missing_object(&kind, &object));
        }
        // The owner (and a superuser) revokes any grant; anyone else only the
        // grants they issued.
        let any_grant = gate.as_ref().is_none_or(|gate| gate.owns(&kind, &object));
        if let Some(gate) = gate.as_ref() {
            if !any_grant && gate.principal() == &Principal::Anonymous {
                return Err(gate::denied(kind_word(storage, &kind, &object), &object));
            }
        }
        for grantee in &grantees {
            for (privilege, columns) in &privileges {
                let held = held_grants(&catalog, &kind, &object, grantee, privilege, columns)?;
                let mut queue: Vec<Held> = Vec::new();
                for grant in held {
                    if !any_grant && grant.grantor != revoker {
                        return Err(gate::denied(kind_word(storage, &kind, &object), &object));
                    }
                    queue.push(grant);
                }
                // What the grantee passed on depends on the grant option that
                // is going away.
                let mut cascade: Vec<Held> = Vec::new();
                let mut frontier: Vec<String> = queue
                    .iter()
                    .filter(|grant| grant.grantable)
                    .map(|grant| grant.grantee.clone())
                    .collect();
                let mut visited: Vec<String> = Vec::new();
                while let Some(grantor) = frontier.pop() {
                    if visited.contains(&grantor) || grantor == PUBLIC {
                        continue;
                    }
                    visited.push(grantor.clone());
                    for dependent in dependents(&catalog, &kind, &object, &grantor, privilege)? {
                        if dependent.grantable {
                            frontier.push(dependent.grantee.clone());
                        }
                        cascade.push(dependent);
                    }
                }
                if !cascade.is_empty() && !spec.cascade {
                    return Err(Error::query_execution(format!(
                        "{DEPENDENT_PRIVILEGES_EXIST}: privilege {privilege} on {} {} was granted on by \"{grantee}\" \
                         (use CASCADE to revoke those grants too)",
                        kind_word(storage, &kind, &object),
                        object
                    )));
                }
                for grant in &queue {
                    remove_held(&catalog, grant, spec.grant_option_only)?;
                }
                for grant in &cascade {
                    remove_held(&catalog, grant, false)?;
                }
                // Taking PUBLIC's default away has to be recorded: there is no
                // grant to delete.
                if *grantee == PUBLIC
                    && columns.is_empty()
                    && !spec.grant_option_only
                    && catalog::default_public_privileges(&kind, &object).contains(&privilege.as_str())
                {
                    if !any_grant {
                        return Err(gate::denied(kind_word(storage, &kind, &object), &object));
                    }
                    catalog.grant_privileges(
                        NODEFAULT,
                        &kind_object(&kind, &object),
                        PUBLIC,
                        &revoker,
                        std::slice::from_ref(privilege),
                        false,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// May the current role change who is a member of `role`?
fn may_administer(gate: Option<&Gate<'_>>, role: &str) -> bool {
    let Some(gate) = gate else { return true };
    if gate.is_superuser() {
        return true;
    }
    let Some(name) = gate.principal().name() else {
        return false;
    };
    if gate.snapshot().role_is_superuser(role) {
        return false;
    }
    gate.snapshot().role(name).is_some_and(|record| record.rolcreaterole) || gate.snapshot().has_admin_option(name, role)
}

fn exec_grant_role(
    storage: &StorageEngine,
    roles: &[String],
    members: &[String],
    admin_option: bool,
    granted_by: Option<&str>,
) -> Result<()> {
    let catalog = storage.catalog();
    let gate = Gate::current(storage)?;
    let grantor = grantor_name(gate.as_ref(), granted_by)?;
    for role in roles {
        for member in members {
            // Re-read per pair: an earlier pair of this statement may have
            // added an edge the cycle check has to see.
            let snapshot = catalog::snapshot(storage)?;
            if role == PUBLIC || crate::storage::is_reserved_role_name(role) {
                return Err(Error::query_execution(format!(
                    "role \"{role}\" is reserved and cannot be granted to another role"
                )));
            }
            if snapshot.role(role).is_none() {
                return Err(catalog::no_such_role(role));
            }
            if member == PUBLIC || !snapshot.role_exists(member) {
                return Err(catalog::no_such_role(member));
            }
            if !may_administer(gate.as_ref(), role) {
                return Err(denied_to(&format!(
                    "grant role \"{role}\": the ADMIN option on the role, or CREATEROLE, is required"
                )));
            }
            if role == member || snapshot.member_closure(role).iter().any(|held| held == member) {
                return Err(Error::query_execution(format!(
                    "role \"{role}\" {ROLE_MEMBERSHIP_CYCLE} \"{member}\""
                )));
            }
            catalog.grant_privileges(MEMBER, role, member, &grantor, &["MEMBER".to_string()], admin_option)?;
        }
    }
    Ok(())
}

fn exec_revoke_role(
    storage: &StorageEngine,
    roles: &[String],
    members: &[String],
    admin_option_only: bool,
) -> Result<()> {
    let catalog = storage.catalog();
    let gate = Gate::current(storage)?;
    let snapshot = catalog::snapshot(storage)?;
    for role in roles {
        if snapshot.role(role).is_none() {
            return Err(catalog::no_such_role(role));
        }
        if !may_administer(gate.as_ref(), role) {
            return Err(denied_to(&format!(
                "revoke role \"{role}\": the ADMIN option on the role, or CREATEROLE, is required"
            )));
        }
        for member in members {
            if !snapshot.role_exists(member) {
                return Err(catalog::no_such_role(member));
            }
            let Some(record) = catalog.get_acl(MEMBER, role, member)? else {
                continue;
            };
            catalog.revoke_privileges(MEMBER, role, member, &["MEMBER".to_string()])?;
            if admin_option_only {
                catalog.grant_privileges(MEMBER, role, member, &record.grantor, &["MEMBER".to_string()], false)?;
            }
        }
    }
    Ok(())
}

fn session_access() -> Result<std::sync::Arc<crate::session::scoped::SessionScopedState>> {
    crate::session_scoped_state_tls()
        .ok_or_else(|| Error::query_execution("SET ROLE requires a session: this statement ran outside one"))
}

fn exec_set_role(storage: &StorageEngine, target: Option<&str>) -> Result<()> {
    let state = session_access()?;
    let Some(role) = target else {
        state.access().set_role(None);
        return Ok(());
    };
    let snapshot = catalog::snapshot(storage)?;
    if role == PUBLIC || !snapshot.role_exists(role) {
        return Err(catalog::no_such_role(role));
    }
    let allowed = match session_principal(storage) {
        Principal::Unrestricted => true,
        Principal::Anonymous => false,
        Principal::Role { operator: true, .. } => true,
        Principal::Role { name, .. } => {
            snapshot.role_is_superuser(&name)
                || &*name == role
                || snapshot.member_closure(&name).iter().any(|held| held == role)
        }
    };
    if !allowed {
        return Err(denied_to(&format!("set role \"{role}\"")));
    }
    state.access().set_role(Some(role));
    Ok(())
}

fn exec_set_authorization(storage: &StorageEngine, target: Option<&str>) -> Result<()> {
    let state = session_access()?;
    let Some(role) = target else {
        state.access().set_authorization(None);
        return Ok(());
    };
    let snapshot = catalog::snapshot(storage)?;
    if role == PUBLIC || !snapshot.role_exists(role) {
        return Err(catalog::no_such_role(role));
    }
    // Decided by the identity the session authenticated as, whatever it has
    // switched to since.
    let allowed = match login_principal(storage, Some(state.access())) {
        Principal::Unrestricted => true,
        Principal::Anonymous => false,
        Principal::Role { operator: true, .. } => true,
        Principal::Role { name, .. } => snapshot.role_is_superuser(&name) || &*name == role,
    };
    if !allowed {
        return Err(denied_to(&format!("set session authorization \"{role}\"")));
    }
    state.access().set_authorization(Some(role));
    Ok(())
}

fn exec_alter_owner(storage: &StorageEngine, kind: &str, name: &str, word: &str, new_owner: &str) -> Result<()> {
    let catalog = storage.catalog();
    if !object_exists(storage, kind, name)? {
        return Err(missing_object(kind, name));
    }
    let snapshot = catalog::snapshot(storage)?;
    if new_owner == PUBLIC || !snapshot.role_exists(new_owner) {
        if storage.config().authentication.legacy_acl_noop && new_owner != PUBLIC {
            // The same leniency GRANT has under this flag: a restore script
            // that names a role this server does not have changes nothing.
            return Ok(());
        }
        return Err(catalog::no_such_role(new_owner));
    }
    if let Some(gate) = Gate::current(storage)? {
        if !gate.is_superuser() {
            gate.require_owner(kind, name, word)?;
            let own = gate.principal().name().unwrap_or("");
            if !snapshot.member_closure(own).iter().any(|held| held == new_owner) {
                return Err(denied_to(&format!(
                    "change the owner: must be a member of role \"{new_owner}\""
                )));
            }
        }
    }
    if new_owner == BOOTSTRAP_SUPERUSER {
        catalog::clear_owner(&catalog, kind, name)
    } else {
        catalog::set_owner(&catalog, kind, name, new_owner)
    }
}

fn exec_rename_role(storage: &StorageEngine, name: &str, new_name: &str) -> Result<()> {
    let catalog = storage.catalog();
    if crate::storage::is_reserved_role_name(name) {
        return Err(Error::query_execution(format!("cannot alter built-in role \"{name}\"")));
    }
    if crate::storage::is_reserved_role_name(new_name) {
        return Err(Error::query_execution(format!(
            "role \"{new_name}\" is reserved and cannot be created"
        )));
    }
    let Some(mut record) = catalog.get_role(name)? else {
        return Err(catalog::no_such_role(name));
    };
    if catalog.role_exists(new_name)? {
        return Err(Error::query_execution(format!("role \"{new_name}\" already exists")));
    }
    if let Some(gate) = Gate::current(storage)? {
        if !gate.is_superuser() {
            let own = gate.principal().name().unwrap_or("");
            let createrole = gate.snapshot().role(own).is_some_and(|role| role.rolcreaterole);
            if !createrole || record.rolsuper {
                return Err(denied_to("rename role"));
            }
        }
    }
    let in_use = [current_principal(storage), session_principal(storage)]
        .iter()
        .any(|principal| principal.name() == Some(name));
    if in_use {
        return Err(Error::query_execution(format!(
            "renaming the role of the current session {}",
            crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
        )));
    }
    record.name = new_name.to_string();
    catalog.save_role(&record)?;
    catalog.delete_role(name)?;
    catalog::rename_role_references(&catalog, name, new_name)
}

/// Refuse `SECURITY DEFINER` on a routine definition. A routine runs with the
/// rights of the role that calls it; accepting the clause and ignoring it
/// would let a definition claim rights it does not get. Looks only at the
/// clause itself: text inside a string, a quoted name or a `$$` body is not it.
pub(crate) fn refuse_security_definer(sql: &str) -> Result<()> {
    let head = sql.trim_start();
    let starts = |prefix: &str| head.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix));
    if !(starts("CREATE") || starts("ALTER")) {
        return Ok(());
    }
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut chars = head.chars().peekable();
    let flush = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word).to_ascii_uppercase());
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                flush(&mut word, &mut words);
                // Skip to the closing quote (a doubled quote is an escaped one).
                while let Some(inner) = chars.next() {
                    if inner == c {
                        if chars.peek() == Some(&c) {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            '$' => {
                flush(&mut word, &mut words);
                // A dollar-quoted body: `$tag$ … $tag$`.
                let mut tag = String::from("$");
                let mut is_tag = false;
                while let Some(&next) = chars.peek() {
                    chars.next();
                    tag.push(next);
                    if next == '$' {
                        is_tag = true;
                        break;
                    }
                    if !(next.is_alphanumeric() || next == '_') {
                        break;
                    }
                }
                if is_tag {
                    let rest: String = chars.by_ref().collect();
                    let after = rest.find(&tag).map_or("", |end| rest.get(end + tag.len()..).unwrap_or(""));
                    if words_name_a_routine(&words) {
                        return refuse_security_definer_tail(&words, after);
                    }
                    return Ok(());
                }
            }
            c if c.is_alphanumeric() || c == '_' => word.push(c),
            _ => flush(&mut word, &mut words),
        }
    }
    flush(&mut word, &mut words);
    if words_name_a_routine(&words) && has_security_definer(&words) {
        return Err(security_definer_refusal());
    }
    Ok(())
}

fn words_name_a_routine(words: &[String]) -> bool {
    words
        .iter()
        .take(4)
        .any(|word| matches!(word.as_str(), "FUNCTION" | "PROCEDURE" | "ROUTINE"))
}

fn has_security_definer(words: &[String]) -> bool {
    words
        .windows(2)
        .any(|pair| matches!(pair, [first, second] if first == "SECURITY" && second == "DEFINER"))
}

/// The clause may come before the body or after it.
fn refuse_security_definer_tail(before: &[String], after_body: &str) -> Result<()> {
    if has_security_definer(before) {
        return Err(security_definer_refusal());
    }
    let after: Vec<String> = after_body
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    if has_security_definer(&after) {
        return Err(security_definer_refusal());
    }
    Ok(())
}

fn security_definer_refusal() -> Error {
    Error::query_execution(format!(
        "SECURITY DEFINER {} (a routine runs with the rights of the role that calls it; \
         omit the clause or declare the routine SECURITY INVOKER)",
        crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
    ))
}

/// `DROP ROLE` housekeeping: refuse while the role owns objects or holds
/// privileges, then forget its memberships.
pub(crate) fn before_drop_role(catalog: &Catalog<'_>, name: &str) -> Result<()> {
    let dependencies = catalog::role_dependencies(catalog, name)?;
    if let Some(first) = dependencies.first() {
        return Err(Error::query_execution(format!(
            "role \"{name}\" cannot be dropped because some objects depend on it \
             (revoke its privileges first): {first}{}",
            if dependencies.len() > 1 {
                format!(" and {} more", dependencies.len() - 1)
            } else {
                String::new()
            }
        )));
    }
    catalog::forget_memberships(catalog, name)
}

/// The statement form of the plan-level `GRANT` the SQL parser understands.
pub(crate) fn grant_from_plan(
    privileges: &[String],
    all_privileges: bool,
    object_type: &str,
    objects: &[String],
    grantees: &[String],
    with_grant_option: bool,
) -> AccessStatement {
    AccessStatement::Grant(GrantSpec {
        privileges: plan_privileges(privileges, all_privileges),
        target: GrantTarget::Objects {
            kind: object_type.to_string(),
            names: objects.to_vec(),
        },
        grantees: grantees.to_vec(),
        with_grant_option,
        granted_by: None,
    })
}

/// The statement form of the plan-level `REVOKE` the SQL parser understands.
pub(crate) fn revoke_from_plan(
    privileges: &[String],
    all_privileges: bool,
    object_type: &str,
    objects: &[String],
    grantees: &[String],
) -> AccessStatement {
    AccessStatement::Revoke(RevokeSpec {
        privileges: plan_privileges(privileges, all_privileges),
        target: GrantTarget::Objects {
            kind: object_type.to_string(),
            names: objects.to_vec(),
        },
        grantees: grantees.to_vec(),
        grant_option_only: false,
        cascade: false,
        granted_by: None,
    })
}

fn plan_privileges(privileges: &[String], all_privileges: bool) -> Vec<PrivilegeSpec> {
    if all_privileges {
        return vec![PrivilegeSpec {
            name: "ALL".to_string(),
            columns: Vec::new(),
        }];
    }
    privileges
        .iter()
        .map(|name| PrivilegeSpec {
            name: name.clone(),
            columns: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
#[path = "statement_tests.rs"]
mod tests;
