# HeliosDB-Lite HA Hardening Test Proposal

## Objective

Harden High Availability features to ensure:
- **Transparent automated role switching** (coordinated and unexpected)
- **Zero transaction loss** with proper sync mode configuration
- **Transaction resumption** on new primary after failover
- **Read load balancing** with consistency guarantees
- **Proper behavior** across sync/async/semi-sync configurations

## Test Categories Overview

| Category | Tests | Priority | Complexity |
|----------|-------|----------|------------|
| A. Role Switching (Coordinated) | 12 | Critical | Medium |
| B. Role Switching (Unexpected/Failover) | 15 | Critical | High |
| C. Transaction Durability | 18 | Critical | High |
| D. Transaction Resumption (TR) | 12 | High | High |
| E. Read Load Balancing | 10 | High | Medium |
| F. Consistency per Sync Mode | 15 | Critical | High |
| G. Split-Brain Protection | 12 | Critical | High |
| H. Network Partition Scenarios | 10 | High | High |
| I. Proxy Resilience | 14 | High | Medium |
| J. Stress & Chaos Engineering | 10 | Medium | High |
| K. Recovery & Rejoin | 12 | High | Medium |
| L. Edge Cases & Corner Scenarios | 15 | Medium | High |
| **TOTAL** | **145** | | |

---

## Category A: Coordinated Role Switching (Switchover)

### A1. Basic Switchover
```
TEST: Basic primary-to-standby switchover
GIVEN: 3-node cluster (1P + 2S) with all nodes healthy
WHEN: Operator initiates switchover to standby-sync
THEN:
  - Old primary demotes gracefully
  - New primary accepts writes within 5 seconds
  - Other standbys reconnect to new primary
  - Zero transaction loss during switchover
```

### A2. Switchover with Active Transactions
```
TEST: Switchover during active write transactions
GIVEN: 3-node cluster with 10 concurrent write transactions in progress
WHEN: Switchover initiated
THEN:
  - In-flight transactions complete on old primary (drain phase)
  - Or transactions replay on new primary via TR
  - No partial commits visible
  - All transactions report success or explicit failure
```

### A3. Switchover with Large Transaction
```
TEST: Switchover during large bulk insert (10M rows)
GIVEN: Bulk insert transaction in progress (10M rows)
WHEN: Switchover initiated at 50% completion
THEN:
  - Transaction either completes before switchover
  - Or rolls back cleanly
  - Or replays on new primary
  - Data integrity verified post-switchover
```

### A4. Cascading Switchover
```
TEST: Multiple sequential switchovers
GIVEN: 3-node cluster
WHEN: Perform 5 consecutive switchovers (A->B->C->A->B->C)
THEN:
  - Each switchover completes successfully
  - No data loss across any switchover
  - Final state matches initial data
```

### A5. Switchover to Lagging Standby
```
TEST: Switchover when target has replication lag
GIVEN: Standby-sync has 1000 WAL entries lag
WHEN: Switchover to lagging standby
THEN:
  - System waits for catch-up (up to sync_timeout)
  - Switchover proceeds after sync
  - Or fails gracefully if timeout exceeded
```

### A6. Switchover with Semi-Sync Target
```
TEST: Switchover to semi-sync standby
GIVEN: Semi-sync standby (received but not applied WAL)
WHEN: Switchover initiated
THEN:
  - Standby applies pending WAL before promotion
  - No data loss from semi-sync gap
```

### A7. Switchover with Async Target
```
TEST: Switchover to async standby
GIVEN: Async standby with potential data gap
WHEN: Switchover initiated with allow_partial_sync=false
THEN:
  - Switchover rejected (async cannot guarantee zero loss)
  - Error message indicates data loss risk
```

### A8. Switchover with Async Target (Allowed)
```
TEST: Switchover to async standby with explicit allow
GIVEN: Async standby
WHEN: Switchover with allow_partial_sync=true
THEN:
  - Switchover proceeds with warning
  - Data loss amount logged
  - New primary operational
```

### A9. Switchover Rollback
```
TEST: Switchover failure mid-process triggers rollback
GIVEN: 3-node cluster
WHEN: Target standby crashes during switchover
THEN:
  - Old primary re-promotes automatically
  - Or alternate standby promoted
  - Cluster remains operational
```

### A10. Switchover with Read-Heavy Load
```
TEST: Switchover during heavy read traffic (1000 QPS reads)
GIVEN: Heavy read workload distributed across standbys
WHEN: Switchover initiated
THEN:
  - Read queries continue with minimal interruption
  - Read latency spike < 2 seconds
  - No read errors during switchover
```

### A11. Switchover with Mixed Workload
```
TEST: Switchover during mixed read/write load
GIVEN: 500 QPS writes + 2000 QPS reads
WHEN: Switchover initiated
THEN:
  - Write pause < 5 seconds
  - Read continuity maintained
  - All writes complete or replay
```

### A12. Switchover Timeout Handling
```
TEST: Switchover exceeds total_timeout
GIVEN: Standby intentionally slow to sync
WHEN: Switchover initiated with 10s timeout
THEN:
  - Switchover aborts after timeout
  - Old primary remains primary
  - Clear error message returned
```

---

## Category B: Unexpected Role Switching (Failover)

### B1. Primary Crash (SIGKILL)
```
TEST: Primary killed with SIGKILL (immediate termination)
GIVEN: 3-node cluster with active transactions
WHEN: Primary receives SIGKILL
THEN:
  - Failover detected within health_check_interval * failure_threshold
  - Best standby promoted automatically
  - Committed transactions preserved
  - Uncommitted transactions lost (acceptable)
```

