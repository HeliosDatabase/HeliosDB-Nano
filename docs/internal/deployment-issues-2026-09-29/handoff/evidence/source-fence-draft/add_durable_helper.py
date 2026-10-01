from pathlib import Path
import difflib,hashlib,json
out=Path('/home/gpc/HDB/sprint/baselines/nano/deployment-20260929/source-fence-draft')
base=out/'engine-proposal.rs';s=base.read_text()
def transform(t):
 needle='/// Storage engine\npub struct StorageEngine {';assert t.count(needle)==1
 t=t.replace(needle,'''/// Optional complete durable-operation phase. In ordinary builds this is a
/// no-op; an enabled source disables certification if an armed phase fails.
pub(crate) struct PhysicalStoragePhase {
    #[cfg(feature = "ha-tier1")]
    phase: Option<crate::replication::physical_source_coordinator::PhysicalDurablePhase>,
}
impl PhysicalStoragePhase {
    pub(crate) fn complete(mut self) {
        #[cfg(feature = "ha-tier1")]
        if let Some(phase) = self.phase.take() { phase.complete(); }
        #[cfg(not(feature = "ha-tier1"))]
        let _ = &mut self;
    }
}

'''+needle,1)
 needle='    /// Begin a transaction\n';assert t.count(needle)==1
 t=t.replace(needle,'''    /// Arm only after ordinary validation, immediately before the first
    /// semantic durable write. Complete after every related write/metadata step.
    pub(crate) fn physical_durable_phase(&self, reason: &'static str) -> Result<PhysicalStoragePhase> {
        #[cfg(not(feature = "ha-tier1"))]
        let _ = reason;
        Ok(PhysicalStoragePhase {
            #[cfg(feature = "ha-tier1")]
            phase: self.physical_source_coordinator.as_ref()
                .map(|coordinator| coordinator.begin_durable_phase(reason)
                    .map_err(|error| Error::storage(error.to_string())))
                .transpose()?,
        })
    }

'''+needle,1);return t
t=transform(s);(out/'engine-durable-phase-proposal.rs').write_text(t)
(out/'storage-durable-phase-helper.patch').write_text(''.join(difflib.unified_diff(s.splitlines(True),t.splitlines(True),fromfile='a/src/storage/engine.rs',tofile='b/src/storage/engine.rs')))
(out/'storage-durable-phase-helper.json').write_text(json.dumps({'base':str(base),'base_sha256':hashlib.sha256(s.encode()).hexdigest(),'draft_sha256':hashlib.sha256(t.encode()).hexdigest(),'apply_after':'storage-source-admission.patch cbb5cfe4; nonoverlapping with branch-engine-propagation.patch'},indent=2)+'\n')
branch=out.parent/'remaining-source-admission-draft/src/storage/engine.rs'
(branch.parent/'engine-composed.rs').write_text(transform(branch.read_text()))
