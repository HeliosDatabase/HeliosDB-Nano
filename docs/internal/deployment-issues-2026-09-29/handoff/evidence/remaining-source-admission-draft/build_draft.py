from pathlib import Path
import difflib,hashlib,json
base=Path('/home/gpc/HDB/worktrees/nano-resync-20260929')
out=Path('/home/gpc/HDB/sprint/baselines/nano/deployment-20260929/remaining-source-admission-draft')
records=[];patches=[]
def save(rel,s,t):
 p=out/rel;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(t)
 patches.append(''.join(difflib.unified_diff(s.splitlines(True),t.splitlines(True),fromfile='a/'+rel,tofile='b/'+rel)))
 records.append({'file':rel,'base_sha256':hashlib.sha256(s.encode()).hexdigest(),'draft_sha256':hashlib.sha256(t.encode()).hexdigest()})
# Actual daemon HTTP direct-storage routes. No await occurs in any guarded body.
for rel,names in [('src/api/handlers/data_handler.rs',['insert_data']),('src/api/handlers/branch_handler.rs',['create_branch','delete_branch','merge_branch']),('src/api/routes/webhooks.rs',['handle_event_with_storage'])]:
 s=(base/rel).read_text();t=s
 for name in names:
  start=t.index('fn '+name+'(');brace=t.index('{',start)
  call='\n    let _physical_operation = state.db.storage.physical_source_operation()?;'
  t=t[:brace+1]+call+t[brace+1:]
 save(rel,s,t)
rel='src/storage/branch.rs';s=(base/rel).read_text();t=s
needle='pub struct BranchManager {\n';assert t.count(needle)==1
t=t.replace(needle,needle+'''    #[cfg(feature = "ha-tier1")]
    physical_source_coordinator: Option<Arc<crate::replication::physical_source_coordinator::PhysicalSourceCoordinator>>,
''',1)
needle='''        Ok(Self {
            db,
            registry:''';assert t.count(needle)==2
t=t.replace(needle,'''        Ok(Self {
            #[cfg(feature = "ha-tier1")]
            physical_source_coordinator: None,
            db,
            registry:''')
needle='impl BranchManager {\n';assert t.count(needle)==1
t=t.replace(needle,'''// No-op in ordinary builds; enabled source phases retain their own !Send permit.
struct BranchDurablePhase {
    #[cfg(feature = "ha-tier1")]
    inner: Option<crate::replication::physical_source_coordinator::PhysicalDurablePhase>,
}
impl BranchDurablePhase {
    fn complete(mut self) {
        #[cfg(feature = "ha-tier1")]
        if let Some(phase) = self.inner.take() { phase.complete(); }
        #[cfg(not(feature = "ha-tier1"))]
        let _ = &mut self;
    }
}

impl BranchManager {
    /// Engine construction only, before publishing this manager or transactions.
    #[cfg(feature = "ha-tier1")]
    pub(crate) fn set_physical_source_coordinator(
        &mut self,
        coordinator: Option<Arc<crate::replication::physical_source_coordinator::PhysicalSourceCoordinator>>,
    ) {
        self.physical_source_coordinator = coordinator;
    }
    #[cfg(feature = "ha-tier1")]
    fn physical_source_operation(&self) -> Result<Option<crate::replication::physical_source_coordinator::PhysicalOperationPermit>> {
        self.physical_source_coordinator.as_ref()
            .map(|coordinator| coordinator.operation().map_err(|error| Error::storage(error.to_string())))
            .transpose()
    }
    #[cfg(not(feature = "ha-tier1"))]
    fn physical_source_operation(&self) -> Result<Option<()>> { Ok(None) }

    fn begin_durable_phase(&self, reason: &'static str) -> Result<BranchDurablePhase> {
        #[cfg(not(feature = "ha-tier1"))]
        let _ = reason;
        Ok(BranchDurablePhase {
            #[cfg(feature = "ha-tier1")]
            inner: self.physical_source_coordinator.as_ref()
                .map(|coordinator| coordinator.begin_durable_phase(reason).map_err(|error| Error::storage(error.to_string())))
                .transpose()?,
        })
    }
''',1)
# Modify named function spans without changing any unrelated same-shaped code.
def change_span(text,first,last,fn):
 a=text.index('    '+first);b=text.index('    '+last,a+1)
 return text[:a]+fn(text[a:b])+text[b:]