### B2. Primary Network Isolation
```
TEST: Primary loses network connectivity
GIVEN: 3-node cluster
WHEN: Primary's network interface disabled
THEN:
  - Standbys detect failure via heartbeat timeout
  - Quorum election proceeds without primary
  - New primary elected
  - Old primary fenced when network restored
```

### B3. Primary Disk Full
```
TEST: Primary runs out of disk space
GIVEN: Primary filesystem at 100%
WHEN: Write attempted
THEN:
  - Write fails with clear error
  - Automatic failover NOT triggered (node healthy, just full)
  - Alert generated for operator
```

### B4. Primary OOM Kill
```
TEST: Primary killed by OOM killer
GIVEN: Primary under memory pressure
WHEN: OOM killer terminates heliosdb-nano
THEN:
  - Same behavior as SIGKILL
  - Failover proceeds automatically
```

### B5. Primary Hang (No Response)
```
TEST: Primary process hangs (100% CPU, no heartbeat)
GIVEN: Primary enters infinite loop (simulated)
WHEN: Heartbeats stop arriving
THEN:
  - Failure detected after threshold
  - Failover proceeds
  - Hung primary fenced via token
```

### B6. Rapid Failover Chain
```
TEST: Multiple rapid failures
GIVEN: 4-node cluster (1P + 3S)
WHEN: Primary fails, then new primary fails within 30s
THEN:
  - Second failover proceeds correctly
  - Cluster stabilizes with third node as primary
  - Data integrity maintained
```

### B7. All Standbys Fail Simultaneously
```
TEST: All standbys crash, primary alone
GIVEN: 1P + 2S cluster
WHEN: Both standbys crash
THEN:
  - Primary continues operating (degraded)
  - Writes succeed (no quorum required for writes in some modes)
  - Warning: no HA protection active
```

### B8. Failover During Switchover
```
TEST: Primary crashes during coordinated switchover
GIVEN: Switchover in progress to standby-sync
WHEN: Old primary crashes in drain phase
THEN:
  - Switchover converts to failover
  - Target standby promoted
  - In-flight transactions handled appropriately
```

### B9. Failover with No Eligible Standby
```
TEST: All standbys too far behind
GIVEN: All standbys have >10000 WAL lag
WHEN: Primary fails
THEN:
  - Least-lagging standby promoted with warning
  - Data loss acknowledged and logged
  - Or cluster enters read-only mode awaiting operator
```

### B10. Primary Restart Race
```
TEST: Primary restarts before failover completes
GIVEN: Primary crashed, failover in progress
WHEN: Old primary restarts quickly
THEN:
  - Old primary joins as standby (fenced)
  - New primary continues as primary
  - No split-brain
```

### B11. Failover with Long-Running Query
```
TEST: Failover during 60-second analytical query
GIVEN: Long SELECT running on primary
WHEN: Primary fails
THEN:
  - Query fails with connection error
  - Client retries on new primary
  - Proxy handles reconnection transparently
```

### B12. Observer-Only Failover
```
TEST: Failover with observer participation
GIVEN: 2 nodes + 2 observers
WHEN: Primary fails
THEN:
  - Observers participate in quorum vote
  - Single standby promoted with observer support
  - Quorum maintained
```

### B13. Failover Candidate Ranking
```
TEST: Best standby selection algorithm
GIVEN: 3 standbys with different states:
  - S1: lag=0, priority=1 (highest)
  - S2: lag=100, priority=2
  - S3: lag=0, priority=3
WHEN: Primary fails
THEN:
  - S1 selected (lowest lag + highest priority)
```

### B14. Failover with Fencing Token Conflict
```
TEST: Old primary tries to write after demotion
GIVEN: Failover completed, old primary has stale token
WHEN: Old primary (now zombie) attempts write
THEN:
  - Write rejected due to invalid fencing token
  - Clear error logged
  - No data corruption
```

### B15. Delayed Failover Detection
```
TEST: Health checks delayed by network latency
GIVEN: Network latency increases from 1ms to 5000ms
WHEN: Primary actually healthy but heartbeats slow
THEN:
  - Failover NOT triggered (heartbeats still arriving)
  - Or configurable sensitivity adjustment
```

---

## Category C: Transaction Durability

### C1. Sync Mode - Zero Loss Guarantee
```
TEST: Transaction committed in sync mode survives failover
GIVEN: Sync mode standby, transaction commits with ACK
WHEN: Primary fails immediately after commit ACK
THEN:
  - Transaction visible on new primary
  - Zero data loss guaranteed
```

### C2. Async Mode - Potential Loss Measurement
```
TEST: Measure actual data loss in async failover
GIVEN: Async standby with known lag
WHEN: Primary fails
THEN:
  - Data loss equals WAL lag at failure time
  - Lost transactions identified and logged
  - RPO matches async configuration
```

### C3. Semi-Sync - Transport Guarantee
```
TEST: Semi-sync transaction received but not applied
GIVEN: Semi-sync standby, WAL received but apply pending
WHEN: Primary fails
THEN:
  - Received WAL applied before promotion
  - Transaction preserved
```

### C4. Commit Acknowledgment Timing
```
TEST: Transaction commit returns only after sync guarantee met
GIVEN: Sync mode with 100ms network latency
WHEN: Transaction commits
THEN:
  - Commit returns after standby ACK (>100ms)
  - Timing verifiable in metrics
```

