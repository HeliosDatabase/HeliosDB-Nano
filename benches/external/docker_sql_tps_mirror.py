#!/usr/bin/env python3
"""Docker-client SQL mirror of tests/tps_workloads.rs for PostgreSQL/MySQL.

This is intentionally stdlib-only. It drives the database client inside an
existing Docker container, which is useful on hosts that do not have psql/mysql
or Python DB drivers installed.

Examples:
  python3 benches/external/docker_sql_tps_mirror.py \
      --backend postgres --container postgres-primary \
      --user helios --password helios --database heliosdb \
      --n 10000 --m 2000

  python3 benches/external/docker_sql_tps_mirror.py \
      --backend mysql --container hdb-sprint-gitea-mysql-db \
      --user gitea --password gitea --database gitea \
      --n 10000 --m 2000

  # Only read/analytics workloads, useful for same-N comparisons.
  python3 benches/external/docker_sql_tps_mirror.py \
      --backend postgres --container postgres-primary \
      --user helios --password helios --database heliosdb \
      --workloads filter_scan,agg_count_sum_avg,group_by_status,join_users_orders,order_by_limit10
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import time
from typing import Iterable, List, Sequence, Tuple


USER_TABLE = "hdb_tps_users"
ORDER_TABLE = "hdb_tps_orders"
BULK_TABLE = "hdb_tps_bulk_users"


def batched(items: Sequence[Tuple[object, ...]], size: int) -> Iterable[Sequence[Tuple[object, ...]]]:
    for start in range(0, len(items), size):
        yield items[start : start + size]


def sql_literal(value: object) -> str:
    if value is None:
        return "NULL"
    if isinstance(value, bool):
        return "TRUE" if value else "FALSE"
    if isinstance(value, (int, float)):
        return str(value)
    text = str(value).replace("'", "''")
    return f"'{text}'"


def insert_rows(table: str, columns: Sequence[str], rows: Sequence[Tuple[object, ...]], batch_size: int) -> str:
    statements: List[str] = []
    column_sql = ", ".join(columns)
    for batch in batched(rows, batch_size):
        values = []
        for row in batch:
            values.append("(" + ", ".join(sql_literal(value) for value in row) + ")")
        statements.append(f"INSERT INTO {table} ({column_sql}) VALUES\n" + ",\n".join(values) + ";")
    return "\n".join(statements)


class DockerSqlClient:
    def __init__(self, backend: str, container: str, user: str, password: str, database: str) -> None:
        self.backend = backend
        self.container = container
        self.user = user
        self.password = password
        self.database = database

    def command(self) -> List[str]:
        if self.backend == "postgres":
            return [
                "docker",
                "exec",
                "-i",
                "-e",
                f"PGPASSWORD={self.password}",
                self.container,
                "psql",
                "-X",
                "-q",
                "-v",
                "ON_ERROR_STOP=1",
                "-U",
                self.user,
                "-d",
                self.database,
            ]
        if self.backend == "mysql":
            return [
                "docker",
                "exec",
                "-i",
                "-e",
                f"MYSQL_PWD={self.password}",
                self.container,
                "mariadb",
                "--batch",
                "--raw",
                "--silent",
                "-u",
                self.user,
                self.database,
            ]
        raise ValueError(f"unsupported backend: {self.backend}")

    def run(self, sql: str, discard_stdout: bool = False) -> None:
        stdout = subprocess.DEVNULL if discard_stdout else subprocess.PIPE
        result = subprocess.run(
            self.command(),
            input=sql.encode(),
            stdout=stdout,
            stderr=subprocess.PIPE,
            check=False,
        )
        if result.returncode != 0:
            stderr = result.stderr.decode(errors="replace")
            raise RuntimeError(f"{self.backend} client failed with exit {result.returncode}:\n{stderr}")


def tx_begin(backend: str) -> str:
    return "START TRANSACTION" if backend == "mysql" else "BEGIN"


def setup_database(client: DockerSqlClient, n: int, batch_size: int) -> None:
    user_rows = [
        (i, f"User{i}", f"u{i}@ex.com", 18 + (i % 60), (i * 7) % 100000)
        for i in range(n)
    ]
    order_rows = [
        (i, i % n, (i * 13) % 5000, "paid" if i % 3 == 0 else "pending")
        for i in range(n * 2)
    ]

    ddl = f"""
