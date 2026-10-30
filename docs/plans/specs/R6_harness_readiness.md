# R6 — daemon readiness flake (Sprinter ffe805264aac)

Recurring under full-tier load, never alone: `wait_http(..., "/ready", 200, ...)` panics with
"/ready never answered 200; last None" (no HTTP response at all within the window), and once
(2026-10-30, physical_promote_online::a_dead_primary_is_promoted_past_only_when_allowed)
`/health` returned nothing for 30 s after a refused promotion with no daemon log lines in that
window. Also once a missing NANO-PHYSICAL-REPLICA.json right after a clean stop that followed
/ready 200 (physical_fast_forward::a_standby_of_another_history_needs_a_full_copy_and_replace).

1. Instrument: the harness records, per daemon, the time from spawn to first HTTP answer,
   and on a readiness/health failure captures the daemon's full log, its process state
   (`/proc/<pid>/status`, a stack dump if `gdb` is available — skip if not) and whether the
   port is bound (`ss`-style check through /proc/net/tcp).
2. Fix the causes you can prove: (a) bind the HTTP listener before any long bootstrap so it
   answers 503 early (spec P1.3 says it should); (b) the helper distinguishes
   connection-refused (keep polling) from a hung connection (per-request timeout, keep
   polling) and reports which; (c) port allocation: `free_port` must not hand out a port a
   concurrent test can take before the daemon binds — hold the reservation (bind with
   SO_REUSEADDR and pass, or allocate from a per-process range) and document the choice;
   (d) for the 30 s silent /health: find what can block the HTTP listener after an aborted
   online promotion (`restore_standby`, the placeholder swap, a lock held across await) — the
   statement offload (S1, block_in_place) is now on, so a statement no longer stalls the
   driver; look for a std lock held on an async path.
3. For the marker-after-stop case: determine whether /ready can answer 200 before the
   bootstrap's install rename, or a stop can race the install; fix the ordering if so.
4. Acceptance: the four named tests plus the promotion one pass 20/20 under a parallel
   `cargo test` load (the dispatching session runs it).

Rules: do not run cargo, git or any build. Report root causes found (with evidence) vs
mitigations.
