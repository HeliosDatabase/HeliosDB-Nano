//! Object identifiers derived from names.
//!
//! An object identifier used to be a position in a sorted list: creating or
//! dropping one table renumbered the tables after it, and the table numbers
//! (from 1000) ran into the array type identifiers of `pg_type`. An identifier
//! is now a hash of the object's kind and name, in a range no built-in
//! identifier reaches, so it is the same in every view, in every process, and
//! before and after unrelated DDL.
//!
//! # Collisions
//!
//! Two names can hash to the same number (about one chance in four million
//! for a database with a thousand objects). [`DictOids`] resolves that the
//! same way everywhere: objects claim their identifier in a fixed order
//! (tables, then indexes, vector indexes, views and sequences, each sorted by
//! name), and an object whose number is taken re-hashes with a counter until
//! it finds a free one. The object that claims first keeps its plain hash; only
//! the later one of a colliding pair can move, and only if the earlier one is
//! created or dropped.

use std::collections::{BTreeMap, HashSet};

use crate::storage::StorageEngine;
use crate::Result;

/// The first identifier this module hands out. PostgreSQL's built-in objects
/// end below 16384; the private type identifiers of this engine sit just above
/// it; nothing else is at or above this value.
pub const FIRST_DERIVED_OID: i32 = 100_000;
/// One past the last identifier handed out. Kept well under `i32::MAX` so an
/// identifier plus a small offset still fits the 4-byte signed wire type.
pub const END_DERIVED_OID: i32 = 2_000_000_000;

/// The kind of object an identifier names. Part of the hash, so a table and an
/// index of the same name get different identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OidClass {
    /// A table, by its storage key.
    Table,
    /// An ordinary index, by `table`, NUL, `name`.
    Index,
    /// A vector index, by `table`, NUL, `name`.
    VectorIndex,
    /// A view, by `schema`, NUL, `name`.
    View,
    /// A sequence, by its lowercased name.
    Sequence,
    /// A constraint, by `table`, NUL, `name`.
    Constraint,
    /// A schema, by name.
    Schema,
    /// A function or procedure, by kind letter, NUL, name.
    Routine,
}

impl OidClass {
    fn tag(self) -> u8 {
        match self {
            Self::Table => b'r',
            Self::Index => b'i',
            Self::VectorIndex => b'x',
            Self::View => b'v',
            Self::Sequence => b'S',
            Self::Constraint => b'c',
            Self::Schema => b'n',
            Self::Routine => b'p',
        }
    }
}

/// The identifier `key` of `class` asks for on its `attempt`-th try.
///
/// FNV-1a over the class tag, the attempt and the key, then a 64-bit
/// finalizer, reduced into the derived range. No part of it depends on the
/// process, the platform or the compiler.
fn hashed_oid(class: OidClass, key: &str, attempt: u32) -> i32 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut feed = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    };
    feed(class.tag());
    for byte in attempt.to_be_bytes() {
        feed(byte);
    }
    for byte in key.bytes() {
        feed(byte);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;
    let span = u64::from((END_DERIVED_OID - FIRST_DERIVED_OID).unsigned_abs());
    let offset = i32::try_from(hash % span).unwrap_or(0);
    FIRST_DERIVED_OID.saturating_add(offset)
}

/// The identifier of `key` when nothing else has claimed it: its plain hash.
pub fn stable_oid(class: OidClass, key: &str) -> i32 {
    hashed_oid(class, key, 0)
}

/// The identifier of a schema. PostgreSQL's fixed numbers for the built-in
/// schemas, the plain hash of the name for every other one.
pub fn schema_oid(schema: &str) -> i32 {
    match schema {
        "pg_catalog" => 11,
        "information_schema" => 13_183,
        "public" => 2200,
        other => stable_oid(OidClass::Schema, other),
    }
}

/// How much of the catalog a producer needs identifiers for. Each level
/// includes the ones before it, and an identifier never depends on a later
/// level, so a producer that stops early agrees with one that does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OidScope {
    /// Tables only.
    Tables,
    /// Tables, then ordinary and vector indexes.
    Indexes,
    /// Tables, indexes, then views.
    Views,
    /// Tables, indexes, views, then sequences: every row of `pg_class`.
    Relations,
}

fn two_part_key(first: &str, second: &str) -> String {
    format!("{first}\u{0}{second}")
}

/// Identifiers of the objects of one database, with collisions resolved.
#[derive(Debug, Default)]
pub struct DictOids {
    used: HashSet<i32>,
    by_key: BTreeMap<(OidClass, String), i32>,
}

