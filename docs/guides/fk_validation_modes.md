# Foreign Key Validation Modes

HeliosDB-Nano enforces foreign keys by default. Bulk loaders, trusted
ingest pipelines, and proxy-routed deployments can choose a session-level
validation mode:

```sql
SET helios.fk_validation = 'enforced'; -- default
SET helios.fk_validation = 'deferred'; -- validate at COMMIT
SET helios.fk_validation = 'audit';    -- accept writes, log violations
SET helios.fk_validation = 'off';      -- skip checks for this session
RESET helios.fk_validation;

-- MySQL-compatible switch:
SET foreign_key_checks = 0;
SET foreign_key_checks = 1;
```

`deferred` queues FK checks during a transaction and validates them at
`COMMIT`, preserving read-your-own-writes for parent rows inserted later in
the same transaction. `audit` writes violations to `pg_log_violations` without
rejecting the DML:

```sql
SET helios.fk_validation = 'audit';
INSERT INTO child (id, parent_id) VALUES (1, 404);
SELECT * FROM pg_log_violations;
```

Per-constraint opt-out is available when the schema records relationships that
are enforced upstream or by a trusted loader:

```sql
CREATE TABLE child (
  id INTEGER PRIMARY KEY,
  parent_id INTEGER,
  CONSTRAINT child_parent_fk
    FOREIGN KEY (parent_id) REFERENCES parent(id) NOT ENFORCED
);

ALTER TABLE child ALTER CONSTRAINT child_parent_fk ENFORCED;
ALTER TABLE child ALTER CONSTRAINT child_parent_fk NOT ENFORCED;
```

Proxy deployments can advertise where FK validation is expected to happen:

```sql
SET helios.fk_validation_source = 'engine'; -- default
SET helios.fk_validation_source = 'proxy';  -- trust proxy-side validation
SET helios.fk_validation_source = 'both';   -- engine still validates
```

Use `off` only for trusted bulk loads or restore/bootstrap phases. Use `audit`
when violations should become operational telemetry instead of write failures.