### C5. Batch Commit Durability
```
TEST: 1000 rapid commits in sync mode
GIVEN: Sync mode, rapid fire 1000 INSERTs
WHEN: Primary fails after 500th commit ACK
THEN:
  - Exactly 500 rows visible on new primary
  - No more, no less
```

### C6. Large Transaction Durability
```
TEST: 1GB transaction in sync mode
GIVEN: Single transaction inserting 1GB of data
WHEN: Committed in sync mode
THEN:
  - Full 1GB replicated before commit ACK
  - Survives immediate failover
```

### C7. DDL Transaction Durability
```
TEST: CREATE TABLE survives failover
GIVEN: CREATE TABLE executed
WHEN: Failover immediately after DDL commit
THEN:
  - Table exists on new primary
  - Schema fully replicated
```

### C8. Multi-Statement Transaction
```
TEST: BEGIN...multiple statements...COMMIT
GIVEN: Transaction with 10 INSERT + 5 UPDATE + 2 DELETE
WHEN: Failover after COMMIT ACK
THEN:
  - All 17 statements visible on new primary
  - Atomic - all or nothing
```

### C9. Savepoint Durability
```
TEST: Partial rollback within transaction
GIVEN: Transaction with SAVEPOINT
WHEN: ROLLBACK TO SAVEPOINT, then COMMIT
THEN:
  - Only committed portion survives failover
  - Rolled-back portion correctly absent
```

### C10. Concurrent Transaction Durability
```
TEST: 100 concurrent transactions
GIVEN: 100 parallel transactions committing
WHEN: Failover at random point
THEN:
  - All committed transactions preserved
  - All uncommitted correctly lost
  - No partial states
```

### C11. Read-Your-Writes in Sync Mode
```
TEST: Immediate read after write sees data
GIVEN: Sync mode
WHEN: INSERT, then immediate SELECT
THEN:
  - SELECT returns inserted row (always)
  - True even if failover between INSERT and SELECT
```

### C12. Write-After-Read Isolation
```
TEST: Transaction isolation during failover
GIVEN: Transaction T1 reads row, T2 updates same row
WHEN: Failover between T1 read and T1 write
THEN:
  - T1's view consistent (snapshot isolation)
  - T2's write properly ordered
```

### C13. Durability Under Load
```
TEST: 10,000 TPS sustained writes
GIVEN: High write throughput (10K TPS)
WHEN: Failover after 60 seconds
THEN:
  - 600,000 transactions processed
  - Sync mode: all committed preserved
  - Metrics show replication lag history
```

### C14. Durability with Connection Pool
```
TEST: Pooled connections transaction durability
GIVEN: 50 pooled connections, rapid transactions
WHEN: Pool connection to primary, failover occurs
THEN:
  - Pool reconnects to new primary
  - In-flight transactions properly handled
```

### C15. Durability with Prepared Statements
```
TEST: Prepared statement execution across failover
GIVEN: Prepared INSERT statement
WHEN: Failover between PREPARE and EXECUTE
THEN:
  - EXECUTE fails (server changed)
  - Re-prepare and execute succeeds
```

### C16. Durability with COPY Command
```
TEST: COPY bulk insert durability
GIVEN: COPY command loading 100,000 rows
WHEN: Failover at 50,000 rows
THEN:
  - Either all 100K committed or none
  - No partial COPY state
```

### C17. Durability with Sequences
```
TEST: Sequence values across failover
GIVEN: SERIAL/SEQUENCE column
WHEN: Multiple inserts, then failover
THEN:
  - Sequence values monotonic on new primary
  - No gaps (or documented gap behavior)
  - No duplicates
```

### C18. Durability with Foreign Keys
```
TEST: Referential integrity across failover
GIVEN: Parent-child tables with FK
WHEN: Insert parent, insert child, failover
THEN:
  - Both records exist
  - FK constraint valid
  - No orphaned children
```

---

## Category D: Transaction Resumption (TR)

### D1. Basic Transaction Replay
```
TEST: Simple transaction replays successfully
GIVEN: TR enabled, transaction: BEGIN; INSERT; UPDATE; COMMIT
WHEN: Connection fails after UPDATE, before COMMIT
THEN:
  - TR replays: INSERT, UPDATE on new connection
  - COMMIT succeeds
  - Data matches original intent
```

### D2. SELECT Replay Verification
```
TEST: SELECT results match during replay
GIVEN: TR with verify_results=true
WHEN: SELECT replayed after failover
THEN:
  - Results compared via checksum
  - Mismatch triggers warning/error
```

### D3. Idempotent Replay
```
TEST: Replay doesn't double-execute
GIVEN: INSERT with explicit ID
WHEN: TR replays INSERT
THEN:
  - Only one row exists (not duplicate)
  - Idempotency preserved
```

### D4. Non-Idempotent Replay Warning
```
TEST: Non-idempotent statement flagged
GIVEN: INSERT with auto-increment ID
WHEN: TR replays
THEN:
  - Warning issued about potential duplicate
  - Or INSERT OR IGNORE semantics used
```

### D5. DDL Replay Handling
```
TEST: CREATE TABLE replay
GIVEN: CREATE TABLE in transaction
WHEN: Replay after failover
THEN:
  - CREATE IF NOT EXISTS semantics
  - Or error if table already replicated
```

### D6. Multi-Statement Replay Order
```
TEST: Statement order preserved
GIVEN: INSERT A; UPDATE A; DELETE A; INSERT A
WHEN: Replay
THEN:
  - Exact order maintained
  - Final state correct
```