DROP TABLE IF EXISTS {ORDER_TABLE};
DROP TABLE IF EXISTS {USER_TABLE};
DROP TABLE IF EXISTS {BULK_TABLE};
CREATE TABLE {USER_TABLE} (
    id INTEGER PRIMARY KEY,
    name TEXT,
    email TEXT,
    age INTEGER,
    balance INTEGER
);
CREATE TABLE {ORDER_TABLE} (
    id INTEGER PRIMARY KEY,
    user_id INTEGER,
    amount INTEGER,
    status TEXT
);
CREATE TABLE {BULK_TABLE} (
    id INTEGER PRIMARY KEY,
    name TEXT,
    email TEXT,
    age INTEGER,
    balance INTEGER
);
"""
    load = "\n".join(
        [
            tx_begin(client.backend) + ";",
            insert_rows(USER_TABLE, ["id", "name", "email", "age", "balance"], user_rows, batch_size),
            insert_rows(ORDER_TABLE, ["id", "user_id", "amount", "status"], order_rows, batch_size),
            "COMMIT;",
        ]
    )
    client.run(ddl + load, discard_stdout=True)


def timed_sql(client: DockerSqlClient, label: str, ops: int, sql: str) -> float:
    start = time.perf_counter()
    client.run(sql, discard_stdout=True)
    secs = time.perf_counter() - start
    print(
        f"{label:<28} {ops:>10} ops  {secs:>9.3f} s  {ops / secs:>14.0f} ops/s  {secs * 1e6 / ops:>10.2f} us/op",
        flush=True,
    )
    return secs


def workload_sql(backend: str, label: str, n: int, m: int, batch_size: int) -> Tuple[int, str]:
    scan_iters = 20
    join_iters = 10

    if label == "bulk_insert_users(txn)":
        rows = [
            (i, f"User{i}", f"u{i}@ex.com", 18 + (i % 60), (i * 7) % 100000)
            for i in range(n)
        ]
        return (
            n,
            f"DELETE FROM {BULK_TABLE};\n"
            + tx_begin(backend)
            + ";\n"
            + insert_rows(BULK_TABLE, ["id", "name", "email", "age", "balance"], rows, batch_size)
            + "\nCOMMIT;\n",
        )

    if label == "autocommit_insert":
        statements = [
            f"INSERT INTO {USER_TABLE} (id, name, email, age, balance) VALUES "
            f"({n + i}, 'AC{n + i}', 'ac{n + i}@ex.com', 33, 500);"
            for i in range(m)
        ]
        return m, "\n".join(statements)

    if label == "point_lookup_pk":
        statements = [
            f"SELECT * FROM {USER_TABLE} WHERE id = {(i * 2654435761) % n};"
            for i in range(m)
        ]
        return m, "\n".join(statements)

    if label == "point_lookup_hot":
        hot_id = min(12345, n - 1)
        statements = [f"SELECT * FROM {USER_TABLE} WHERE id = {hot_id};" for _ in range(m)]
        return m, "\n".join(statements)

    if label == "update_by_pk":
        statements = [
            f"UPDATE {USER_TABLE} SET balance = balance + 1 WHERE id = {(i * 40503) % n};"
            for i in range(m)
        ]
        return m, "\n".join(statements)

    if label == "delete_by_pk":
        statements = [f"DELETE FROM {USER_TABLE} WHERE id = {n + i};" for i in range(m)]
        return m, "\n".join(statements)

    if label == "filter_scan(age>50)":
        return scan_iters, "\n".join(
            f"SELECT id, name FROM {USER_TABLE} WHERE age > 50;" for _ in range(scan_iters)
        )

    if label == "agg_count_sum_avg":
        return scan_iters, "\n".join(
            f"SELECT COUNT(*), SUM(balance), AVG(age) FROM {USER_TABLE};" for _ in range(scan_iters)
        )

    if label == "group_by_status":
        return scan_iters, "\n".join(
            f"SELECT status, COUNT(*), SUM(amount) FROM {ORDER_TABLE} GROUP BY status;" for _ in range(scan_iters)
        )

    if label == "join_users_orders":
        return join_iters, "\n".join(
            f"SELECT u.name, o.amount FROM {USER_TABLE} u "
            f"INNER JOIN {ORDER_TABLE} o ON u.id = o.user_id "
            f"WHERE o.status = 'paid' AND u.age > 40;"
            for _ in range(join_iters)
        )

    if label == "order_by_limit10":
        return scan_iters, "\n".join(
            f"SELECT id, balance FROM {USER_TABLE} ORDER BY balance DESC LIMIT 10;" for _ in range(scan_iters)
        )

    raise ValueError(label)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=["postgres", "mysql"], required=True)
    parser.add_argument("--container", required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--password", required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--n", type=int, default=10_000)
    parser.add_argument("--m", type=int, default=2_000)
    parser.add_argument("--batch-size", type=int, default=500)
    parser.add_argument(
        "--workloads",
        help="Comma-separated workload labels to run; default runs all workloads",
    )
    args = parser.parse_args()

    if args.n <= 0 or args.m < 0 or args.batch_size <= 0:
        print("--n and --batch-size must be positive; --m must be non-negative", file=sys.stderr)
        return 2

    if shutil.which("docker") is None:
        print("docker is required", file=sys.stderr)
        return 2

    client = DockerSqlClient(args.backend, args.container, args.user, args.password, args.database)
    print(f"\n================ {args.backend} Docker TPS mirror ================")
    print(f"container={args.container}  database={args.database}  N={args.n}  M={args.m}")
    print("-" * 80)

    setup_database(client, args.n, args.batch_size)
    default_labels = [
        "bulk_insert_users(txn)",
        "autocommit_insert",
        "point_lookup_pk",
        "point_lookup_hot",
        "update_by_pk",
        "delete_by_pk",
        "filter_scan(age>50)",
        "agg_count_sum_avg",
        "group_by_status",
        "join_users_orders",
        "order_by_limit10",
    ]
    if args.workloads:
        aliases = {label.split("(")[0]: label for label in default_labels}
        labels = []
        for raw_label in args.workloads.split(","):
            label = raw_label.strip()
            label = aliases.get(label, label)
            if label not in default_labels:
                valid = ", ".join(default_labels)
                print(f"unknown workload '{raw_label}'. Valid workloads: {valid}", file=sys.stderr)
                return 2
            labels.append(label)
    else:
        labels = default_labels

    for label in labels:
        ops, sql = workload_sql(args.backend, label, args.n, args.m, args.batch_size)
        timed_sql(client, label, ops, sql)
    print("-" * 80)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
