# Proposal: PyO3 binding for `EmbeddedDatabase` (issue #1)

Status: **IMPLEMENTED in v3.33.0** — `bindings/python` (the `#[pymodule]`, abi3 wheel),
the `query_params_with_columns` core method, and a projection-aware prefix-decode scan
optimization all shipped. The plan below is preserved as the design record.

**Measured outcome (real 448k-row dir).** In-process beats PG-wire — the access mode the
dashboard's cutover uses today — by 1.5–4.7×: `COUNT(*)` 715→153 ms, `COUNT(DISTINCT
session_id)` 1489→725 ms, `WHERE type=…` 1312→852 ms, `GROUP BY+SUM` 1439→902 ms. It is
**not** sqlite-competitive on full-table aggregates (sqlite: 1.7–145 ms). The binding and
the prefix decode confirmed, by measurement, that the access mode was *not* the dominant
cost: a row store reads and materializes the whole row per scan, so even decoding only the
referenced column prefix (~25% on `COUNT(DISTINCT)`) leaves Nano well above sqlite. True
parity needs **columnar scans** — see `PROPOSAL_COLUMNAR_STORAGE.md`. The binding remains
the right access layer; columnar storage is the orthogonal lever underneath it.

Closes the latency gap reported in [issue #1](https://github.com/dimensigon/HDB-HeliosDB-Nano/issues/1):
the Token-Dashboard team's only Python-accessible "embedded" mode today is the
`heliosdb-nano repl` subprocess (stdin/stdout pipe), which loses to `sqlite3` by
3–100× on simple aggregates because of IPC + a text protocol + Python-side parsing.
The fix is a thin **in-process** PyO3 binding so `EmbeddedDatabase::new(path)` and
`db.query(sql)` run inside the Python process with no pipe and no serialisation hop.

This document is the build plan: it inventories what the Rust API already gives us,
identifies the one missing method, and provides drop-in scaffold (crate, module,
packaging) so the work is mechanical.

---

## 1. What we already have

`EmbeddedDatabase` (in `src/lib.rs`) already exposes everything the binding needs
except one column-aware query variant. Verified signatures:

| Rust method | Signature | Python use |
|---|---|---|
| `new` | `new(path: impl AsRef<Path>) -> Result<Self>` | `EmbeddedDatabase(path)` |
| `new_in_memory` | `new_in_memory() -> Result<Self>` | `EmbeddedDatabase.in_memory()` |
| `execute` | `execute(&self, sql) -> Result<u64>` | `db.execute(sql) -> rowcount` |
| `execute_params` | `execute_params(&self, sql, &[Value]) -> Result<u64>` | `db.execute(sql, params)` |
| `execute_params_returning` | `-> Result<(u64, Vec<Tuple>)>` | `db.execute(..., returning=True)` |
| `query_with_columns` | `query_with_columns(&self, sql) -> Result<(Vec<Tuple>, Vec<String>)>` | `db.query(sql)` (no params) |
| `query_params` | `query_params(&self, sql, &[Value]) -> Result<Vec<Tuple>>` | rows only — **no column names** |
| `flush` | `flush(&self) -> Result<()>` | `db.flush()` |
| `create_vector_store` | `(&self, name, dims: u32) -> Result<VectorStoreInfo>` | `db.create_vector_store(...)` |
| `insert_vectors` | `(&self, store, Vec<Vec<f32>>) -> Result<Vec<String>>` | `db.insert_vectors(...)` |
| `upsert_vectors` | `(&self, store, Vec<(String, Vec<f32>)>) -> Result<()>` | `db.upsert_vectors(...)` |
| `search_vectors` | `(&self, store, query: Vec<f32>, k: usize) -> Result<Vec<(String, f32)>>` | `db.vector_search(...)` |
| `fetch_vectors` / `delete_vectors` / `list_vector_stores` | … | optional surface |

`EmbeddedDatabase` holds its state behind `Arc<StorageEngine>` and an
`Arc<Mutex<…>>` transaction slot, and it is already shared with the MV
auto-refresh background worker thread (`src/lib.rs` ~9215) — so it is `Send + Sync`.
The binding adds a `const _: fn() = || { fn assert<T: Send + Sync>(){} assert::<EmbeddedDatabase>(); };`
to make that a compile-time guarantee.

## 2. The one gap to close first (small, in `src/lib.rs`)

Python's `db.query(sql, params)` must return **rows _and_ column names** so we can
build `list[dict]`. `query_with_columns` has the columns but takes no params;
`query_params` takes params but drops the columns. Add the union — it is a thin
reuse of the two existing code paths, no new logic:

```rust
/// Like `query_with_columns`, but with `$1..$n` parameter binding.
/// Returns the result rows alongside their output column names.
pub fn query_params_with_columns(
    &self,
    sql: &str,
    params: &[Value],
) -> Result<(Vec<Tuple>, Vec<String>)> {
    // column-name extraction is identical to query_with_columns; row production
    // is identical to query_params. Factor the shared planner/executor call so
    // both entry points share it (no behavioural change to existing methods).
}
```

Everything else below depends only on already-public methods.

## 3. Crate layout — separate workspace member (recommended)

Keep PyO3 out of the core crate's dependency graph (it must stay embeddable with
zero Python). Add a workspace member:

```
HDB/Nano/
├── Cargo.toml                  # [workspace] members = [".", "bindings/python"]
├── src/...                     # core crate, unchanged except §2
└── bindings/python/
    ├── Cargo.toml
    ├── pyproject.toml
    ├── src/lib.rs              # the #[pymodule]
    └── tests/test_smoke.py