### D7. Replay with Parameters
```
TEST: Parameterized statement replay
GIVEN: INSERT with $1, $2 parameters
WHEN: Replay
THEN:
  - Same parameter values used
  - Data matches original
```

### D8. Replay Timeout
```
TEST: Replay fails if too slow
GIVEN: statement_timeout_ms = 5000
WHEN: Replay statement takes 10s
THEN:
  - Replay aborted
  - Clear error to client
```

### D9. Partial Replay Recovery
```
TEST: Replay fails mid-transaction
GIVEN: 5 statements, 3rd fails on replay
WHEN: Replay attempted
THEN:
  - Transaction rolled back
  - Client notified of failure point
  - Option to retry
```

### D10. Replay with WAL Sync Wait
```
TEST: Replay waits for WAL to reach standby
GIVEN: wait_for_wal_sync=true
WHEN: Failover with WAL lag
THEN:
  - Replay waits until target LSN reached
  - Then proceeds with replay
```

### D11. Replay Skip Read-Only
```
TEST: skip_read_only optimization
GIVEN: Transaction with 8 SELECTs + 2 INSERTs
WHEN: Replay with skip_read_only=true
THEN:
  - Only 2 INSERTs replayed
  - Performance optimization verified
```

### D12. Replay Mode: None
```
TEST: TR disabled - clean failure
GIVEN: replay_mode=None
WHEN: Connection fails mid-transaction
THEN:
  - Transaction fails immediately
  - No replay attempted
  - Clear error to client
```

---

## Category E: Read Load Balancing

### E1. Round-Robin Distribution
```
TEST: Reads distributed evenly
GIVEN: 3 standbys, round-robin strategy
WHEN: 300 read queries
THEN:
  - ~100 queries per standby (±10%)
```

### E2. Weighted Distribution
```
TEST: Reads respect weights
GIVEN: S1 weight=1, S2 weight=2, S3 weight=3
WHEN: 600 read queries
THEN:
  - S1: ~100, S2: ~200, S3: ~300
```

### E3. Latency-Based Routing
```
TEST: Prefer low-latency standby
GIVEN: S1 latency=1ms, S2 latency=100ms
WHEN: Read queries
THEN:
  - Majority routed to S1
```

### E4. Unhealthy Node Exclusion
```
TEST: Reads avoid unhealthy standbys
GIVEN: S1 healthy, S2 unhealthy
WHEN: Read queries via proxy
THEN:
  - Zero reads to S2
  - All reads to S1 or primary
```

### E5. Read-After-Write Consistency
```
TEST: Read sees own write (session sticky)
GIVEN: Session writes to primary
WHEN: Immediate read in same session
THEN:
  - Read routed to primary OR
  - Read waits for replication to standby
```

### E6. Stale Read Prevention
```
TEST: Don't read from lagging standby
GIVEN: S1 lag=0, S2 lag=10000 WAL
WHEN: Consistency requirement: strong
THEN:
  - Reads avoid S2
  - Or S2 catches up first
```

### E7. Primary Offload
```
TEST: Reads don't hit primary under normal conditions
GIVEN: All standbys healthy
WHEN: 1000 read queries
THEN:
  - 0 reads to primary (unless configured otherwise)
```

### E8. Primary Fallback
```
TEST: Reads fall back to primary if no standby
GIVEN: All standbys down
WHEN: Read query
THEN:
  - Read succeeds on primary
  - Warning logged about degraded HA
```

### E9. Hot Standby Query Compatibility
```
TEST: Complex queries work on standby
GIVEN: Analytical query with JOINs
WHEN: Routed to standby
THEN:
  - Query executes successfully
  - Results correct
```

### E10. Read Distribution Metrics
```
TEST: Metrics show read distribution
GIVEN: Running cluster with reads
WHEN: Check proxy metrics
THEN:
  - Per-node read counts visible
  - Latency percentiles available
```

---

## Category F: Consistency per Sync Mode

### F1. Sync Mode - Strong Consistency
```
TEST: Sync mode provides linearizability
GIVEN: Sync standby, write W1 commits
WHEN: Read R1 after W1 commit returns
THEN:
  - R1 sees W1 on ANY node (primary or standby)
```

### F2. Async Mode - Eventual Consistency
```
TEST: Async may show stale reads
GIVEN: Async standby
WHEN: Write W1, immediate read on standby
THEN:
  - Read may NOT see W1
  - Read eventually sees W1 (bounded lag)
```

### F3. Semi-Sync Mode - Read-Your-Writes
```
TEST: Semi-sync guarantees in-session consistency
GIVEN: Semi-sync standby
WHEN: Same session: write W1, read R1
THEN:
  - R1 sees W1 (session routed consistently)
```

### F4. Sync Mode Write Latency
```
TEST: Sync write latency >= network RTT
GIVEN: 50ms RTT to standby
WHEN: Sync write
THEN:
  - Write latency >= 50ms
  - Latency metric captured
```

### F5. Async Mode Write Latency
```
TEST: Async write latency independent of standby
GIVEN: Slow standby (1000ms lag)
WHEN: Async write
THEN:
  - Write returns immediately
  - Lag doesn't affect latency
```

### F6. Semi-Sync Degradation
```
TEST: Semi-sync degrades to async if standby slow
GIVEN: Standby takes >30s to ACK
WHEN: Semi-sync write with timeout
THEN:
  - Write commits after timeout
  - Mode temporarily async
  - Warning logged
```

### F7. Mixed Sync Modes in Cluster
```
TEST: Different standbys with different modes
GIVEN: S1=sync, S2=semi-sync, S3=async
WHEN: Write operation
THEN:
  - Commit waits for S1 (sync)
  - S2 may lag slightly
  - S3 may lag significantly
```

