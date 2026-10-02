from pathlib import Path
import difflib,hashlib,subprocess
base=Path('/home/gpc/HDB/worktrees/nano-resync-20260929');out=Path(__file__).parent
original=(base/'src/lib.rs').read_text()
needle='    /// Destroy an active session and release all resources'
a=original.index(needle)
changed=original[:a]+(out/'portability_method.rs').read_text()+'\n'+original[a:]+'\n'+(out/'portability_tests.rs').read_text()
file=out/'src/lib.rs';file.write_text(changed)
subprocess.run(['rustfmt','--edition','2021','--config-path',str(base/'.rustfmt.toml'),'--config','skip_children=true',str(file)],check=True)
patch=''.join(difflib.unified_diff(original.splitlines(True),file.read_text().splitlines(True),fromfile='a/src/lib.rs',tofile='b/src/lib.rs'))
(out/'generation-portability.patch').write_text(patch)
print(hashlib.sha256(patch.encode()).hexdigest())
