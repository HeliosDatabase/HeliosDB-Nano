//! Supplemental external draft: a dishonest ART hit must not hide the actual
//! stored owner of a scalar PK. This fixture deliberately misdirects a public
//! ART handle; it does not claim that normal SQL produces that exact state.
//! Public API only, private in-memory databases, no HA singleton/ports/files.
//! No branch/composite/missing-schema policy is asserted here.

use heliosdb_nano::{storage::ArtIndexManager, EmbeddedDatabase, Schema, Tuple, Value};
use uuid::Uuid;

const TABLE: &str = "pk_owner_fixture";
const APIS: [&str; 6] = [
    "get_row_by_pk",
    "get_row_by_pk_with_schema",
    "get_row_by_typed_pk_with_schema",
    "get_row_by_typed_pk_with_schema_no_cache_fill",
    "get_row_by_pk_for_write_with_schema",
    "get_row_by_typed_pk_for_write_with_schema",
];

#[derive(Clone, Copy, Debug)]
enum KeyKind {
    Int,
    BigInt,
    Uuid,
}

struct Fixture {
    db: EmbeddedDatabase,
    schema: Schema,
    owner: Tuple,
    decoy: Tuple,
    typed: Value,
    literal: Value,
}

impl Fixture {
    fn new(kind: KeyKind) -> Self {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let (sql_type, owner_sql, decoy_sql, typed, literal) = match kind {
            KeyKind::Int => ("INT", "11".into(), "22".into(), Value::Int4(11), Value::Int4(11)),
            KeyKind::BigInt => ("BIGINT", "11".into(), "22".into(), Value::Int8(11), Value::Int4(11)),
            KeyKind::Uuid => {
                let owner = Uuid::parse_str("123e4567-e89b-12d3-a456-426614174011").unwrap();
                let decoy = Uuid::parse_str("123e4567-e89b-12d3-a456-426614174022").unwrap();
                (
                    "UUID",
                    format!("'{owner}'"),
                    format!("'{decoy}'"),
                    Value::Uuid(owner),
                    Value::String(owner.to_string()),
                )
            }
        };
        // PK deliberately is NOT column zero; comparing the first value is wrong.
        db.execute(&format!(
            "CREATE TABLE {TABLE} (padding INT, id {sql_type} PRIMARY KEY, label TEXT)"
        ))
        .unwrap();
        db.execute(&format!(
            "INSERT INTO {TABLE} VALUES (101, {owner_sql}, 'actual-owner'), (202, {decoy_sql}, 'wrong-index-target')"
        ))
        .unwrap();
        let schema = db.storage.catalog().get_table_schema(TABLE).unwrap();
        assert!(schema.has_single_column_pk());
        assert_eq!(schema.get_column_index("id"), Some(1));
        // Derive expected IDs from row storage, independently of the PK lookup.
        let rows = db.storage.scan_table_with_schema(TABLE, &schema).unwrap();
        assert_eq!(rows.len(), 2);
        let owner = rows
            .iter()
            .find(|row| row.values.get(1) == Some(&typed))
            .unwrap()
            .clone();
        let decoy = rows
            .iter()
            .find(|row| row.values.get(1) != Some(&typed))
            .unwrap()
            .clone();
        assert_eq!(
            owner.values,
            vec![Value::Int4(101), typed.clone(), Value::String("actual-owner".into())]
        );
        assert_eq!(decoy.values.first(), Some(&Value::Int4(202)));
        assert_eq!(decoy.values.get(2), Some(&Value::String("wrong-index-target".into())));
        assert!(owner.row_id.is_some());
        assert!(decoy.row_id.is_some());
        assert_ne!(owner.row_id, decoy.row_id);
        assert!(
            db.storage.row_cache().is_enabled(),
            "warm-path coverage requires row cache"
        );
        Self {
            db,
            schema,
            owner,
            decoy,
            typed,
            literal,
        }
    }

    fn lookup(&self, api: usize) -> heliosdb_nano::Result<Option<Tuple>> {
        let storage = &self.db.storage;
        match api {
            0 => storage.get_row_by_pk(TABLE, &self.literal),
            1 => storage.get_row_by_pk_with_schema(TABLE, &self.literal, &self.schema),
            2 => storage.get_row_by_typed_pk_with_schema(TABLE, &self.typed, &self.schema),
            3 => storage.get_row_by_typed_pk_with_schema_no_cache_fill(TABLE, &self.typed, &self.schema),
            4 => storage.get_row_by_pk_for_write_with_schema(TABLE, &self.literal, &self.schema),
            5 => storage.get_row_by_typed_pk_for_write_with_schema(TABLE, &self.typed, &self.schema),
            _ => unreachable!(),
        }
    }

