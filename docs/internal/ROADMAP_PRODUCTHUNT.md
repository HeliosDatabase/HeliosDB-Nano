# HeliosDB-Nano — ProductHunt Launch Roadmap

**Today:** 2026-06-14 · **Launch:** 2026-06-17 (3 days) · **Branch:** `integrate/v3.58.0`
(28 commits over main `fb8943b`; clean; ISSUE-08 stashed; **nothing pushed/released yet**).
**Last public release:** v3.57.0 (live on crates.io + GitHub Releases, Windows binary incl.).

This roadmap maps the 16 pending items to launch-criticality, and adds the
launch-readiness items that aren't in the engineering queue but are what PH
traffic actually judges on first contact.

---

## STRATEGIC PIVOT (2026-06-15): focus = Nano's own GENERAL DB performance
CodeKB has pinned to 3.36.1 for now, so Nano **stops optimizing for the code-graph
ingest workload** and focuses on its general-purpose DB performance. **This is good
for the launch:** the 3.57 "write-path regression" was a code-graph-ingest-SCALE case
(bulk-loading 142k rows into a 3-secondary-index table) — **NOT general OLTP.** Nano's
GENERAL performance is healthy and unregressed: **pg35 vs PostgreSQL 18.4 = 32–34 of 35
categories won, zero PG wins, no per-category erosion below the historical envelope.**
- The CodeKB-driven items (embed_batch, FastIngest profile, RocksDB knobs, item-1b,
  candidate-c) are all **opt-in / gated / pg35-neutral** → keep them (valid bulk-load
  features), but they're no longer launch-critical.
- **Candidate-(d)** (ART secondary-index deferral under bulk_load_mode) → DE-PRIORITIZED;
  build only if general bulk-load demand appears. Not a launch item.
- **General-perf focus going forward:** maintain the pg35 lead (erosion tracker guards it)
  + the deferred general polish (engine.rs `Arc<Tuple>` cache, integer-filter scan dedup,
  INLJ streaming) for further OLTP/HTAP gains.

## TL;DR recommendation
- **You already have a shippable public release (v3.57.0).** The launch is NOT
  blocked on engineering.
- **The launch perf story is GENERAL: Nano beats PostgreSQL 18.4 on 32–34/35 pg35
  categories** (honest, quiet-host run to confirm the published number). NOT code-graph
  ingest. v3.58's shippable adds are features (COPY text+CSV, MVCC, RocksDB knobs), not a
  perf headline.
- **The real pre-launch risk is NOT code — it's the front door.** Honesty of the
  README/benchmarks/claims and a working 5-minute install are what convert PH
  visitors. Treat the "Launch readiness" section below as P0.
- **Do NOT let the date force an unvalidated perf claim.** Only headline the
  "faster ingest / beats baseline" story if CodeKB stage-2 lands < 6,782.6 s.

---

## P0 — MUST resolve before 2026-06-17

### A. Release-gating — CodeKB stage-2 RESULT (2026-06-15): v3.58 is NOT an ingest perf win
| # | Item | Status | Note |
|---|---|---|---|
| 1 | **CodeKB stage-2, corrected apples-to-apples** | **DONE — NEGATIVE** | Earlier "beats 1.74×" was an artifact of FastIngest **skipping refs**. Same flags (full refs): 3.58 no-embed ref-write **347 s vs 3.36.1 18.6 s = 18.6× slower**; with-embed **489 s vs 241 s = 2×**. Candidate-c fixed the SYMBOL path (1.4 s); a **2nd write-path regression remains on the 142k REF rows.** |

> **⇒ DO NOT claim an ingest perf win for v3.58** — it is a regression vs 3.36.1 on
> ref-heavy ingest. v3.58 still ships real features (COPY text+CSV, FastIngest profile,
> RocksDB knobs, MVCC, erosion tracker) — just not "faster ingest." Launch on v3.57.0,
> or v3.58 *without* the perf headline.

**NEW critical-path item — candidate (d) (the 2nd write-path regression):**
`_hdb_code_symbol_refs` has 3 ART secondary indexes vs symbols' 1, and **ART
secondary-index maintenance is NOT gated by `bulk_load_mode`** (unlike SMFI +
candidate-c) → 142k refs × 3 indexes maintained per row (347 s). **Fix (CodeKB-proposed,
same opt-in shape):** under `bulk_load_mode`, defer ART secondary-index maintenance on the
bulk path + rebuild once at batch end → pg35-neutral. **Caveat:** needs engine
instrumentation to confirm per-row-regression vs 3.36.1-deferred-build. **Non-trivial
(deferral+rebuild) → likely POST-launch** (2 days out).
**ALSO open (separate):** super-linear embed-at-scale (full-corpus 11,144 s vs ~2,150 s
extrapolated, ~5×) — likely embed memory pressure (14.5 GB RSS). Distinct from candidate-d.

