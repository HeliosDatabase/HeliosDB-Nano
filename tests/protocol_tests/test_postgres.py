#!/usr/bin/env python3
"""
PostgreSQL Protocol Test Suite for HeliosDB Nano

Tests PostgreSQL wire protocol implementation with CRUD operations,
session tracking, and protocol-specific features.
"""

import psycopg2
import sys
from datetime import datetime


def test_postgresql():
    """Test PostgreSQL protocol with CRUD operations"""

    print("=" * 60)
    print("PostgreSQL Protocol Test Suite")
    print("=" * 60)

    try:
        # Connect
        print("\n[1/7] Connecting to HeliosDB via PostgreSQL protocol...")
        conn = psycopg2.connect(
            host='localhost',
            port=20000,
            database='heliosdb',
            user='test_user',
            password='test_pass',
            sslmode='disable'
        )
        conn.autocommit = False
        cursor = conn.cursor()
        print("✓ Connection established")

        # CREATE
        print("\n[2/7] Creating test table...")
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_users (
                id SERIAL PRIMARY KEY,
                name TEXT NOT NULL,
                email TEXT NOT NULL,
                age INTEGER,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
        """)
        conn.commit()
        print("✓ Table 'test_users' created")

        # INSERT
        print("\n[3/7] Inserting test data...")
        cursor.execute("""
            INSERT INTO test_users (name, email, age)
            VALUES (%s, %s, %s) RETURNING id
        """, ('Alice Johnson', 'alice@example.com', 30))
        alice_id = cursor.fetchone()[0]

        cursor.execute("""
            INSERT INTO test_users (name, email, age)
            VALUES (%s, %s, %s) RETURNING id
        """, ('Bob Smith', 'bob@example.com', 25))
        bob_id = cursor.fetchone()[0]

        cursor.execute("""
            INSERT INTO test_users (name, email, age)
            VALUES (%s, %s, %s) RETURNING id
        """, ('Charlie Davis', 'charlie@example.com', 35))
        charlie_id = cursor.fetchone()[0]

        conn.commit()
        print(f"✓ Inserted 3 rows (IDs: {alice_id}, {bob_id}, {charlie_id})")

        # READ
        print("\n[4/7] Reading test data...")
        cursor.execute("SELECT id, name, email, age FROM test_users ORDER BY id")
        rows = cursor.fetchall()
        print(f"✓ Selected {len(rows)} rows:")
        for row in rows:
            print(f"  ID={row[0]}, Name={row[1]}, Email={row[2]}, Age={row[3]}")

        # UPDATE
        print("\n[5/7] Updating test data...")
        cursor.execute("""
            UPDATE test_users
            SET age = %s, email = %s
            WHERE name = %s
        """, (31, 'alice.johnson@example.com', 'Alice Johnson'))
        conn.commit()
        print(f"✓ Updated {cursor.rowcount} row(s)")

        # Verify update
        cursor.execute("SELECT age, email FROM test_users WHERE name = %s", ('Alice Johnson',))
        updated_row = cursor.fetchone()
        print(f"  Verified: Alice's age is now {updated_row[0]}, email is {updated_row[1]}")

        # DELETE
        print("\n[6/7] Deleting test data...")
        cursor.execute("DELETE FROM test_users WHERE name = %s", ('Bob Smith',))
        conn.commit()
        print(f"✓ Deleted {cursor.rowcount} row(s)")

        # Verify remaining rows
        cursor.execute("SELECT COUNT(*) FROM test_users")
        count = cursor.fetchone()[0]
        print(f"  Remaining rows: {count}")

        # Check sessions
        print("\n[7/7] Checking active sessions...")
        cursor.execute("""
            SELECT session_id, protocol, username, state, current_query
            FROM helios_sessions
            ORDER BY session_id
        """)
        sessions = cursor.fetchall()
        print(f"✓ Found {len(sessions)} active session(s):")
        for session in sessions:
            query_preview = session[4][:50] + "..." if session[4] and len(session[4]) > 50 else session[4]
            print(f"  Session {session[0]}: {session[1]} - {session[2]} ({session[3]})")
            if query_preview:
                print(f"    Query: {query_preview}")

        # Test pg_stat_activity view (PostgreSQL compatibility)
        print("\n[BONUS] Testing pg_stat_activity view...")
        cursor.execute("""
            SELECT pid, usename, datname, state, application_name
            FROM pg_stat_activity
            LIMIT 5
        """)
        pg_sessions = cursor.fetchall()
        print(f"✓ pg_stat_activity returned {len(pg_sessions)} row(s)")
        for pg_session in pg_sessions:
            print(f"  PID={pg_session[0]}, User={pg_session[1]}, DB={pg_session[2]}, State={pg_session[3]}")

        # Cleanup
        print("\n[CLEANUP] Dropping test table...")
        cursor.execute("DROP TABLE IF EXISTS test_users")
        conn.commit()
        print("✓ Test table dropped")

        cursor.close()
        conn.close()

        print("\n" + "=" * 60)
        print("✅ PostgreSQL protocol test PASSED")
        print("=" * 60)
        return True

    except psycopg2.Error as e:
        print(f"\n❌ PostgreSQL Error: {e}")
        print(f"Error Code: {e.pgcode}")
        print(f"Error Details: {e.pgerror}")
        return False
    except Exception as e:
        print(f"\n❌ Unexpected Error: {e}")
        import traceback
        traceback.print_exc()
        return False


if __name__ == '__main__':
    success = test_postgresql()
    sys.exit(0 if success else 1)
