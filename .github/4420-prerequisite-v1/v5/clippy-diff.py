#!/usr/bin/env python3
"""Compare identical-env Cargo JSON diagnostics without stashing/checkouts."""
from collections import Counter
import argparse
import json
from pathlib import Path
import re
from run_stage import diagnostic_problems

def findings(path):
    found=Counter()
    text=path.read_text(errors='replace')
    hard=diagnostic_problems(text)
    for line in text.splitlines():
        try: item=json.loads(line)
        except json.JSONDecodeError: continue
        if item.get('reason')!='compiler-message': continue
        message=item['message']; code=(message.get('code') or {}).get('code','')
        if not code and (message['message'].startswith('aborting due to ') or re.fullmatch(r'\d+ warnings? emitted', message['message'])): continue
        if message.get('level') not in {'warning','error'}: continue
        primary=next((span for span in message.get('spans',[]) if span.get('is_primary')), {})
        text=tuple(part.get('text','').strip() for part in primary.get('text',[]))
        found[(code,message['message'],primary.get('file_name',''),text)]+=1
    return found,hard

parser=argparse.ArgumentParser(); parser.add_argument('baseline',type=Path);parser.add_argument('candidate',type=Path)
args=parser.parse_args();before,bad_before=findings(args.baseline);after,bad_after=findings(args.candidate)
added=after-before
print(json.dumps({'new_findings':[{'fingerprint':list(key),'count':count} for key,count in added.items()],
                  'baseline_compile_errors':bad_before,'candidate_compile_errors':bad_after},indent=2))
raise SystemExit(1 if added or bad_before or bad_after else 0)