```

`Cargo.toml` (root) — add the member:

```toml
[workspace]
members = [".", "bindings/python"]
```

`bindings/python/Cargo.toml`:

```toml
[package]
name = "heliosdb-nano-py"
version = "0.1.0"
edition = "2021"

[lib]
name = "heliosdb_nano"      # the importable Python module name
crate-type = ["cdylib"]

[dependencies]
heliosdb-nano = { path = "../.." }            # default features; add features as needed
pyo3 = { version = "0.22", features = ["extension-module", "abi3-py38"] }
```

`abi3-py38` builds **one** wheel that works on CPython ≥ 3.8 — no per-version matrix.

> Alternative considered: a `pyo3` feature flag on the core crate. Rejected — it
> would pull `pyo3` into the default resolver for embedders and entangle the
> `cdylib` crate-type with the core `rlib`. A member crate is cleaner and is the
> maturin-standard layout.

## 4. The module (`bindings/python/src/lib.rs`)

```rust
use heliosdb_nano::{EmbeddedDatabase, Value, Tuple};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};
use pyo3::exceptions::PyRuntimeError;

const _: fn() = || { fn a<T: Send + Sync + 'static>() {} a::<EmbeddedDatabase>(); };

#[pyclass(name = "EmbeddedDatabase", module = "heliosdb_nano")]
struct PyDatabase { inner: EmbeddedDatabase }

fn err<E: std::fmt::Display>(e: E) -> PyErr { PyRuntimeError::new_err(e.to_string()) }

/// Python object -> engine Value (parameter binding).
fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() { return Ok(Value::Null); }
    if let Ok(b) = obj.extract::<bool>()    { return Ok(Value::Boolean(b)); }
    if let Ok(i) = obj.extract::<i64>()     { return Ok(Value::Int8(i)); }
    if let Ok(f) = obj.extract::<f64>()     { return Ok(Value::Float8(f)); }
    if let Ok(s) = obj.extract::<String>()  { return Ok(Value::String(s)); }
    if let Ok(b) = obj.downcast::<PyBytes>(){ return Ok(Value::Bytes(b.as_bytes().to_vec())); }
    if let Ok(v) = obj.extract::<Vec<f32>>(){ return Ok(Value::Vector(v)); }   // embeddings
    Err(err(format!("unsupported parameter type: {}", obj.get_type())))
}

