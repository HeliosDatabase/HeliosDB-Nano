#!/usr/bin/env python3
"""Per-category erosion tracker. Compares a new pg35 run against the historical
envelope (min/median/max ratio per category). Flags a category only when Nano
performs WORSE than its worst historical run (noise-robust), or a solid-win
category degrades toward a tie."""
import re,sys,os,json,statistics
HIST="/home/gpc/HDB/Nano/perf/v358_program/pg35_category_history.json"
ROW=re.compile(r'^(.+?)\s*\|\s*([\d.]+(?:us|ms|s)|N/A)\s*\|\s*([\d.]+(?:us|ms|s)|N/A)\s*\|\s*([\d.]+x|--|N/A)\s*\|\s*(\S+)\s*$')
def to_us(v):
    m=re.match(r'([\d.]+)(us|ms|s)$',v or ""); return float(m.group(1))*{"us":1,"ms":1000,"s":1_000_000}[m.group(2)] if m else None
def w(r): return "Nano" if r<=0.95 else ("PG" if r>=1.05 else "~tie")
def parse(p):
    acc={}
    for ln in open(p,errors="ignore"):
        m=ROW.match(ln.rstrip())
        if not m or m.group(1).strip().lower()=="category": continue
        n,pp=to_us(m.group(2)),to_us(m.group(3))
        if n and pp: acc.setdefault(m.group(1).strip(),[]).append(n/pp)
    return {k:round(statistics.median(v),4) for k,v in acc.items()}
log,label=sys.argv[1],sys.argv[2]
cur=parse(log)
H=json.load(open(HIST)); env=H.get("envelope",{}); H.setdefault("snapshots",[]).append({"label":label,"ratios":cur})
json.dump(H,open(HIST,"w"),indent=1)
print(f"=== pg35 erosion tracker: {label} ({len(cur)} categories) vs historical envelope ===")
eroded=[]
for c,r in cur.items():
    e=env.get(c)
    if not e: continue
    rmax=e["ratio_max"]
    # genuine erosion: worse than the worst historical run by a real margin
    guard=rmax+max(0.03,0.08*max(rmax,0.05))
    if r>guard:
        eroded.append((c,r,e))
    # solid historical Nano win degrading toward tie/PG
    elif e["ratio_median"]<=0.85 and r>=0.95:
        eroded.append((c,r,e))
if eroded:
    print(f"  ⚠ {len(eroded)} category(ies) ERODED below historical envelope:")
    for c,r,e in sorted(eroded,key=lambda x:-x[1]):
        print(f"    {c:<22} now={r:.3f}[{w(r)}]  hist min/med/max={e['ratio_min']}/{e['ratio_median']}/{e['ratio_max']}")
else:
    print("  ✓ every category within its historical envelope — no erosion of Nano's advantage")
WATCH=["INNER JOIN","LEFT JOIN","4-table JOIN","ORDER+LIMIT","Prepared stmts"]
print("  watch categories (closest to PG):")
for c in WATCH:
    if c in cur:
        e=env.get(c,{}); print(f"    {c:<22} now={cur[c]:.3f}[{w(cur[c])}]  hist med={e.get('ratio_median','?')} max={e.get('ratio_max','?')}")
