from pathlib import Path
import difflib, hashlib, subprocess, re
base=Path('/home/gpc/HDB/worktrees/nano-resync-20260929')
out=Path(__file__).resolve().parent
files=['src/lib.rs','src/storage/catalog.rs']
original={f:(base/f).read_text() for f in files}
lib=original[files[0]]
a=lib.index('        // Log to WAL for replication before creating (schema will be moved)',lib.index('    fn execute_create_table_plan'))
b=lib.index('        // KanttBan #23',a)
tail_end=lib.index('\n        Ok(1)\n    }',b)
metadata=re.sub(r'(?<![\w.])name\.clone\(\)', 'name.to_string()', lib[b:tail_end]).replace('name.as_str()', 'name')
lib=lib[:a]+'''        // Catalog keeps its phase armed through all SQL-level metadata.
        // Its own preflight still rejects duplicate names before durable work.
        catalog.create_table_with_finalize(name, schema, || {
            self.finalize_create_table_metadata(name, columns, constraints, original_sql)
        })?;
        Ok(1)
    }

    fn finalize_create_table_metadata(
        &self,
        name: &str,
        columns: &[sql::logical_plan::ColumnDef],
        constraints: &[sql::logical_plan::TableConstraint],
        original_sql: Option<&str>,
    ) -> Result<()> {
        let catalog = self.storage.catalog();
'''+metadata+'\n        Ok(())\n    }'+lib[tail_end+len('\n        Ok(1)\n    }'):]
cat=original[files[1]]
a=cat.index('    pub fn create_table(&self, table_name: &str, schema: Schema) -> Result<()> {')
b=a+len('    pub fn create_table(&self, table_name: &str, schema: Schema) -> Result<()> {')
cat=cat[:b]+'''
        self.create_table_with_finalize(table_name, schema, || Ok(()))
    }

    /// Keep the complete CREATE TABLE semantic unit inside one durable phase.
    /// The callback runs only after this invocation creates the relation. It
    /// must finish identity, partition, constraint and related metadata before
    /// returning success; an error disables certification without changing the
    /// normal SQL error or attempting to invent transactional DDL rollback.
    pub(crate) fn create_table_with_finalize<F>(
        &self,
        table_name: &str,
        schema: Schema,
        finalize: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        let _physical_operation = self.storage.physical_source_operation()?;
'''+cat[b:]
a=cat.index('        // Log CreateTable to WAL first',a)
cat=cat[:a]+'''        let durable_phase = self
            .storage
            .physical_durable_phase("CREATE TABLE did not complete schema, indexes and SQL metadata")?;

'''+cat[a:]
a=cat.index('        self.storage.bump_schema_generation();',a)+len('        self.storage.bump_schema_generation();')
cat=cat[:a]+'''

        finalize()?;
        durable_phase.complete();'''+cat[a:]
cat+='\n'+(out/'tests.rs').read_text()
for f,text in zip(files,[lib,cat]):
    (out/f).parent.mkdir(parents=True,exist_ok=True)
    (out/f).write_text(text)
    subprocess.run(['rustfmt','--edition','2021','--config-path',str(base/'.rustfmt.toml'),'--config','skip_children=true',str(out/f)],check=True)
patch=''.join(''.join(difflib.unified_diff(original[f].splitlines(True),(out/f).read_text().splitlines(True),fromfile='a/'+f,tofile='b/'+f)) for f in files)
(out/'ddl-source-admission.patch').write_text(patch)
(out/'base-source.sha256').write_text(''.join(hashlib.sha256(original[f].encode()).hexdigest()+'  '+f+'\n' for f in files))
print(hashlib.sha256(patch.encode()).hexdigest(),len(patch.splitlines()))
