//! In-process PyO3 binding for HeliosDB-Nano's `EmbeddedDatabase` (issue #1).
//!
//! Wraps the Rust embedded API directly so Python clients avoid the `heliosdb-nano
//! repl` subprocess pipe + text protocol that loses to sqlite3 on aggregates. Every
//! engine call releases the GIL (`detach`), so multiple Python threads can run
//! queries concurrently — sound because `EmbeddedDatabase` is `Send + Sync`.

use heliosdb_core::{EmbeddedDatabase, Tuple, Value};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};
use pyo3::IntoPyObjectExt;

// Compile-time proof of the property that makes GIL release sound. If the core crate
// ever makes EmbeddedDatabase non-Send/Sync, this fails to build instead of silently
// becoming unsound.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<EmbeddedDatabase>();
};

fn rt_err<E: std::fmt::Display>(e: E) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Python object -> engine `Value`, for `$n` parameter binding.
///
/// Order matters: `bool` is checked before `int` (Python `bool` is an `int`
/// subclass) and `int` before `float` (so integers don't become floats).
fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.extract::<bool>() {
        return Ok(Value::Boolean(b));
    }
    if let Ok(i) = obj.extract::<i64>() {
        return Ok(Value::Int8(i));
    }
    if let Ok(f) = obj.extract::<f64>() {
        return Ok(Value::Float8(f));
    }
    if let Ok(s) = obj.extract::<String>() {
        return Ok(Value::String(s));
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(Value::Bytes(b.as_bytes().to_vec()));
    }
    if let Ok(v) = obj.extract::<Vec<f32>>() {
        return Ok(Value::Vector(v)); // embeddings: list[float]
    }
    Err(PyTypeError::new_err(format!(
        "unsupported parameter type: {}",
        obj.get_type()
    )))
}

/// engine `Value` -> Python object, for row output.
fn value_to_py(py: Python<'_>, v: &Value) -> PyResult<Py<PyAny>> {
    match v {
        Value::Null => Ok(py.None()),
        Value::Boolean(b) => (*b).into_py_any(py),
        Value::Int2(n) => (*n).into_py_any(py),
        Value::Int4(n) => (*n).into_py_any(py),
        Value::Int8(n) => (*n).into_py_any(py),
        Value::Float4(f) => (*f).into_py_any(py),
        Value::Float8(f) => (*f).into_py_any(py),
        Value::Numeric(s) | Value::String(s) | Value::Json(s) => s.as_str().into_py_any(py),
        Value::Bytes(b) => PyBytes::new(py, b).into_py_any(py),
        Value::Uuid(u) => u.to_string().into_py_any(py),
        Value::Timestamp(t) => t.to_rfc3339().into_py_any(py),
        Value::Date(d) => d.to_string().into_py_any(py),
        Value::Time(t) => t.to_string().into_py_any(py),
        Value::Interval(us) => (*us).into_py_any(py),
        Value::Vector(vec) => vec.as_slice().into_py_any(py),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for it in items {
                list.append(value_to_py(py, it)?)?;
            }
            list.into_py_any(py)
        }
        // DictRef / CasRef / ColumnarRef are resolved by the scan path before output.
        _ => Ok(py.None()),
    }
}

fn rows_to_dicts(py: Python<'_>, rows: &[Tuple], cols: &[String]) -> PyResult<Py<PyAny>> {
    let out = PyList::empty(py);
    for row in rows {
        let d = PyDict::new(py);
        for (i, col) in cols.iter().enumerate() {
            let v = match row.values.get(i) {
                Some(v) => value_to_py(py, v)?,
                None => py.None(),
            };
            d.set_item(col, v)?;
        }
        out.append(d)?;
    }
    out.into_py_any(py)
}