/// engine Value -> Python object (row output). Internal storage refs are already
/// resolved by the scan path before they reach here.
fn value_to_py(py: Python<'_>, v: &Value) -> PyObject {
    match v {
        Value::Null => py.None(),
        Value::Boolean(b) => b.into_py(py),
        Value::Int2(n) => n.into_py(py),
        Value::Int4(n) => n.into_py(py),
        Value::Int8(n) => n.into_py(py),
        Value::Float4(f) => f.into_py(py),
        Value::Float8(f) => f.into_py(py),
        Value::Numeric(s) | Value::String(s) | Value::Json(s) => s.into_py(py),
        Value::Bytes(b) => PyBytes::new_bound(py, b).into_py(py),
        Value::Uuid(u) => u.to_string().into_py(py),
        Value::Timestamp(t) => t.to_rfc3339().into_py(py),
        Value::Date(d) => d.to_string().into_py(py),
        Value::Time(t) => t.to_string().into_py(py),
        Value::Interval(us) => us.into_py(py),
        Value::Vector(vec) => vec.clone().into_py(py),
        Value::Array(items) => {
            let l = PyList::empty_bound(py);
            for it in items { l.append(value_to_py(py, it)).unwrap(); }
            l.into_py(py)
        }
        // DictRef/CasRef/ColumnarRef are resolved upstream; fall back to None.
        _ => py.None(),
    }
}

fn rows_to_dicts(py: Python<'_>, rows: &[Tuple], cols: &[String]) -> PyObject {
    let out = PyList::empty_bound(py);
    for row in rows {
        let d = PyDict::new_bound(py);
        for (i, col) in cols.iter().enumerate() {
            let v = row.values.get(i).map(|v| value_to_py(py, v)).unwrap_or_else(|| py.None());
            d.set_item(col, v).unwrap();
        }
        out.append(d).unwrap();
    }
    out.into_py(py)
}

fn collect_params(params: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<Value>> {
    let Some(p) = params else { return Ok(vec![]) };
    if p.is_none() { return Ok(vec![]); }
    let seq = p.downcast::<PyTuple>().map(|t| t.to_list()).or_else(|_| p.downcast::<PyList>().cloned())
        .map_err(|_| err("params must be a tuple or list"))?;
    seq.iter().map(|o| py_to_value(&o)).collect()
}

#[pymethods]
impl PyDatabase {
    #[new]
    fn new(path: String) -> PyResult<Self> {
        Ok(Self { inner: EmbeddedDatabase::new(path).map_err(err)? })
    }

    #[staticmethod]
    fn in_memory() -> PyResult<Self> {
        Ok(Self { inner: EmbeddedDatabase::new_in_memory().map_err(err)? })
    }

    /// SELECT → list[dict]. Optional positional params bind to $1..$n.
    #[pyo3(signature = (sql, params = None))]
    fn query(&self, py: Python<'_>, sql: &str, params: Option<&Bound<'_, PyAny>>) -> PyResult<PyObject> {
        let ps = collect_params(params)?;
        let (rows, cols) = py.allow_threads(|| {
            if ps.is_empty() { self.inner.query_with_columns(sql) }
            else { self.inner.query_params_with_columns(sql, &ps) }   // §2
        }).map_err(err)?;
        Ok(rows_to_dicts(py, &rows, &cols))
    }

    /// DDL/DML → affected row count. Optional positional params.
    #[pyo3(signature = (sql, params = None))]
    fn execute(&self, py: Python<'_>, sql: &str, params: Option<&Bound<'_, PyAny>>) -> PyResult<u64> {
        let ps = collect_params(params)?;
        py.allow_threads(|| {
            if ps.is_empty() { self.inner.execute(sql) } else { self.inner.execute_params(sql, &ps) }
        }).map_err(err)
    }

    /// Batch DML: one SQL, many parameter rows. Returns total affected.
    fn execute_many(&self, py: Python<'_>, sql: &str, rows: &Bound<'_, PyList>) -> PyResult<u64> {
        let batches: Vec<Vec<Value>> = rows.iter().map(|r| collect_params(Some(&r))).collect::<PyResult<_>>()?;
        py.allow_threads(|| {
            let mut n = 0;
            for ps in &batches { n += self.inner.execute_params(sql, ps)?; }
            Ok::<u64, heliosdb_nano::Error>(n)
        }).map_err(err)
    }

    /// HNSW search → list[(id, distance)].
    fn vector_search(&self, py: Python<'_>, store: &str, query: Vec<f32>, k: usize) -> PyResult<Vec<(String, f32)>> {
        py.allow_threads(|| self.inner.search_vectors(store, query, k)).map_err(err)
    }

    fn create_vector_store(&self, name: &str, dims: u32) -> PyResult<()> {
        self.inner.create_vector_store(name, dims).map(|_| ()).map_err(err)
    }
    fn insert_vectors(&self, py: Python<'_>, store: &str, vectors: Vec<Vec<f32>>) -> PyResult<Vec<String>> {
        py.allow_threads(|| self.inner.insert_vectors(store, vectors)).map_err(err)
    }
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| self.inner.flush()).map_err(err)
    }
}