### F8. Sync Mode Quorum
```
TEST: Sync mode with quorum (2 of 3)
GIVEN: 3 sync standbys, quorum=2
WHEN: Write operation
THEN:
  - Commit after 2 standby ACKs
  - Third standby can lag
```

### F9. Consistency Level Configuration
```
TEST: Per-query consistency level
GIVEN: SELECT with consistency=strong
WHEN: Read routed to standby
THEN:
  - Standby checks it's up-to-date
  - Or read forwarded to primary
```

### F10. Consistency During Failover
```
TEST: Consistency guaranteed through failover
GIVEN: Sync mode, active reads and writes
WHEN: Failover occurs
THEN:
  - No inconsistent reads (snapshot isolation)
  - Committed writes visible post-failover
```

### F11. Cross-Session Consistency
```
TEST: Session A write visible to Session B
GIVEN: Sync mode
WHEN: Session A commits W1
THEN:
  - Session B read sees W1 immediately
```

### F12. Causal Consistency
```
TEST: Causally related operations ordered
GIVEN: W1 happens-before W2 (causal)
WHEN: Any node reads
THEN:
  - If W2 visible, W1 also visible
```

### F13. Monotonic Reads
```
TEST: Reads don't go backwards
GIVEN: Session reads V1, then V2 of same row
WHEN: Multiple reads in session
THEN:
  - V2 >= V1 (never older version)
```

### F14. Monotonic Writes
```
TEST: Writes apply in order
GIVEN: Session writes W1, then W2
WHEN: Failover between W1 and W2
THEN:
  - W1 always applied before W2
  - Or both lost (never W2 without W1)
```

### F15. Isolation Level Interaction
```
TEST: Transaction isolation + replication consistency
GIVEN: SERIALIZABLE isolation, sync replication
WHEN: Concurrent transactions across nodes
THEN:
  - Full serializability maintained
  - No anomalies possible
```

---

## Category G: Split-Brain Protection

### G1. Quorum Prevents Double Primary
```
TEST: No two primaries with quorum
GIVEN: 5-node cluster, network partitioned 2|3
WHEN: Both partitions try to elect primary
THEN:
  - Only partition with 3 nodes elects primary
  - Partition with 2 nodes enters fenced state
```

### G2. Fencing Token Validation
```
TEST: Stale primary rejected by token
GIVEN: Old primary with token=5, new primary with token=6
WHEN: Old primary attempts write
THEN:
  - Write rejected (token < current)
  - Client receives clear error
```

### G3. Observer Quorum Participation
```
TEST: Observers help reach quorum
GIVEN: 2 data nodes + 2 observers (quorum=3)
WHEN: Primary fails
THEN:
  - Remaining data node + observers = 3
  - Quorum achieved, failover proceeds
```

### G4. Network Heal After Partition
```
TEST: Split cluster heals correctly
GIVEN: Partition healed, two nodes were isolated primaries
WHEN: Network restored
THEN:
  - Only one remains primary (higher token)
  - Other demotes to standby
  - Data reconciled
```

### G5. Asymmetric Partition
```
TEST: A can reach B, B can reach C, A cannot reach C
GIVEN: Partial network partition
WHEN: Health checks run
THEN:
  - Correct transitive failure detection
  - Appropriate action based on reachable quorum
```

### G6. Election Timeout Handling
```
TEST: No quorum within timeout
GIVEN: Network partitioned, no quorum reachable
WHEN: election_timeout (10s) expires
THEN:
  - All nodes enter fenced state
  - No writes accepted
  - Alert raised
```

### G7. Vote Request/Response
```
TEST: Voting protocol correct
GIVEN: Candidate requests votes
WHEN: Nodes receive VoteRequest
THEN:
  - Only vote once per term
  - Higher term gets vote
  - Response includes current term
```

### G8. Term Increment on Election
```
TEST: Term increases on each election
GIVEN: Current term = 5
WHEN: New election triggered
THEN:
  - Candidates use term = 6
  - Winners have term = 6
```

### G9. Stale Vote Rejection
```
TEST: Old term vote request rejected
GIVEN: Current term = 10, vote request for term = 8
WHEN: Node receives request
THEN:
  - Vote denied
  - Response includes term = 10
```

### G10. Heartbeat Maintains Leadership
```
TEST: Regular heartbeats prevent re-election
GIVEN: Primary sending heartbeats
WHEN: Standbys receive heartbeats on time
THEN:
  - No election triggered
  - Term remains stable
```

### G11. Multiple Simultaneous Candidates
```
TEST: Two nodes start election simultaneously
GIVEN: Both nodes detect primary failure at same time
WHEN: Both send VoteRequests
THEN:
  - Tie-breaking (higher last_lsn wins)
  - Or random backoff and retry
  - Eventually one winner
```

### G12. Manual Split-Brain Resolution
```
TEST: Operator resolves split-brain
GIVEN: Split-brain detected (both claim primary)
WHEN: Operator force-promotes one
THEN:
  - Designated node becomes primary
  - Other fenced
  - Data loss documented
```

---

## Category H: Network Partition Scenarios

### H1. Primary Isolated
```
TEST: Primary loses all connectivity
GIVEN: Primary can't reach any standby or observer
WHEN: Timeout expires
THEN:
  - Primary self-fences (no quorum)
  - Standbys elect new primary
```

