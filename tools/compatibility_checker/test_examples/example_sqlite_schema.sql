-- Example SQLite schema with compatibility issues
-- This file demonstrates various SQLite features that need conversion

-- Issue: AUTOINCREMENT - CRITICAL
-- Should use: SERIAL PRIMARY KEY
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    email TEXT UNIQUE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Issue: IF NOT EXISTS - WARNING
CREATE TABLE IF NOT EXISTS posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER REFERENCES users(id),
    title TEXT NOT NULL,
    content TEXT,
    published_at TIMESTAMP
);

-- Issue: WITHOUT ROWID - WARNING
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT
) WITHOUT ROWID;

-- Issue: Type affinity - INFO
-- SQLite allows dynamic typing, HeliosDB requires explicit types
CREATE TABLE flexible_data (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    value  -- No type specified, relies on type affinity
);

-- Issue: BLOB type - WARNING (not yet supported)
CREATE TABLE files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    filename TEXT,
    content BLOB,
    mime_type TEXT
);

-- Issue: REAL type - INFO
-- Should use FLOAT4 or FLOAT8
CREATE TABLE measurements (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    temperature REAL,
    humidity REAL,
    recorded_at TIMESTAMP
);

-- Issue: ON CONFLICT REPLACE - WARNING
-- Different semantics in HeliosDB
CREATE TABLE cache (
    key TEXT PRIMARY KEY ON CONFLICT REPLACE,
    value TEXT,
    expires_at TIMESTAMP
);

-- Trigger usage - WARNING (Phase 3 feature)
CREATE TRIGGER update_user_timestamp
AFTER UPDATE ON users
FOR EACH ROW
BEGIN
    UPDATE users SET updated_at = CURRENT_TIMESTAMP WHERE id = NEW.id;
END;

-- View usage - INFO (Phase 2 feature)
CREATE VIEW active_users AS
SELECT * FROM users WHERE status = 'active';

-- Index creation (mostly compatible)
CREATE INDEX idx_users_email ON users(email);
CREATE UNIQUE INDEX idx_posts_slug ON posts(slug);

-- Composite primary key (compatible)
CREATE TABLE user_roles (
    user_id INTEGER REFERENCES users(id),
    role_id INTEGER REFERENCES roles(id),
    PRIMARY KEY (user_id, role_id)
);

-- Foreign key constraints (compatible, but check configuration)
CREATE TABLE comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    post_id INTEGER REFERENCES posts(id) ON DELETE CASCADE,
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    content TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Check constraints (mostly compatible)
CREATE TABLE products (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    price REAL CHECK(price > 0),
    quantity INTEGER CHECK(quantity >= 0),
    status TEXT CHECK(status IN ('active', 'discontinued', 'out_of_stock'))
);

-- ATTACH DATABASE statement - CRITICAL
-- Not supported in HeliosDB
ATTACH DATABASE 'analytics.db' AS analytics;

-- Cross-database query - CRITICAL (requires ATTACH)
-- SELECT * FROM analytics.events WHERE user_id = ?;

-- PRAGMA statements - WARNING
-- Should use HeliosDB configuration
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA cache_size = 10000;

-- Partial index (compatible, but verify syntax)
CREATE INDEX idx_active_users ON users(email) WHERE status = 'active';

-- Expression index (compatible)
CREATE INDEX idx_users_lower_email ON users(LOWER(email));

-- Virtual table (FTS5) - WARNING (Phase 2)
CREATE VIRTUAL TABLE posts_fts USING fts5(title, content);

-- JSON1 extension usage - WARNING (Phase 2)
-- CREATE TABLE json_data (
--     id INTEGER PRIMARY KEY AUTOINCREMENT,
--     data JSON
-- );
-- SELECT json_extract(data, '$.name') FROM json_data;

-- Recommended HeliosDB-compatible version:
-- CREATE TABLE users_heliosdb (
--     id SERIAL PRIMARY KEY,
--     name TEXT NOT NULL,
--     email VARCHAR(255) UNIQUE,
--     created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
-- );

-- CREATE TABLE files_heliosdb (
--     id SERIAL PRIMARY KEY,
--     filename TEXT,
--     content BYTEA,  -- Instead of BLOB (when supported)
--     mime_type TEXT
-- );

-- CREATE TABLE measurements_heliosdb (
--     id SERIAL PRIMARY KEY,
--     temperature FLOAT8,
--     humidity FLOAT8,
--     recorded_at TIMESTAMP
-- );