### B. v3.58.0 release mechanics (only if (1) green AND you authorize)
| # | Item | Effort |
|---|---|---|
| 2 | Bump Cargo.toml/lock → 3.58.0 | trivial |
| 3 | CHANGELOG entry (embed_batch, FastIngest, RocksDB knobs, item 1b, candidate c, COPY text+CSV, erosion tracker) | small |
| 4 | **Consolidated pg35 A/B on the full branch — on a QUIET host** | medium — all per-run softening this session was host-load (3 agents + builds); a quiet run gives honest magnitudes for any public number |
| 5 | Stash-protect ISSUE-08 → ff-merge → tag v3.58.0 → push → watch workflow | small — **needs your explicit go (irreversible/public)** |

### C. LAUNCH READINESS — not in the 16, but P0 for a PH launch
| # | Item | Why it matters on PH |
|---|---|---|
| L1 | **Install works from crates.io** for the launch version (`cargo install` / binstall / curl-sh / Docker) — smoke-test on a clean box | A broken first `install` kills conversion |
| L2 | **Front-door honesty sweep** (README / landing / llms.txt): every claim matches reality on the launched version. Esp. any ingest-speed / "beats X" / HTAP claim must be backed by the stage-2 number or removed | PH/HN audiences fact-check; an overclaim becomes the top comment |
| L3 | **Benchmarks reproducible + methodology stated** (the 33-0 vs PG18.4 pg35 result, the SQLite comparisons): publish host, versions, command, and the noise caveat | "benchmarks or it didn't happen" |
| L4 | **5-minute quickstart works end-to-end** (install → connect via psql → create/insert/select → one differentiator demo, e.g. branch or code-graph or COPY) | The reviewer's actual test |
| L5 | **PH assets** (tagline, description, demo gif/screens, maker comment) | Not engineering — flag to whoever owns the listing |

---

## P1 — Strongly nice-to-have before the 17th (de-risks the launch story)
| # | Item | Note |
|---|---|---|
| 14 | **Proxy validates COPY text+CSV on the migration mirror** | Validates the "PG-wire COPY" launch bullet; Proxy asked, pending |
| — | **Decide the launch version + the headline** | v3.58 (perf win + COPY) if stage-2 green, else v3.57; pick the demo accordingly |

---

## P2 — POST-LAUNCH (3.59) — explicitly NOT before the 17th
| # | Item | Status |
|---|---|---|
| 6 | Item 6 — pipelined extended exec (N Bind/Execute, one Sync) | held; Proxy investigating its relay |
| 7 | Item 5 — cheap session reset (`DISCARD ALL`/`helios.reset_session()`) | gated on Proxy SCRAM (F.3b) |
| 8 | Item 10 — `ParameterStatus` capability probe (`helios.*` GUCs) | scoped |

## P3 — Later (3.60 / own cycle)
| # | Item | Status |
|---|---|---|
| 9 | Item 7 — binary result-format per portal | scoped |
| 10 | Item 9 — `helios.fast_autocommit` (default off) | scoped |
| 11 | Deferred Nano polish — engine.rs `Arc<Tuple>` cache · integer-filter scan dedup · INLJ streaming | from the v3.57 review |
| 12 | R4.1 — on-disk layout v2 | its own cycle |
| 13 | COPY binary (2g) | deferred by user; scoped (needs PG reference-vector harness + numeric format) |

## P4 — Internal loose ends (anytime)
| # | Item |
|---|---|
| 15 | Erosion tracker — uniform-softening = host-load auto-annotation |
| 16 | Tidy stale `[ ]` checkboxes in the execution log |

---

## Recommended 3-day plan
- **Day 1 (06-14):** wait on CodeKB stage-2; in parallel start **L1–L4 launch-readiness** (these don't depend on v3.58 and are the real risk). Decide launch version + headline.
- **Day 2 (06-15):** if stage-2 green → cut v3.58.0 (items 2–5) on a quiet host with a clean pg35 A/B; finalize L2/L3 numbers from that run. If stage-2 red → lock launch on v3.57.0, drop the perf headline, keep COPY/HTAP/agentic story.
- **Day 3 (06-16):** publish-verify (L1 install smoke test on a clean box), freeze; PH assets (L5) ready.
- **06-17:** launch.

**Bottom line:** the engineering is in good shape — one perf signal gates v3.58, and v3.57 is a safe floor. **Spend the 3 days primarily on launch readiness (front-door honesty + install + reproducible benchmarks), not on the P2/P3 feature queue.**