### H2. Standby Isolated
```
TEST: Single standby loses connectivity
GIVEN: S1 can't reach primary or others
WHEN: S1 detects isolation
THEN:
  - S1 enters disconnected state
  - Cluster continues without S1
  - S1 rejoins when network heals
```

### H3. Proxy Isolated from Primary
```
TEST: Proxy can't reach primary but standbys OK
GIVEN: Proxy -> primary network down
WHEN: Write arrives at proxy
THEN:
  - Write fails (no primary access)
  - Reads continue to standbys
  - Or proxy detects cluster failover
```

### H4. Client Isolated from Proxy
```
TEST: Client loses proxy connectivity
GIVEN: Client -> proxy network down
WHEN: Client attempts query
THEN:
  - Client-side timeout
  - Application handles retry
  - No server-side impact
```

### H5. Datacenter Partition
```
TEST: DC1 (primary + S1) | DC2 (S2 + S3 + observer)
GIVEN: Cross-DC network fails
WHEN: Partition detected
THEN:
  - DC2 has quorum (3 nodes)
  - DC2 elects new primary
  - DC1 fenced
```

### H6. Flapping Network
```
TEST: Network up/down repeatedly
GIVEN: Network flaps every 5 seconds
WHEN: Over 60 seconds
THEN:
  - No excessive failovers (dampening)
  - Cluster stabilizes when network stable
```

### H7. Slow Network (High Latency)
```
TEST: 5 second network latency
GIVEN: All network requests take 5s
WHEN: Health checks and replication
THEN:
  - Appropriate timeout configuration
  - No false failovers
  - Degraded performance noted
```

### H8. Packet Loss
```
TEST: 30% packet loss
GIVEN: Network dropping 30% of packets
WHEN: Replication continues
THEN:
  - TCP retries succeed
  - WAL eventually delivered
  - Performance degraded but functional
```

### H9. MTU Issues
```
TEST: Large WAL entries with small MTU
GIVEN: WAL entry 10KB, MTU 1500
WHEN: WAL replicated
THEN:
  - Proper fragmentation
  - Complete WAL delivered
  - No corruption
```

### H10. DNS Failure
```
TEST: DNS resolution fails
GIVEN: Hostnames used for config
WHEN: DNS unavailable
THEN:
  - Cached connections continue
  - New connections fail with clear error
  - Or IP fallback if configured
```

---

## Category I: Proxy Resilience

### I1. Proxy Restart Recovery
```
TEST: Proxy restarts, connections recover
GIVEN: Active connections through proxy
WHEN: Proxy restarted
THEN:
  - Clients reconnect
  - Backend state preserved
  - Minimal transaction loss
```

### I2. Multiple Proxy Instances
```
TEST: Load balanced proxies
GIVEN: 2 proxy instances
WHEN: Traffic distributed
THEN:
  - Both proxies route correctly
  - Failover between proxies works
```

### I3. Proxy Connection Pool Exhaustion
```
TEST: Pool reaches max connections
GIVEN: Pool max = 100, all in use
WHEN: 101st connection attempted
THEN:
  - Queued or rejected with clear error
  - No crash or corruption
```

### I4. Proxy Health Check Accuracy
```
TEST: Health checks detect unhealthy backends
GIVEN: Backend starts returning errors
WHEN: Health check runs
THEN:
  - Backend marked unhealthy after threshold
  - Traffic stops routing to it
```

### I5. Proxy Graceful Shutdown
```
TEST: Proxy shutdown drains connections
GIVEN: Active transactions
WHEN: SIGTERM sent to proxy
THEN:
  - New connections rejected
  - Active transactions complete
  - Clean shutdown after drain
```

### I6. Proxy Backend Reconnection
```
TEST: Proxy reconnects to restarted backend
GIVEN: Backend restarts
WHEN: Backend becomes available
THEN:
  - Proxy detects via health check
  - New connections use backend
```

### I7. Proxy Write Timeout Activation
```
TEST: Write timeout during failover
GIVEN: Primary fails, failover in progress
WHEN: Write arrives at proxy
THEN:
  - Proxy waits up to write_timeout (30s)
  - If new primary available, routes there
  - If timeout, returns error
```

### I8. Proxy Session Stickiness
```
TEST: Session stays on same backend
GIVEN: Sticky session configuration
WHEN: Multiple queries in session
THEN:
  - All go to same backend
  - Until backend fails
```

### I9. Proxy Query Classification
```
TEST: SELECT vs INSERT correctly classified
GIVEN: Mixed query workload
WHEN: Queries arrive
THEN:
  - SELECTs route to read pool
  - INSERTs route to write pool (primary)
```

### I10. Proxy Transaction Detection
```
TEST: BEGIN...COMMIT boundaries detected
GIVEN: Explicit transaction
WHEN: BEGIN received
THEN:
  - All subsequent queries same backend
  - Until COMMIT/ROLLBACK
```

### I11. Proxy Protocol Forwarding
```
TEST: PostgreSQL wire protocol intact
GIVEN: Complex query with parameters
WHEN: Forwarded through proxy
THEN:
  - Protocol messages unchanged
  - Parameters correctly passed
```

### I12. Proxy Admin API
```
TEST: Admin endpoint shows status
GIVEN: Running proxy
WHEN: GET /nodes called
THEN:
  - All backends listed
  - Health status shown
  - Metrics available
```

### I13. Proxy Metric Accuracy
```
TEST: Proxy metrics match reality
GIVEN: 1000 queries executed
WHEN: Check metrics
THEN:
  - Query count accurate
  - Latency histogram correct
  - Error counts match
```

