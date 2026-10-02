# Fixed committed boundary and checkpoint split (external draft)

Patch SHA256: `5b5f577c53c636e57d679e55b348f01b8d2b2e0526951908c1e87ab5648610c6`.
Full physical source: `287b2aeaaa9fbabf4ffedc0a2861f71fe2330c7347ca4616d38be173d027cdd3`.
Full checkpoint source: `f0d3c859dfd2ad6a029f16258ad4725f8956b36af5b03034b0cae1f09cd87917`.
Base files are recorded in source-base.sha256 from the frozen nano-resync worktree. No worktree was edited or test/build executed. Rustfmt parsed both files using repository configuration.

Composition requires `replication::physical_source_coordinator` from the separate reviewed coordinator draft. `poll_source_through(db, history, after, &CommittedPhysicalBarrier, byte_limit)` checks the capability's exact Arc-backed RocksDB handle and history. Its private numeric helper never widens the cap: newer latest sequence is only an availability check; flush may make newer bytes durable, but returned batches and advertised durable_barrier stop at the certified cap. Whole batches cannot cross the cap. Byte limits may return an incomplete prefix; gaps fail closed. Raw poll_source remains available and explicitly does not certify SQL consistency.

`OnlineCheckpointExport::capture` creates an owned checkpoint and obtains its exact cut, with bounded identity/path checks. `CapturedCheckpoint::{history,checkpoint_sequence,storage_identity}` expose metadata only. The coordinator captures under its fence and checks exact cut equality, then returns. `CapturedCheckpoint::finish` performs file census and whole-file hashing after admission is released, revalidating source history and captured ownership. Dropping an unfinished capture removes only its own TempDir. Raw create remains compatibility capture+finish without claiming certification.

Six additional unrun tests cover fixed-cap exclusion of later writes; byte-limited prefixes/interior/future caps; WAL gaps before and after cap; real opaque-cap source/history binding; capture followed by later writes before hashing and physical continuation; abandoned lease ownership and changed-history refusal.

This delta is not all-mutator wiring, committed-boundary wire negotiation, SQL hydration, serving publication, WAL retention, or remote transport acceptance. The certificate is only meaningful after every source mutator uses the one coordinator, including metadata and error cleanup. Source operations may continue during hashing. Waiting timeouts cannot forcibly bound synchronous RocksDB/checkpoint/fsync IO. The existing file-count/byte limits bound transfer/hash census; checkpoint creation itself can still copy large source state before census.
