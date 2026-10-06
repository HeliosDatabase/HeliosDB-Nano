# Owned CLI physical resync regression — NON-SERVING only

External, unrun draft against frozen `/home/gpc/HDB/worktrees/nano-resync-20260929` CLI/config/cursor APIs. No repository input was edited. Python AST parsing and Rustfmt parsing pass; neither source is compiled/executed by this author.

`run_cli_resync_regression.py` requires prebuilt `--binary` and `--inspector` executables and an evidence `--output-dir`. Parent must compile the exact reviewed `inspect_physical_fixture.rs` as an isolated example in a later authorized locked gate; the script never builds. It imports the existing reviewed `probe_standby_wire_writes.py` fixture helper read-only and records its hash along with both executables and the driver before/after. The raw v1 cursor layout is independently decoded by the inspector; future cursor changes require updating/reviewing this fixture helper rather than silently accepting them.

Run under the campaign's shared build/host lock and no-swap scope only:

```text
python3 cli-resync-regression/run_cli_resync_regression.py \
  --binary /absolute/path/heliosdb-nano \
  --inspector /absolute/path/inspect_physical_fixture \
  --output-dir /owned/evidence
```

The driver owns one unique retained fixture tree, private token/export/data/cwd directories, ephemeral ports and every child PID. It verifies source PG/native/physical listener ownership through `/proc` before connecting, uses actual PostgreSQL password startup and SQL command results, never kills by name, and never reuses a pre-existing user database. SIGINT/SIGTERM around spawn are deferred until child ownership is registered; cleanup shields signals and attempts all exact resources even if one close fails. Success requires orderly exit0 for the followed receiver and primary; forced cleanup can never count as the orderly proof step.

Covered cases:

1. Default-off source is explicitly enabled on a persistent primary, with a private32-byte bearer file, loopback endpoint, separate owned export directory and bounded transfer settings.
2. Wrong token and wrong expected history return nonzero before adopting any receiver path; the entire destination-parent file census stays unchanged. A pre-existing destination containing a sentinel is refused and preserved byte-for-byte.
3. Fresh absent destination receives Snapshot then Follow. Source commits initial rows including a never-modified snapshot-only row; after installation it commits UPDATE/INSERT. Progress messages schedule waiting and shutdown only. The receiver owns no listening TCP sockets and never prints SQL server readiness.
4. After BOTH processes exit cleanly, raw read-only inspection compares exact owned `data:resync_cli_probe:` key/value records and their framed SHA256. Receiver cursor format/history/monotonic bounds/batch identity are validated independently, and local_sequence must equal actual RocksDB latest_sequence. Marker/manifest cut/history/IDENTITY/hash must agree.
5. Restart the primary with the same data directory, verify the same dataset UUID and committed SQL rows, reject wrong-history Resume without modifying the closed receiver, then restart the receiver using explicit Resume. New committed DELETE/INSERT must appear in closed raw comparison. The durable source cursor advances while original snapshot UUID/cut stay unchanged.
6. Ordinary SQL startup on the final physical receiver must fail with the physical-marker diagnostic, no ready banner and no database file changes.

The inspector uses only `DB::open_for_read_only`, not StorageEngine, EmbeddedDatabase, or writable StagedPhysicalReplica. RocksDB diagnostic logs are redirected to an existing private directory disjoint from the inspected database. Before/after recursive file-content/permission census verifies inspection did not modify database bytes. Source inspection occurs only after orderly primary shutdown. All logs, JSONL wire frames, commands, inventories, raw records and final results are retained.

Interpretation limits: raw catch-up progress is not a coherent Nano SQL commit/DDL fence. The source SQL COMMIT result and source rows establish the fixture's intended committed data, but the raw comparison only proves exact transfer of the owned data prefix. Source shutdown may legitimately append metadata writes after receiver stop, so the driver requires receiver source_sequence <= closed-source latest_sequence; it does not falsely require equality or certify all namespaces. No SQL query is ever executed on the physical receiver. Strict hydration, serving-generation publication, complete feature fidelity, certified source-operation fences and usable online standby recovery remain separate unfinished requirements.

Exit0 requires the complete matrix, unchanged executable/helper/driver hashes and successful cleanup. Exit1 records a concrete assertion failure; exit2 records setup, interruption, cleanup, artifact identity or bounded-progress uncertainty. No pass can be inferred from the source review alone.

Inventory hardening after independent review: census requires a real nonsymlink root and raises directory traversal errors; source/receiver databases must contain regular files. Only the destination-parent control permits an empty directory. Root permissions are included in before/after comparison. Six pure mocked checks cover missing/symlink/unreadable/empty database refusal, explicit empty-parent acceptance and successful file hashing. No database or process was opened for those checks. Updated driver SHA256: a3d92ab3aab9d87a032f9aac6d4b8e4f58c5c486df895c4fb8e45c69bf52d3c6.
