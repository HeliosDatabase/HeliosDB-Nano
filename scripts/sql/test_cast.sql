-- Test CAST with vectors
CREATE TABLE IF NOT EXISTS test_docs (
    id INT4 PRIMARY KEY,
    title TEXT,
    embedding VECTOR(3)
);

-- Test inserting with CAST (this is what user needs)
INSERT INTO test_docs VALUES (1, 'Test', '[0.9, 0.1, 0.0]'::VECTOR(3));
INSERT INTO test_docs VALUES (2, 'AI', '[0.8, 0.2, 0.0]'::VECTOR(3));

-- Test querying with CAST
SELECT * FROM test_docs WHERE embedding <-> '[1.0, 0.0, 0.0]'::VECTOR(3) < 0.5;

-- Test other CAST types
SELECT 
    '123'::INT4 as int_cast,
    '45.67'::FLOAT8 as float_cast,
    123::TEXT as text_cast;
