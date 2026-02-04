#!/usr/bin/env python3
"""
Example SQLite code with various compatibility issues
This file is used to test the HeliosDB compatibility checker
"""

import sqlite3
from sqlite3 import connect, Connection

# Issue: sqlite3.connect() - CRITICAL
# Should use: EmbeddedDatabase::new()
conn = sqlite3.connect('database.db')
cursor = conn.cursor()

# Issue: ? placeholders - CRITICAL
# Should use: $1, $2, $3 style placeholders
cursor.execute("SELECT * FROM users WHERE id = ?", (1,))
cursor.execute("INSERT INTO users (name, email) VALUES (?, ?)", ('Alice', 'alice@example.com'))

# Issue: Multiple ? placeholders
cursor.execute("""
    UPDATE users
    SET name = ?, email = ?, updated_at = ?
    WHERE id = ?
""", ('Bob', 'bob@example.com', '2025-01-01', 2))

# Issue: last_insert_rowid() - WARNING
# Should use: RETURNING clause
cursor.execute("INSERT INTO posts (title, content) VALUES (?, ?)", ('Title', 'Content'))
last_id = cursor.lastrowid  # Not directly available in HeliosDB

# Dynamic typing issue - WARNING
# SQLite allows flexible types, HeliosDB requires strict typing
cursor.execute("INSERT INTO flexible_table (value) VALUES (?)", ('123',))  # String instead of int
cursor.execute("INSERT INTO flexible_table (value) VALUES (?)", (123,))    # Int

# Using PRAGMA - WARNING
# Should use: HeliosDB configuration
cursor.execute("PRAGMA foreign_keys = ON")
cursor.execute("PRAGMA journal_mode = WAL")
cursor.execute("PRAGMA synchronous = NORMAL")

# Query with sqlite_version() - INFO
cursor.execute("SELECT sqlite_version()")

# Batch operations
users = [
    ('User1', 'user1@example.com'),
    ('User2', 'user2@example.com'),
    ('User3', 'user3@example.com'),
]
cursor.executemany("INSERT INTO users (name, email) VALUES (?, ?)", users)

# Transaction handling
conn.execute("BEGIN TRANSACTION")
cursor.execute("UPDATE accounts SET balance = balance - ? WHERE id = ?", (100, 1))
cursor.execute("UPDATE accounts SET balance = balance + ? WHERE id = ?", (100, 2))
conn.commit()

# Using context manager
with sqlite3.connect('database.db') as conn:
    cursor = conn.cursor()
    cursor.execute("SELECT * FROM users WHERE status = ?", ('active',))
    results = cursor.fetchall()

# Row factory usage
conn.row_factory = sqlite3.Row
cursor = conn.cursor()
cursor.execute("SELECT * FROM users WHERE id = ?", (1,))
row = cursor.fetchone()

# Backup database
backup_conn = sqlite3.connect('backup.db')
conn.backup(backup_conn)

# Close connections
cursor.close()
conn.close()


class DatabaseManager:
    """Example class using sqlite3"""

    def __init__(self, db_path: str):
        # Issue: sqlite3.connect()
        self.conn = sqlite3.connect(db_path)
        self.cursor = self.conn.cursor()

    def get_user(self, user_id: int):
        # Issue: ? placeholder
        self.cursor.execute("SELECT * FROM users WHERE id = ?", (user_id,))
        return self.cursor.fetchone()

    def create_user(self, name: str, email: str):
        # Issue: ? placeholders and lastrowid
        self.cursor.execute(
            "INSERT INTO users (name, email) VALUES (?, ?)",
            (name, email)
        )
        self.conn.commit()
        return self.cursor.lastrowid

    def update_user(self, user_id: int, name: str, email: str):
        # Issue: ? placeholders
        self.cursor.execute(
            "UPDATE users SET name = ?, email = ? WHERE id = ?",
            (name, email, user_id)
        )
        self.conn.commit()

    def delete_user(self, user_id: int):
        # Issue: ? placeholder
        self.cursor.execute("DELETE FROM users WHERE id = ?", (user_id,))
        self.conn.commit()

    def close(self):
        self.cursor.close()
        self.conn.close()


# This should be migrated to:
# use heliosdb_lite::EmbeddedDatabase;
# let db = EmbeddedDatabase::new("database.db")?;
# let results = db.query("SELECT * FROM users WHERE id = $1", &[&1])?;
