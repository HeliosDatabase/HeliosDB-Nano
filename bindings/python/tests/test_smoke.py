"""Smoke tests for the heliosdb_nano PyO3 binding. Run via `pytest` after
`maturin develop -m bindings/python/Cargo.toml`."""

import heliosdb_nano


def test_module_surface():
    assert hasattr(heliosdb_nano, "EmbeddedDatabase")
    assert isinstance(heliosdb_nano.__version__, str)


def test_basic_crud_and_query():
    db = heliosdb_nano.EmbeddedDatabase.in_memory()
    db.execute("CREATE TABLE t (id INT, name TEXT)")
    assert db.execute("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'a')") == 3
    rows = db.query("SELECT id, name FROM t ORDER BY id")
    assert rows == [
        {"id": 1, "name": "a"},
        {"id": 2, "name": "b"},
        {"id": 3, "name": "a"},
    ]


def test_param_binding():
    db = heliosdb_nano.EmbeddedDatabase.in_memory()
    db.execute("CREATE TABLE t (id INT, name TEXT)")
    db.execute("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'a')")
    rows = db.query("SELECT id FROM t WHERE name = $1 ORDER BY id", ("a",))
    assert [r["id"] for r in rows] == [1, 3]
    cnt = db.query("SELECT COUNT(*) AS n FROM t WHERE name = $1", ("a",))
    assert cnt[0]["n"] == 2


def test_execute_many():
    db = heliosdb_nano.EmbeddedDatabase.in_memory()
    db.execute("CREATE TABLE m (id INT, v INT)")
    total = db.execute_many("INSERT INTO m VALUES ($1, $2)", [(1, 10), (2, 20), (3, 30)])
    assert total == 3
    assert db.query("SELECT SUM(v) AS s FROM m")[0]["s"] == 60


def test_types_roundtrip():
    db = heliosdb_nano.EmbeddedDatabase.in_memory()
    db.execute("CREATE TABLE typ (b BOOLEAN, f DOUBLE PRECISION, s TEXT)")
    db.execute("INSERT INTO typ VALUES ($1, $2, $3)", (True, 3.5, "hi"))
    db.flush()
    row = db.query("SELECT b, f, s FROM typ")[0]
    assert row["b"] == True  # noqa: E712
    assert row["f"] == 3.5
    assert row["s"] == "hi"


def test_vectors():
    db = heliosdb_nano.EmbeddedDatabase.in_memory()
    db.create_vector_store("vs", 3)
    ids = db.insert_vectors("vs", [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]])
    assert len(ids) == 3
    res = db.vector_search("vs", [1.0, 0.0, 0.0], 1)
    assert len(res) == 1
    # Nearest neighbour of [1,0,0] is the first inserted vector.
    assert res[0][0] == ids[0]


def test_bad_params_raise():
    db = heliosdb_nano.EmbeddedDatabase.in_memory()
    db.execute("CREATE TABLE t (id INT)")
    try:
        db.query("SELECT * FROM t WHERE id = $1", 5)  # not a tuple/list
    except TypeError:
        pass
    else:
        raise AssertionError("expected TypeError for non-sequence params")