    fn cache_state(&self, warm: bool) {
        let cache = self.db.storage.row_cache();
        cache.clear();
        assert!(cache.is_empty());
        if warm {
            let decoy = self
                .db
                .storage
                .get_row_by_id(TABLE, self.decoy.row_id.unwrap(), &self.schema)
                .unwrap()
                .unwrap();
            assert_eq!(decoy.values, self.decoy.values);
            assert_eq!(decoy.row_id, self.decoy.row_id);
            assert_eq!(
                cache.get(TABLE, self.decoy.row_id.unwrap()).unwrap().values,
                self.decoy.values
            );
        }
        assert!(cache.get(TABLE, self.owner.row_id.unwrap()).is_none());
    }

    fn misdirect(&self) {
        let key = ArtIndexManager::encode_key(std::slice::from_ref(&self.typed));
        let index = self.db.storage.art_indexes().get_pk_index(TABLE).unwrap();
        {
            let mut tree = index.write().unwrap();
            assert_eq!(tree.remove(&key).unwrap(), self.owner.row_id);
            tree.insert(&key, self.decoy.row_id.unwrap()).unwrap();
        } // Release the ART lock BEFORE any lookup/scan under test.
        assert_eq!(
            self.db.storage.art_indexes().pk_index_lookup(TABLE, &key),
            self.decoy.row_id
        );
        // Neither backing row changed when the index hint was redirected.
        let rows = self.db.storage.scan_table_with_schema(TABLE, &self.schema).unwrap();
        assert!(rows
            .iter()
            .any(|row| row.row_id == self.owner.row_id && row.values == self.owner.values));
        assert!(rows
            .iter()
            .any(|row| row.row_id == self.decoy.row_id && row.values == self.decoy.values));
    }
}

fn check_existing_owner(kind: KeyKind, warm: bool) {
    let fixture = Fixture::new(kind);
    // Honest-index control also proves literal BIGINT/UUID coercion works in
    // the untyped APIs while typed APIs receive their exact declared variant.
    for (api, name) in APIS.iter().enumerate() {
        fixture.db.storage.row_cache().clear();
        let actual = fixture
            .lookup(api)
            .unwrap()
            .unwrap_or_else(|| panic!("honest control missed: {kind:?} {name}"));
        assert_eq!(actual.values, fixture.owner.values, "honest control: {kind:?} {name}");
        assert_eq!(
            actual.row_id, fixture.owner.row_id,
            "honest control ID: {kind:?} {name}"
        );
    }
    fixture.misdirect();
    let mut failures = Vec::new();
    for (api, name) in APIS.iter().enumerate() {
        fixture.cache_state(warm);
        match fixture.lookup(api) {
            Ok(Some(row)) if row.values == fixture.owner.values && row.row_id == fixture.owner.row_id => (),
            outcome => failures.push(format!(
                "{name}: expected true owner {:?}, got {outcome:?}",
                fixture.owner
            )),
        }
        // A fallback must not republish an index-derived tuple in row cache.
        // A correctly cached old/decoy tuple remains valid for its OWN row ID.
        let cache = fixture.db.storage.row_cache();
        if cache.get(TABLE, fixture.owner.row_id.unwrap()).is_some() {
            failures.push(format!("{name}: mismatch fallback admitted owner into row cache"));
        }
        let decoy_cached = cache.get(TABLE, fixture.decoy.row_id.unwrap());
        if warm {
            if !matches!(decoy_cached, Some(ref row) if row.values == fixture.decoy.values && row.row_id == fixture.decoy.row_id)
            {
                failures.push(format!("{name}: warm legitimate decoy cache entry changed"));
            }
        } else if decoy_cached.is_some() {
            failures.push(format!("{name}: mismatch path filled cold cache with wrong target"));
        }
    }
    assert!(
        failures.is_empty(),
        "kind={kind:?} warm={warm}:\n{}",
        failures.join("\n")
    );
}

#[test]
fn misdirected_nonfirst_int_pk_returns_stored_owner_with_cold_cache() {
    check_existing_owner(KeyKind::Int, false);
}

#[test]
fn misdirected_nonfirst_int_pk_returns_stored_owner_with_warm_cache() {
    check_existing_owner(KeyKind::Int, true);
}

#[test]
fn misdirected_bigint_pk_preserves_literal_coercion_with_cold_cache() {
    check_existing_owner(KeyKind::BigInt, false);
}

#[test]
fn misdirected_bigint_pk_preserves_literal_coercion_with_warm_cache() {
    check_existing_owner(KeyKind::BigInt, true);
}

#[test]
fn misdirected_uuid_pk_preserves_literal_coercion_with_cold_cache() {
    check_existing_owner(KeyKind::Uuid, false);
}

#[test]
fn misdirected_uuid_pk_preserves_literal_coercion_with_warm_cache() {
    check_existing_owner(KeyKind::Uuid, true);
}