impl DictOids {
    /// No object has claimed an identifier yet.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The identifiers of the relations of `storage`, up to `scope`.
    pub fn load(storage: &StorageEngine, scope: OidScope) -> Result<Self> {
        let mut oids = Self::empty();
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        for table in &tables {
            oids.claim(OidClass::Table, table);
        }
        if scope >= OidScope::Indexes {
            for (name, table, _, _) in crate::sql::phase3::system_views::sorted_art_indexes(storage) {
                oids.claim(OidClass::Index, &two_part_key(&table, &name));
            }
            for metadata in crate::sql::phase3::system_views::sorted_vector_indexes(storage) {
                oids.claim(OidClass::VectorIndex, &two_part_key(&metadata.table_name, &metadata.name));
            }
        }
        if scope >= OidScope::Views {
            for (schema, name, _) in crate::sql::phase3::system_views::sorted_views(storage)? {
                oids.claim(OidClass::View, &two_part_key(&schema, &name));
            }
        }
        if scope >= OidScope::Relations {
            let mut sequences: Vec<String> = catalog
                .list_sequences()?
                .into_iter()
                .map(|definition| definition.name.to_lowercase())
                .collect();
            for table in &tables {
                for column in catalog.list_identity_columns(table)? {
                    sequences.push(format!("{table}_{column}_seq").to_lowercase());
                }
            }
            sequences.sort();
            sequences.dedup();
            for sequence in &sequences {
                oids.claim(OidClass::Sequence, sequence);
            }
        }
        Ok(oids)
    }

    /// The identifier of `key`, claiming one if it has none yet. The first
    /// free number of the key's hash sequence is taken.
    pub fn claim(&mut self, class: OidClass, key: &str) -> i32 {
        if let Some(oid) = self.by_key.get(&(class, key.to_string())) {
            return *oid;
        }
        let mut attempt = 0u32;
        let oid = loop {
            let candidate = hashed_oid(class, key, attempt);
            if self.used.insert(candidate) {
                break candidate;
            }
            attempt = attempt.wrapping_add(1);
        };
        self.by_key.insert((class, key.to_string()), oid);
        oid
    }

    /// The identifier `key` has claimed.
    pub fn get(&self, class: OidClass, key: &str) -> Option<i32> {
        self.by_key.get(&(class, key.to_string())).copied()
    }

    /// The identifier of the table with storage key `table`: the exact key,
    /// else the first key that differs only in ASCII case (a foreign key may
    /// spell its target that way).
    pub fn table(&self, table: &str) -> Option<i32> {
        self.get(OidClass::Table, table).or_else(|| {
            self.by_key
                .iter()
                .find(|((class, key), _)| *class == OidClass::Table && key.eq_ignore_ascii_case(table))
                .map(|(_, oid)| *oid)
        })
    }

    /// The table whose identifier is `oid`.
    pub fn table_of(&self, oid: i32) -> Option<&str> {
        self.by_key
            .iter()
            .find(|((class, _), claimed)| *class == OidClass::Table && **claimed == oid)
            .map(|((_, key), _)| key.as_str())
    }

    /// The identifier of the ordinary index `name` of `table`.
    pub fn index(&self, table: &str, name: &str) -> Option<i32> {
        self.get(OidClass::Index, &two_part_key(table, name))
    }

    /// The identifier of the first ordinary index called `name`, whatever its
    /// table, comparing without ASCII case. `0` when there is none: the value
    /// `pg_constraint.conindid` carries for a constraint with no index.
    pub fn index_named(&self, name: &str) -> i32 {
        self.by_key
            .iter()
            .find(|((class, key), _)| {
                *class == OidClass::Index
                    && key
                        .split_once('\u{0}')
                        .is_some_and(|(_, index)| index.eq_ignore_ascii_case(name))
            })
            .map_or(0, |(_, oid)| *oid)
    }

    /// The identifier of the vector index `name` of `table`.
    pub fn vector_index(&self, table: &str, name: &str) -> Option<i32> {
        self.get(OidClass::VectorIndex, &two_part_key(table, name))
    }

    /// The identifier of the view `name` in `schema`.
    pub fn view(&self, schema: &str, name: &str) -> Option<i32> {
        self.get(OidClass::View, &two_part_key(schema, name))
    }

