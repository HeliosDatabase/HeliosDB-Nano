from pathlib import Path
import subprocess,re,shutil,hashlib,json,datetime,difflib
p=Path('/home/gpc/HDB/sprint/baselines/nano/deployment-20260929');w=Path('/home/gpc/HDB/worktrees/nano-resync-20260929');out=p/'source-fence-composition-r2';out.mkdir(exist_ok=False)
patches=['physical-committed-draft/physical-committed.patch','source-fence-draft/storage-source-admission.patch','source-fence-draft/storage-durable-phase-helper.patch','library-source-admission.patch','transaction-source-admission.patch','library-session-source-admission.patch','background-source-admission.patch','wal-group-admission.patch','remaining-source-admission-draft/http-branch-source-admission.patch','remaining-source-admission-draft/branch-engine-propagation.patch','fast-dml-source-phase.patch','ddl-source-admission-draft/ddl-source-admission.patch','physical-certified-draft/physical-certified.patch','physical-certified-draft/main-raw-compat.patch','generation-session-transfer.patch']
# Fail before copying if an author's final artifact path differs.
for x in patches: assert (p/x).is_file(),x
rels=set()
for x in patches: rels.update(re.findall(r'^--- a/(.+)$',(p/x).read_text(),re.M))
for rel in rels:
 f=out/rel;f.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(w/rel,f)
coord=out/'src/replication/physical_source_coordinator.rs';coord.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(p/'source-fence-draft/physical_source_coordinator.rs',coord)
records=[]
for name in patches:
 patchfile=p/name
 helper=None
 if name=='fast-dml-source-phase.patch':
  # Both independently authored engine deltas insert immediately before Begin.
  # Apply the unchanged fast-DML hunks, then insert its exact reviewed helper
  # at the still-unique Begin anchor in the composed engine.
  rel='src/storage/engine.rs'; base=(w/rel).read_text(); proposed=(p/'fast-dml-source-phase-draft'/rel).read_text()
  a=proposed.index('    /// Preserve the existing caller result while refusing to certify a source')
  b=proposed.index('    /// Begin a transaction\n',a);helper=proposed[a:b]
  proposed=proposed[:a]+proposed[b:]
  patchfile=p/'fast-dml-source-phase-composition-adapter.patch'
  patchfile.write_text(''.join(difflib.unified_diff(base.splitlines(True),proposed.splitlines(True),fromfile='a/'+rel,tofile='b/'+rel)))
 result=subprocess.run(['git','apply','--unsafe-paths','--directory='+str(out),str(patchfile)],cwd='/tmp',capture_output=True,text=True)
 if result.returncode==0 and helper:
  f=out/'src/storage/engine.rs';body=f.read_text();anchor='    /// Begin a transaction\n    pub fn begin_transaction';assert body.count(anchor)==1
  f.write_text(body.replace(anchor,helper+anchor))
 records.append({'patch':name,'sha256':hashlib.sha256((p/name).read_bytes()).hexdigest(),'exit':result.returncode,'stderr':result.stderr})
 if result.returncode: break
(p/'source-fence-composition-r2.json').write_text(json.dumps({'at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'output':str(out),'patches':records},indent=2)+'\n');print(json.dumps(records,indent=2))