### I14. Proxy Configuration Reload
```
TEST: Hot reload configuration
GIVEN: Running proxy
WHEN: SIGHUP sent
THEN:
  - New config applied
  - Active connections unaffected
```

---

## Category J: Stress & Chaos Engineering

### J1. Sustained High Load
```
TEST: 10K TPS for 1 hour
GIVEN: Cluster under 10K TPS writes
WHEN: Run for 1 hour
THEN:
  - No memory leaks
  - Latency stable
  - No data loss
```

### J2. Connection Storm
```
TEST: 10,000 simultaneous connections
GIVEN: Connection storm
WHEN: All connect at once
THEN:
  - Graceful degradation
  - No crash
  - Connections served or rejected cleanly
```

### J3. Large Payload Stress
```
TEST: 100MB single row value
GIVEN: BLOB/TEXT with 100MB data
WHEN: Insert and replicate
THEN:
  - Data integrity maintained
  - Replication completes
  - Memory managed
```

### J4. Chaos Monkey - Random Failures
```
TEST: Random node kills over 30 minutes
GIVEN: Random SIGKILL to random node every 5 min
WHEN: Run for 30 minutes
THEN:
  - Cluster recovers each time
  - Data integrity maintained
  - Final state consistent
```

### J5. Chaos Monkey - Network Chaos
```
TEST: Random network partitions
GIVEN: Random network cuts every 2 minutes
WHEN: Run for 20 minutes
THEN:
  - Correct failover/healing each time
  - No split-brain
  - Data consistent
```

### J6. Memory Pressure
```
TEST: Operation under memory limits
GIVEN: Container limited to 512MB
WHEN: Heavy workload
THEN:
  - Graceful memory management
  - OOM kills handled
  - Auto-recovery
```

### J7. Disk I/O Saturation
```
TEST: Disk I/O maxed out
GIVEN: Disk throughput saturated
WHEN: Writes continue
THEN:
  - Backpressure applied
  - No data corruption
  - Recovery when I/O clears
```

### J8. CPU Saturation
```
TEST: 100% CPU utilization
GIVEN: CPU-heavy queries
WHEN: All cores saturated
THEN:
  - Queries queue
  - Health checks still respond
  - No false failovers
```

### J9. Clock Skew
```
TEST: Nodes have different clock times
GIVEN: Node clocks 30s apart
WHEN: Timeouts evaluated
THEN:
  - Monotonic clocks used internally
  - No time-based bugs
```

### J10. Long-Running Transactions
```
TEST: Transaction open for 1 hour
GIVEN: BEGIN, then idle for 1 hour
WHEN: Failover occurs
THEN:
  - Transaction handled appropriately
  - Resources released
  - No blocking others
```

---

## Category K: Recovery & Rejoin

### K1. Standby Rejoin After Crash
```
TEST: Crashed standby rejoins cluster
GIVEN: Standby crashes and restarts
WHEN: Standby comes back online
THEN:
  - Automatically reconnects to primary
  - Catches up from last known LSN
  - Returns to streaming
```

### K2. Standby Rejoin After Long Outage
```
TEST: Standby offline for 24 hours
GIVEN: Standby down for extended period
WHEN: Standby restarts
THEN:
  - Full resync if WAL not available
  - Or catches up from available WAL
  - Eventually consistent
```

### K3. Former Primary Rejoins as Standby
```
TEST: Demoted primary becomes standby
GIVEN: Former primary after failover
WHEN: Starts up
THEN:
  - Joins as standby (not primary)
  - Syncs from new primary
  - No conflicting data
```

### K4. Recovery from Backup
```
TEST: New standby from backup
GIVEN: Fresh node with backup data
WHEN: Joins cluster
THEN:
  - Identifies as standby
  - Catches up from backup LSN
  - Becomes healthy
```

### K5. Partial WAL Recovery
```
TEST: Some WAL missing on standby
GIVEN: Standby missing WAL range
WHEN: Reconnects
THEN:
  - Requests missing WAL
  - Or triggers full resync
  - Data integrity maintained
```

### K6. Recovery After Disk Corruption
```
TEST: Standby with corrupted data file
GIVEN: Data corruption detected
WHEN: Standby restarts
THEN:
  - Corruption detected (CRC check)
  - Full resync initiated
  - Clean data restored
```

### K7. Recovery Prioritization
```
TEST: Multiple nodes recovering
GIVEN: 2 standbys rejoin simultaneously
WHEN: Primary handles recovery
THEN:
  - Both served appropriately
  - No starvation
  - Both eventually healthy
```

### K8. Recovery Bandwidth Limiting
```
TEST: Recovery doesn't starve production
GIVEN: Standby catching up
WHEN: Production traffic ongoing
THEN:
  - Recovery bandwidth limited
  - Production latency stable
```

### K9. Point-in-Time State
```
TEST: Recovery to specific LSN
GIVEN: Target LSN specified
WHEN: Standby syncs
THEN:
  - Stops at exactly that LSN
  - Ready for promotion at that point
```

### K10. Cross-Version Recovery
```
TEST: Standby on newer version
GIVEN: Standby upgraded while offline
WHEN: Rejoins cluster
THEN:
  - Protocol compatibility check
  - Either works or clean error
```

### K11. Incremental Resync
```
TEST: Only sync changed data
GIVEN: Standby has most data
WHEN: Catches up after brief outage
THEN:
  - Only delta transferred
  - Efficient bandwidth use
```

### K12. Recovery Progress Monitoring
```
TEST: Track recovery progress
GIVEN: Standby recovering
WHEN: Check status
THEN:
  - Current LSN visible
  - Target LSN visible
  - ETA calculated
```

