# heliosdb-nano (Python)

In-process Python binding for [HeliosDB-Nano](https://github.com/dimensigon/HDB-HeliosDB-Nano)'s
embedded database. Calls the Rust `EmbeddedDatabase` API directly — **no subprocess,
no wire protocol, no serialization hop** — so Python sees the same path behind the
"embedded beats SQLite" numbers.

```python
import heliosdb_nano

db = heliosdb_nano.EmbeddedDatabase("/path/to/data")   # or .in_memory()

db.execute("CREATE TABLE messages (id INT, session_id TEXT, body TEXT)")
db.execute_many(
    "INSERT INTO messages (id, session_id, body) VALUES ($1, $2, $3)",
    [(1, "s1", "hi"), (2, "s1", "yo"), (3, "s2", "hey")],
)

db.query("SELECT COUNT(DISTINCT session_id) AS n FROM messages")   # [{'n': 2}]
db.query("SELECT * FROM messages WHERE session_id = $1", ("s1",))  # [{...}, {...}]

# Vector search (HNSW)
db.create_vector_store("emb", 3)
ids = db.insert_vectors("emb", [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]])
db.vector_search("emb", [1.0, 0.0, 0.0], k=1)                      # [(id, distance)]
```

## API

| method | returns |
|---|---|
| `EmbeddedDatabase(path)` / `EmbeddedDatabase.in_memory()` | a handle |
| `query(sql, params=None)` | `list[dict]` |
| `execute(sql, params=None)` | affected row count (`int`) |
| `execute_many(sql, rows)` | total affected (`int`) |
| `vector_search(store, query, k)` | `list[(id, distance)]` |
| `create_vector_store(name, dimensions)` / `insert_vectors(store, vectors)` | — / `list[id]` |
| `flush()` | — |

`params` is a tuple/list binding positionally to `$1..$n`. The GIL is released
around every engine call, so multiple Python threads can query concurrently.

## Build from source

```bash
pip install maturin
maturin develop -m bindings/python/Cargo.toml      # editable install into the active venv
maturin build  --release -m bindings/python/Cargo.toml   # abi3 wheel in target/wheels/
```
