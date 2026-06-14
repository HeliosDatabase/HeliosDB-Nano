#!/usr/bin/env python3
"""Build the historical envelope (min/median/max Nano/PG ratio per category)
from curated past runs, to anchor noise-robust erosion detection."""
import re, glob, json, statistics, os
ROW=re.compile(r'^(.+?)\s*\|\s*([\d.]+(?:us|ms|s)|N/A)\s*\|\s*([\d.]+(?:us|ms|s)|N/A)\s*\|\s*([\d.]+x|--|N/A)\s*\|\s*(\S+)\s*$')
def to_us(v):
    m=re.match(r'([\d.]+)(us|ms|s)$',v or ""); return float(m.group(1))*{"us":1,"ms":1000,"s":1_000_000}[m.group(2)] if m else None
CUR=[]
for g in ["/home/gpc/HDB/Nano/perf/v357_vs_postgresql/*accepted*.log",
          "/home/gpc/OLD/Nano-r01/perf/v357_vs_postgresql/pg35_full_iters20_fixed*.log",
          "/home/gpc/OLD/Nano-r01/perf/v351_vs_postgresql/pg35_18_4_final_joinfix_r*.log",
          "/home/gpc/OLD/Nano-r01/perf/v351_vs_postgresql/pg35_18_4_exact_source_r*.log",
          "/home/gpc/OLD/Nano-r01/perf/v351_vs_postgresql/pg35_18_4_opusfix_r*.log"]:
    CUR+=glob.glob(g)
def parse(p):
    o={}
    for ln in open(p,errors="ignore"):
        m=ROW.match(ln.rstrip())
        if not m or m.group(1).strip().lower()=="category": continue
        n,pp=to_us(m.group(2)),to_us(m.group(3))
        if n and pp: o[m.group(1).strip()]={"ratio":n/pp,"nano_us":n,"pg_us":pp}
    return o
runs=[parse(f) for f in CUR]; runs=[r for r in runs if r]
cats=set().union(*[set(r) for r in runs])
env={}
for c in cats:
    rs=[r[c]["ratio"] for r in runs if c in r]
    ns=[r[c]["nano_us"] for r in runs if c in r]; ps=[r[c]["pg_us"] for r in runs if c in r]
    if not rs: continue
    env[c]={"ratio_min":round(min(rs),4),"ratio_median":round(statistics.median(rs),4),
            "ratio_max":round(max(rs),4),"nano_us_median":round(statistics.median(ns),2),
            "pg_us_median":round(statistics.median(ps),2),"n":len(rs)}
HIST="/home/gpc/HDB/Nano/perf/v358_program/pg35_category_history.json"
json.dump({"envelope":env,"snapshots":[]},open(HIST,"w"),indent=1)
print(f"historical envelope from {len(runs)} curated runs -> {len(env)} categories seeded.")
print(f"{'category':<22}{'min':>8}{'median':>8}{'max':>8}  guard(>max means eroded below history)")
for c in sorted(env,key=lambda x:env[x]["ratio_median"]):
    e=env[c]; print(f"{c:<22}{e['ratio_min']:>8.3f}{e['ratio_median']:>8.3f}{e['ratio_max']:>8.3f}")
