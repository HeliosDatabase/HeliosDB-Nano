#!/usr/bin/env python3
"""JSON-lines Oracle client helper for tests/ora35_benchmark.rs.

The Rust benchmark keeps this process alive and sends one JSON object per line:

  {"op":"execute","sql":"...","commit":true}
  {"op":"query_count","sql":"..."}

Responses are one JSON object per line:

  {"ok":true,"rows":0}
  {"ok":false,"error":"..."}

This intentionally uses python-oracledb thin mode so the benchmark host does
not need an Oracle Instant Client.
"""

from __future__ import annotations

import json
import os
import sys
from typing import Any, Dict

import oracledb


def emit(obj: Dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def main() -> int:
    user = os.environ.get("ORA35_USER", "system")
    password = os.environ.get("ORA35_PASSWORD", "oracle")
    dsn = os.environ.get("ORA35_DSN", "127.0.0.1:21521/FREEPDB1")

    try:
        conn = oracledb.connect(user=user, password=password, dsn=dsn)
        conn.autocommit = False
        cursor = conn.cursor()
        emit({"ok": True, "connected": True})
    except Exception as exc:  # pragma: no cover - reported to Rust harness
        emit({"ok": False, "error": f"connect failed: {exc}"})
        return 0

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
            op = req.get("op")
            if op == "close":
                emit({"ok": True})
                break
            if op == "commit":
                conn.commit()
                emit({"ok": True})
                continue
            if op == "rollback":
                conn.rollback()
                emit({"ok": True})
                continue

            sql = req["sql"]
            if op == "execute":
                cursor.execute(sql)
                if req.get("commit", True):
                    conn.commit()
                emit({"ok": True, "rows": cursor.rowcount if cursor.rowcount >= 0 else 0})
            elif op == "query_count":
                cursor.execute(sql)
                rows = cursor.fetchall()
                emit({"ok": True, "rows": len(rows)})
            else:
                emit({"ok": False, "error": f"unknown op: {op}"})
        except Exception as exc:
            try:
                conn.rollback()
            except Exception:
                pass
            emit({"ok": False, "error": str(exc)})

    try:
        cursor.close()
        conn.close()
    except Exception:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
