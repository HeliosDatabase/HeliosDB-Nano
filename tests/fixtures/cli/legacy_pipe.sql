CREATE TABLE t (id INT PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'a'), (2, 'b');
SELECT * FROM t ORDER BY id;
UPDATE t SET v = 'z' WHERE id = 2;
DELETE FROM t WHERE id = 1;
SELECT id, v FROM t;
SELECT count(*) AS n FROM t;
SELECT 1 + 1;
PRAGMA table_info(t);
SELECT * FROM missing_table;
SELEC 1;
-- a comment line
SELECT 7 AS seven; -- trailing comment
\dt
\d t
DROP TABLE t;