/// Accept a Python tuple/list of params (or `None`) as `Vec<Value>`.
fn collect_params(params: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<Value>> {
    let Some(p) = params else { return Ok(Vec::new()) };
    if p.is_none() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    if let Ok(list) = p.cast::<PyList>() {
        for item in list.iter() {
            out.push(py_to_value(&item)?);
        }
    } else if let Ok(tup) = p.cast::<PyTuple>() {
        for item in tup.iter() {
            out.push(py_to_value(&item)?);
        }
    } else {
        return Err(PyTypeError::new_err("params must be a tuple or list"));
    }
    Ok(out)
}

#[pyclass(name = "EmbeddedDatabase", module = "heliosdb_nano")]
struct PyDatabase {
    inner: EmbeddedDatabase,
}

#[pymethods]
impl PyDatabase {
    /// Open (or create) a database at `path`.
    #[new]
    fn new(path: String) -> PyResult<Self> {
        Ok(Self {
            inner: EmbeddedDatabase::new(path).map_err(rt_err)?,
        })
    }

    /// Open an ephemeral in-memory database.
    #[staticmethod]
    fn in_memory() -> PyResult<Self> {
        Ok(Self {
            inner: EmbeddedDatabase::new_in_memory().map_err(rt_err)?,
        })
    }

    /// Open an EXISTING database READ-ONLY. A separate process can read while
    /// another process holds the same directory open for writing (RocksDB
    /// read-only opens take no lock) — e.g. a status/monitor reader alongside a
    /// writer. Writes through this handle raise. The handle sees the on-disk
    /// state as of open; reopen to observe a writer's later commits.
    #[staticmethod]
    fn open_read_only(path: String) -> PyResult<Self> {
        Ok(Self {
            inner: EmbeddedDatabase::open_read_only(path).map_err(rt_err)?,
        })
    }

    /// Run a query and return rows as `list[dict]`. Optional positional `params`
    /// bind to `$1..$n`.
    #[pyo3(signature = (sql, params = None))]
    fn query(&self, py: Python<'_>, sql: &str, params: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        let ps = collect_params(params)?;
        let (rows, cols) = py
            .detach(|| self.inner.query_params_with_columns(sql, &ps))
            .map_err(rt_err)?;
        rows_to_dicts(py, &rows, &cols)
    }

    /// Execute DDL/DML and return the affected row count. Optional positional params.
    #[pyo3(signature = (sql, params = None))]
    fn execute(&self, py: Python<'_>, sql: &str, params: Option<&Bound<'_, PyAny>>) -> PyResult<u64> {
        let ps = collect_params(params)?;
        py.detach(|| {
            if ps.is_empty() {
                self.inner.execute(sql)
            } else {
                self.inner.execute_params(sql, &ps)
            }
        })
        .map_err(rt_err)
    }

    /// Execute one statement against many parameter rows; returns total affected.
    fn execute_many(&self, py: Python<'_>, sql: &str, rows: &Bound<'_, PyList>) -> PyResult<u64> {
        let batches: Vec<Vec<Value>> = rows.iter().map(|r| collect_params(Some(&r))).collect::<PyResult<_>>()?;
        py.detach(|| self.inner.execute_many_params(sql, &batches))
            .map_err(rt_err)
    }

    /// HNSW similarity search. Returns `list[(id, distance)]`.
    fn vector_search(&self, py: Python<'_>, store: &str, query: Vec<f32>, k: usize) -> PyResult<Vec<(String, f32)>> {
        py.detach(|| self.inner.search_vectors(store, query, k)).map_err(rt_err)
    }

    /// Create a vector store with the given dimensionality.
    fn create_vector_store(&self, name: &str, dimensions: u32) -> PyResult<()> {
        self.inner
            .create_vector_store(name, dimensions)
            .map(|_| ())
            .map_err(rt_err)
    }

    /// Bulk-insert vectors; returns the generated ids.
    fn insert_vectors(&self, py: Python<'_>, store: &str, vectors: Vec<Vec<f32>>) -> PyResult<Vec<String>> {
        py.detach(|| self.inner.insert_vectors(store, vectors)).map_err(rt_err)
    }

    /// Force the memtable to flush to SST.
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.flush()).map_err(rt_err)
    }

    fn __repr__(&self) -> String {
        "<heliosdb_nano.EmbeddedDatabase>".to_string()
    }
}

#[pymodule]
fn heliosdb_nano(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDatabase>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
