#!/usr/bin/env python3
"""
Oracle Protocol Test Suite for HeliosDB Nano

Tests Oracle wire protocol implementation with CRUD operations,
session tracking, and Oracle-specific SQL features.
"""

import oracledb
import sys
from datetime import datetime


def test_oracle():
    """Test Oracle protocol with CRUD operations"""

    print("=" * 60)
    print("Oracle Protocol Test Suite")
    print("=" * 60)

    try:
        # Connect
        print("\n[1/8] Connecting to HeliosDB via Oracle protocol...")
        conn = oracledb.connect(
            user='test_user',
            password='test_pass',
            dsn='localhost:1521/heliosdb'
        )
        cursor = conn.cursor()
        print("✓ Connection established")

        # DROP existing table if exists
        print("\n[2/8] Preparing test environment...")
        try:
            cursor.execute("DROP TABLE test_products")
            conn.commit()
            print("✓ Dropped existing test_products table")
        except oracledb.DatabaseError:
            print("✓ No existing test_products table to drop")

        # CREATE
        print("\n[3/8] Creating test table...")
        cursor.execute("""
            CREATE TABLE test_products (
                id NUMBER PRIMARY KEY,
                name VARCHAR2(100) NOT NULL,
                price NUMBER(10,2) NOT NULL,
                in_stock NUMBER(1) DEFAULT 1,
                category VARCHAR2(50),
                created_date DATE DEFAULT SYSDATE
            )
        """)
        conn.commit()
        print("✓ Table 'test_products' created")

        # INSERT (Oracle style)
        print("\n[4/8] Inserting test data...")
        cursor.execute("""
            INSERT INTO test_products (id, name, price, in_stock, category)
            VALUES (1, 'Widget Pro', 19.99, 1, 'Hardware')
        """)

        cursor.execute("""
            INSERT INTO test_products (id, name, price, in_stock, category)
            VALUES (2, 'Gadget Plus', 29.99, 0, 'Electronics')
        """)

        cursor.execute("""
            INSERT INTO test_products (id, name, price, in_stock, category)
            VALUES (3, 'Tool Master', 49.99, 1, 'Hardware')
        """)

        conn.commit()
        print(f"✓ Inserted 3 rows")

        # READ (Oracle style with DUAL)
        print("\n[5/8] Testing Oracle-specific queries...")
        cursor.execute("SELECT SYSDATE FROM DUAL")
        date_row = cursor.fetchone()
        print(f"✓ Oracle SYSDATE: {date_row[0]}")

        cursor.execute("SELECT USER FROM DUAL")
        user_row = cursor.fetchone()
        print(f"✓ Current USER: {user_row[0]}")

        # SELECT with Oracle functions
        print("\n[6/8] Reading test data with Oracle functions...")
        cursor.execute("""
            SELECT
                id,
                UPPER(name) AS name_upper,
                TO_CHAR(price, '999.99') AS price_formatted,
                DECODE(in_stock, 1, 'Available', 'Out of Stock') AS stock_status,
                NVL(category, 'Uncategorized') AS category
            FROM test_products
            ORDER BY id
        """)
        rows = cursor.fetchall()
        print(f"✓ Selected {len(rows)} rows:")
        for row in rows:
            print(f"  ID={row[0]}, Name={row[1]}, Price={row[2]}, Status={row[3]}, Category={row[4]}")

        # UPDATE (Oracle style with NVL)
        print("\n[7/8] Updating test data...")
        cursor.execute("""
            UPDATE test_products
            SET price = NVL(price, 0) + 5.00
            WHERE id = 1
        """)
        conn.commit()
        print(f"✓ Updated {cursor.rowcount} row(s)")

        # Verify update
        cursor.execute("SELECT name, price FROM test_products WHERE id = 1")
        updated_row = cursor.fetchone()
        print(f"  Verified: {updated_row[0]} price is now {updated_row[1]}")

        # DELETE with Oracle WHERE clause
        print("\n[8/8] Deleting test data...")
        cursor.execute("DELETE FROM test_products WHERE in_stock = 0")
        conn.commit()
        print(f"✓ Deleted {cursor.rowcount} row(s) (out of stock items)")

        # Verify remaining rows
        cursor.execute("SELECT COUNT(*) FROM test_products")
        count = cursor.fetchone()[0]
        print(f"  Remaining products: {count}")

        # Check sessions (Oracle style)
        print("\n[BONUS] Checking active sessions via v$session...")
        cursor.execute("""
            SELECT sid, username, status, machine, program
            FROM v$session
            WHERE username IS NOT NULL
            ORDER BY sid
        """)
        sessions = cursor.fetchall()
        print(f"✓ Found {len(sessions)} active session(s):")
        for session in sessions:
            print(f"  SID={session[0]}, User={session[1]}, Status={session[2]}, Machine={session[3]}")

        # Test more Oracle-specific features
        print("\n[ADVANCED] Testing advanced Oracle features...")

        # Test ROWNUM
        cursor.execute("""
            SELECT id, name, price
            FROM test_products
            WHERE ROWNUM <= 2
            ORDER BY price DESC
        """)
        limited_rows = cursor.fetchall()
        print(f"✓ ROWNUM query returned {len(limited_rows)} row(s)")

        # Test aggregate with HAVING
        cursor.execute("""
            SELECT category, COUNT(*) AS product_count, AVG(price) AS avg_price
            FROM test_products
            GROUP BY category
            HAVING COUNT(*) > 0
            ORDER BY avg_price DESC
        """)
        agg_rows = cursor.fetchall()
        print(f"✓ Aggregation query returned {len(agg_rows)} category(s):")
        for agg_row in agg_rows:
            print(f"  Category={agg_row[0]}, Count={agg_row[1]}, Avg Price={agg_row[2]:.2f}")

        # Cleanup
        print("\n[CLEANUP] Dropping test table...")
        cursor.execute("DROP TABLE test_products")
        conn.commit()
        print("✓ Test table dropped")

        cursor.close()
        conn.close()

        print("\n" + "=" * 60)
        print("✅ Oracle protocol test PASSED")
        print("=" * 60)
        return True

    except oracledb.DatabaseError as e:
        error_obj, = e.args
        print(f"\n❌ Oracle Error: {error_obj.message}")
        print(f"Error Code: {error_obj.code}")
        return False
    except Exception as e:
        print(f"\n❌ Unexpected Error: {e}")
        import traceback
        traceback.print_exc()
        return False


if __name__ == '__main__':
    success = test_oracle()
    sys.exit(0 if success else 1)