#[pymodule]
fn heliosdb_nano(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDatabase>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
```

Notes that matter for correctness/perf:
- **`py.allow_threads`** wraps every engine call so the GIL is released while Rust
  runs — multiple Python threads can query concurrently (the engine is `Sync`).
- **Errors** map `heliosdb_nano::Error` → `RuntimeError` via `Display`. A Phase-2
  refinement is a `heliosdb_nano.DatabaseError` subclass hierarchy.
- **No subprocess, no text protocol** — this is the entire point; rows cross the
  boundary as native Python objects built directly from `Value`.

## 5. Packaging (`bindings/python/pyproject.toml`)

```toml
[build-system]
requires = ["maturin>=1.5,<2.0"]
build-backend = "maturin"

[project]
name = "heliosdb-nano"
requires-python = ">=3.8"
dynamic = ["version"]

[tool.maturin]
features = ["pyo3/extension-module"]
manifest-path = "bindings/python/Cargo.toml"
```

Build/dev/publish:

```bash
pip install maturin
maturin develop -m bindings/python/Cargo.toml      # editable install into the active venv
maturin build  --release -m bindings/python/Cargo.toml   # abi3 wheel in target/wheels/
# maturin publish  (PyPI) — single abi3 wheel per platform
```

## 6. Python usage (matches the issue's requested surface)

```python
import heliosdb_nano

db = heliosdb_nano.EmbeddedDatabase("/path/to/data")

db.query("SELECT COUNT(*) AS n FROM dashboard.messages")          # [{'n': 448573}]
db.query("SELECT * FROM messages WHERE session_id = $1", ("abc",))# param-bound
db.execute_many(
    "INSERT INTO messages (uuid, body) VALUES ($1, $2)",
    [(u1, b1), (u2, b2)],
)
db.vector_search("messages_body_vec", query_vec, k=20)            # [(id, dist), ...]
```

## 7. Phasing

- **Phase 1 (unblocks the cutover):** §2 method + `new` / `in_memory` / `query` /
  `execute` / `execute_many` / `flush`. Ship an abi3 wheel. This alone removes the
  IPC + text-protocol overhead the dashboard measured.
- **Phase 2:** `vector_search` + vector-store management, `RETURNING` via
  `execute_params_returning`, richer type mapping (real `datetime`/`uuid`/`Decimal`
  objects instead of strings), a typed exception hierarchy.
- **Phase 3:** PyPI publish + CI wheels (`maturin-action`) for linux/macos/windows,
  `.pyi` type stubs, a `pytest` suite mirroring `tests/mv_j_real_repro.rs` semantics.

## 8. Expected outcome

In-process calls eliminate the subprocess pipe and the REPL text protocol entirely,
so Python sees the Rust-level path — the same one behind the "embedded beats SQLite"
claim. The dashboard's hot-path aggregates (`COUNT(*)`, `COUNT(DISTINCT)`,
`GROUP BY … SUM`) should land in the single-digit-millisecond range rather than the
300 ms+ REPL floor, and vector search keeps its sub-20 ms HNSW advantage with no
serialisation tax. That closes the gap that parked the primary-store cutover.

## 9. Open questions for the Token-Dashboard team

1. Row shape preference: `list[dict]` (shown here) vs `list[tuple]` + a separate
   `.columns`? Dicts are friendlier; tuples are marginally faster for wide rows.
2. Do you need a DB-API 2.0 (`cursor`/`fetchone`/`fetchmany`) compatibility shim, or
   is the direct `query`/`execute` surface enough to drop into `db_helios.py`?
3. Is abi3-py38 the right floor, or do you need 3.7?
