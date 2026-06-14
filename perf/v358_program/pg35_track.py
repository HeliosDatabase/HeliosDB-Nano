#!/usr/bin/env python3
"""Per-category pg35 erosion tracker (two tiers + trend).

  ERODED  (hard, blocks the item): Nano performed WORSE than its worst historical
          run for that category, or a solid Nano-win degraded toward a tie.
  SOFTENING (note, informational): Nano still winning but its speedup dropped
          >3% vs the historical median — e.g. 70x -> 68x. Surfaces gradual loss
          of a big lead before it ever reaches the envelope edge.
  TREND   : per-category ratio trajectory across program snapshots; monotonic
          worsening over >=3 snapshots is flagged even if each step is small.
"""
import re,sys,os,json,statistics
HIST="/home/gpc/HDB/Nano/perf/v358_program/pg35_category_history.json"
ROW=re.compile(r'^(.+?)\s*\|\s*([\d.]+(?:us|ms|s)|N/A)\s*\|\s*([\d.]+(?:us|ms|s)|N/A)\s*\|\s*([\d.]+x|--|N/A)\s*\|\s*(\S+)\s*$')
def to_us(v):
    m=re.match(r'([\d.]+)(us|ms|s)$',v or ""); return float(m.group(1))*{"us":1,"ms":1000,"s":1_000_000}[m.group(2)] if m else None
def w(r): return "Nano" if r<=0.95 else ("PG" if r>=1.05 else "~tie")
def sp(r): return f"{1/r:.1f}x" if r and r>0 else "-"
def parse(p):
    acc={}
    for ln in open(p,errors="ignore"):
        m=ROW.match(ln.rstrip())
        if not m or m.group(1).strip().lower()=="category": continue
        n,pp=to_us(m.group(2)),to_us(m.group(3))
        if n and pp: acc.setdefault(m.group(1).strip(),[]).append(n/pp)
    return {k:round(statistics.median(v),5) for k,v in acc.items()}

log,label=sys.argv[1],sys.argv[2]
cur=parse(log)
H=json.load(open(HIST)); env=H.get("envelope",{}); snaps=H.setdefault("snapshots",[])
prev_snaps=[s for s in snaps]   # before appending current
snaps.append({"label":label,"ratios":cur}); json.dump(H,open(HIST,"w"),indent=1)

print(f"=== pg35 erosion tracker: {label} ({len(cur)} categories) ===")
eroded,softening=[],[]
SOFT_REL=0.03   # >3% worse-than-median speedup => note (catches 70x->68x)
for c,r in cur.items():
    e=env.get(c)
    if not e: continue
    rmax,rmed,rmin=e["ratio_max"],e["ratio_median"],e["ratio_min"]
    guard=rmax+max(0.03,0.08*max(rmax,0.05))
    if r>guard or (rmed<=0.85 and r>=0.95):
        eroded.append((c,r,e))
    elif rmed>0 and r>rmed*(1+SOFT_REL):   # still within envelope but softened vs median
        drop=(r-rmed)/rmed*100.0
        softening.append((c,r,rmed,drop,e))

if eroded:
    print(f"  ⛔ {len(eroded)} ERODED below historical envelope (blocks item):")
    for c,r,e in sorted(eroded,key=lambda x:-x[1]):
        print(f"     {c:<22} now={sp(r)}[{w(r)}] r={r:.4f}  hist min/med/max r={e['ratio_min']}/{e['ratio_median']}/{e['ratio_max']}")
else:
    print("  ✓ no category eroded below its historical envelope")

if softening:
    print(f"  ⚠ {len(softening)} SOFTENING (>3% slower vs median, still winning — note):")
    for c,r,rmed,drop,e in sorted(softening,key=lambda x:-x[3]):
        print(f"     {c:<22} {sp(rmed)}(med) -> {sp(r)}(now)   -{drop:.1f}%   r {rmed:.4f}->{r:.4f}")
else:
    print("  ✓ no category softened >3% vs its historical median")

# trend across program snapshots (>=3 needed)
if len(snaps)>=3:
    print("  trend (monotonic worsening over last 3 program snapshots):")
    worsening=[]
    for c in cur:
        seq=[s["ratios"].get(c) for s in snaps[-3:] if c in s["ratios"]]
        if len(seq)==3 and seq[0]<seq[1]<seq[2]:   # ratio rising = Nano relatively slower each step
            worsening.append((c,seq))
    if worsening:
        for c,seq in worsening: print(f"     {c:<22} r {seq[0]:.4f} -> {seq[1]:.4f} -> {seq[2]:.4f}  (declining 3 in a row)")
    else:
        print("     none")
print("  watch (closest to PG): "+", ".join(f"{c}={sp(cur[c])}[{w(cur[c])}]" for c in ["INNER JOIN","LEFT JOIN","4-table JOIN","ORDER+LIMIT","Prepared stmts"] if c in cur))