def create(x):
 x=x.replace(') -> Result<BranchId> {',') -> Result<BranchId> {\n        let _physical_operation = self.physical_source_operation()?;',1)
 x=x.replace('        self.db\n            .put(&meta_key, &meta_value)', '        let durable_phase = self.begin_durable_phase("branch create did not complete durable metadata")?;\n        self.db\n            .put(&meta_key, &meta_value)',1)
 x=x.replace('        Ok(branch_id)','        durable_phase.complete();\n        Ok(branch_id)',1);return x
def drop(x):
 x=x.replace(') -> Result<()> {',') -> Result<()> {\n        let _physical_operation = self.physical_source_operation()?;',1)
 x=x.replace('        self.db\n            .put(&meta_key, &meta_value)', '        let durable_phase = self.begin_durable_phase("branch drop did not complete durable metadata and GC")?;\n        self.db\n            .put(&meta_key, &meta_value)',1)
 x=x.replace('        Ok(())','        durable_phase.complete();\n        Ok(())',1);return x
def merge(x):
 x=x.replace(') -> Result<MergeResult> {',') -> Result<MergeResult> {\n        let _physical_operation = self.physical_source_operation()?;',1)
 x=x.replace('        let merged_keys = self.apply_merge(', '        let durable_phase = self.begin_durable_phase("branch merge did not complete rows and metadata")?;\n        let merged_keys = self.apply_merge(',1)
 # The earlier manual-conflict return is deliberately outside this phase.
 x=x.replace('        Ok(MergeResult {','        durable_phase.complete();\n        Ok(MergeResult {',1);return x
def gc_data(x):
 x=x.replace('        // Delete collected keys','        let durable_phase = self.begin_durable_phase("branch GC did not complete deletion")?;\n\n        // Delete collected keys',1)
 x=x.replace('        Ok(())','        durable_phase.complete();\n        Ok(())',1);return x
def gc_eligible(x):
 x=x.replace(') -> Result<usize> {',') -> Result<usize> {\n        let _physical_operation = self.physical_source_operation()?;',1)
 x=x.replace('        // Process eligible branches','        let durable_phase = self.begin_durable_phase("branch GC did not persist its completed queue")?;\n\n        // Process eligible branches',1)
 x=x.replace('        Ok(gc_count)','        durable_phase.complete();\n        Ok(gc_count)',1);return x
t=change_span(t,'pub fn create_branch(','pub fn drop_branch(',create)
t=change_span(t,'pub fn drop_branch(','fn schedule_branch_gc(',drop)
t=change_span(t,'pub fn merge_branch(','fn find_merge_base(',merge)
t=change_span(t,'fn gc_branch_data(','pub fn gc_eligible_branches(',gc_data)
t=change_span(t,'pub fn gc_eligible_branches(','pub fn run_gc(',gc_eligible)
# Branch transaction setter forwards the exact coordinator into the inner Txn.
needle='impl BranchTransaction {\n';assert t.count(needle)==1
t=t.replace(needle,needle+'''    #[cfg(feature = "ha-tier1")]
    pub(crate) fn set_physical_source_coordinator(
        &mut self,
        coordinator: Option<Arc<crate::replication::physical_source_coordinator::PhysicalSourceCoordinator>>,
    ) {
        self.tx.set_physical_source_coordinator(coordinator);
    }
''',1)
t += (out/'branch_tests.txt').read_text()
save(rel,s,t)
(out/'http-branch-source-admission.patch').write_text(''.join(patches))
(out/'http-branch-source-admission.json').write_text(json.dumps(records,indent=2)+'\n')