---

## Category L: Edge Cases & Corner Scenarios

### L1. Empty Database Failover
```
TEST: Failover with no data
GIVEN: Fresh cluster, no tables
WHEN: Primary fails
THEN:
  - Clean failover
  - Empty database preserved
```

### L2. Single-Node "Cluster"
```
TEST: HA mode with 1 node
GIVEN: Only primary, no standbys
WHEN: Configured for HA
THEN:
  - Warnings about no HA protection
  - Normal operation
  - Clear documentation
```

### L3. Maximum Cluster Size
```
TEST: 10 standby nodes
GIVEN: 1 primary + 10 standbys
WHEN: Normal operation
THEN:
  - All standbys receive WAL
  - Replication lag manageable
  - Failover candidate selection works
```

### L4. Unicode/Binary Data
```
TEST: Special character data replication
GIVEN: UTF-8, emoji, binary data
WHEN: Replicated
THEN:
  - Exact byte preservation
  - No encoding issues
```

### L5. Maximum Row Size
```
TEST: 1GB row replication
GIVEN: Row at maximum size
WHEN: Replicated
THEN:
  - Complete transfer
  - No truncation
```

### L6. Concurrent Schema Changes
```
TEST: DDL during replication
GIVEN: ALTER TABLE during active DML
WHEN: Replicated
THEN:
  - Schema changes ordered correctly
  - DML uses correct schema version
```

### L7. Timezone Handling
```
TEST: Timestamp data across timezones
GIVEN: Primary in UTC, standby in PST
WHEN: Timestamp replicated
THEN:
  - Exact value preserved
  - No timezone conversion
```

### L8. NULL vs Empty String
```
TEST: NULL replication fidelity
GIVEN: NULL values and empty strings
WHEN: Replicated
THEN:
  - NULL remains NULL
  - '' remains ''
  - No confusion
```

### L9. Transaction ID Wraparound
```
TEST: High transaction ID values
GIVEN: Near max transaction ID
WHEN: Wraparound occurs
THEN:
  - Handled correctly
  - No comparison bugs
```

### L10. Exactly-Once Semantics
```
TEST: Message delivery guarantees
GIVEN: Network retries
WHEN: WAL message retransmitted
THEN:
  - Applied exactly once
  - No duplicates
```

### L11. Graceful Degradation
```
TEST: Cluster degrades gracefully
GIVEN: Resources depleting
WHEN: Approaching limits
THEN:
  - Warnings before failures
  - Predictable behavior
```

### L12. Upgrade Rollback
```
TEST: Rollback after failed upgrade
GIVEN: Upgrade failed mid-way
WHEN: Rollback initiated
THEN:
  - Clean rollback
  - No data loss
  - Previous version functional
```

### L13. Backup During Failover
```
TEST: Backup job during failover
GIVEN: Backup running
WHEN: Failover occurs
THEN:
  - Backup fails cleanly or completes
  - No corruption
  - Can retry
```

### L14. Monitoring During Outage
```
TEST: Metrics during partial outage
GIVEN: Primary down, standbys up
WHEN: Check metrics
THEN:
  - Accurate cluster state
  - Alerting triggered
  - Dashboard shows outage
```

### L15. Configuration Drift
```
TEST: Mismatched node configurations
GIVEN: Nodes with different settings
WHEN: Cluster operates
THEN:
  - Warnings about drift
  - Minimum viable config used
  - Documented behavior
```

---

## Implementation Priority

### Phase 1: Critical Foundation (Week 1-2)
- A1-A4 (Basic Switchover)
- B1-B5 (Critical Failover)
- C1-C5 (Core Durability)
- G1-G4 (Split-Brain Basics)

### Phase 2: Transaction Integrity (Week 3-4)
- C6-C18 (Complete Durability)
- D1-D12 (Transaction Replay)
- F1-F15 (Consistency)

### Phase 3: Resilience (Week 5-6)
- A5-A12 (Advanced Switchover)
- B6-B15 (Edge Failover)
- H1-H10 (Network Partitions)
- I1-I14 (Proxy Resilience)

### Phase 4: Hardening (Week 7-8)
- E1-E10 (Load Balancing)
- G5-G12 (Advanced Split-Brain)
- J1-J10 (Stress Tests)
- K1-K12 (Recovery)
- L1-L15 (Edge Cases)

---

## Test Infrastructure Requirements

### Docker Resources
- 6+ containers per test scenario
- Network manipulation capabilities (tc, iptables)
- Resource limits (cgroups)
- Volume management for data persistence

### Monitoring Stack
- Prometheus for metrics
- Grafana for dashboards
- Log aggregation (Loki/ELK)
- Alerting integration

### Automation
- CI/CD pipeline integration
- Parallel test execution
- Automatic failure categorization
- Performance regression detection

### Reporting
- Test result aggregation
- Failure analysis
- Performance trending
- Coverage metrics

---

## Success Criteria

Each test should verify:
1. **Functional correctness** - Expected behavior achieved
2. **Data integrity** - No corruption or loss beyond documented limits
3. **Performance bounds** - Operations complete within SLA
4. **Error handling** - Clear errors for failure cases
5. **Recovery capability** - System returns to healthy state
6. **Observability** - Appropriate logs and metrics generated

## Approval Request

Please review this test proposal and:
1. Approve tests to implement
2. Prioritize specific categories
3. Identify any missing scenarios
4. Specify any tests to skip or defer

Once approved, implementation will begin with Phase 1 tests.