    /// The identifier of the sequence `name`.
    pub fn sequence(&self, name: &str) -> Option<i32> {
        self.get(OidClass::Sequence, &name.to_lowercase())
    }

    /// Claim the identifier of the constraint `name` of `table`.
    pub fn claim_constraint(&mut self, table: &str, name: &str) -> i32 {
        self.claim(OidClass::Constraint, &two_part_key(table, name))
    }

    /// Claim the identifier of a routine. `kind` is `f` for a function and `p`
    /// for a procedure.
    pub fn claim_routine(&mut self, kind: &str, name: &str) -> i32 {
        self.claim(OidClass::Routine, &two_part_key(kind, name))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_in_range_and_fixed() {
        for (class, key) in [
            (OidClass::Table, "orders"),
            (OidClass::Table, ""),
            (OidClass::Index, "orders\u{0}orders_pkey"),
            (OidClass::Schema, "app"),
        ] {
            let oid = stable_oid(class, key);
            assert!((FIRST_DERIVED_OID..END_DERIVED_OID).contains(&oid), "{class:?} {key:?} -> {oid}");
            assert_eq!(oid, stable_oid(class, key));
        }
        assert_ne!(stable_oid(OidClass::Table, "orders"), stable_oid(OidClass::Index, "orders"));
        assert_ne!(stable_oid(OidClass::Table, "orders"), stable_oid(OidClass::Table, "Orders"));
    }

    #[test]
    fn built_in_schemas_keep_their_postgresql_numbers() {
        assert_eq!(schema_oid("pg_catalog"), 11);
        assert_eq!(schema_oid("public"), 2200);
        assert_eq!(schema_oid("information_schema"), 13_183);
        assert!(schema_oid("app") >= FIRST_DERIVED_OID);
        assert_eq!(schema_oid("app"), schema_oid("app"));
    }

    #[test]
    fn an_identifier_does_not_depend_on_the_other_objects() {
        let mut few = DictOids::empty();
        let alone = few.claim(OidClass::Table, "m");
        let mut many = DictOids::empty();
        for name in ["a", "b", "m", "z"] {
            many.claim(OidClass::Table, name);
        }
        assert_eq!(many.table("m"), Some(alone));
        assert_eq!(many.table("M"), Some(alone), "a lookup falls back to ASCII case folding");
        assert_eq!(many.table_of(alone), Some("m"));
        assert_eq!(many.table("missing"), None);
    }

    /// Two keys that ask for the same number: the first to claim keeps its
    /// plain hash, the second takes the next number of its own sequence, and
    /// the outcome is the same every time.
    #[test]
    fn a_collision_is_resolved_the_same_way_every_time() {
        // Find a colliding pair by brute force over a small key space.
        let mut seen = std::collections::HashMap::new();
        let mut pair = None;
        for n in 0u32..400_000 {
            let key = format!("t{n}");
            if let Some(other) = seen.insert(stable_oid(OidClass::Table, &key), key.clone()) {
                pair = Some((other, key));
                break;
            }
        }
        let (first, second) = pair.expect("400000 keys in a range of 1.9e9 collide");
        let resolve = || {
            let mut oids = DictOids::empty();
            let a = oids.claim(OidClass::Table, &first);
            let b = oids.claim(OidClass::Table, &second);
            (a, b)
        };
        let (a, b) = resolve();
        assert_eq!(a, stable_oid(OidClass::Table, &first));
        assert_ne!(a, b, "two objects never share an identifier");
        assert_eq!(b, hashed_oid(OidClass::Table, &second, 1));
        assert_eq!(resolve(), (a, b));
        // Claiming again returns what was claimed.
        let mut oids = DictOids::empty();
        let once = oids.claim(OidClass::Table, &first);
        assert_eq!(oids.claim(OidClass::Table, &first), once);
    }

    #[test]
    fn two_part_keys_do_not_run_together() {
        let mut oids = DictOids::empty();
        let a = oids.claim(OidClass::Index, &two_part_key("ab", "c"));
        let b = oids.claim(OidClass::Index, &two_part_key("a", "bc"));
        assert_ne!(a, b);
        assert_eq!(oids.index("ab", "c"), Some(a));
        assert_eq!(oids.index_named("C"), a);
        assert_eq!(oids.index_named("nothing"), 0);
        let c = oids.claim_constraint("t", "t_pkey");
        assert_eq!(oids.claim_constraint("t", "t_pkey"), c);
        assert_ne!(oids.claim_routine("f", "x"), oids.claim_routine("p", "x"));
    }
}
