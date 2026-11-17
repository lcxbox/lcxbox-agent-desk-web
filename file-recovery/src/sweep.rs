//! Mark-sweep object collection under the vault lock.
//!
//! Roots are the objects of every record whose material is saved (including
//! in-flight records), with their recorded base chains and entry tables, the
//! manifests and batches of unfinished command snapshots, and the latest
//! manifests of live conversations. The
//! sweep lists each namespace directory, removes unmarked objects and
//! temporary files, and replaces the namespace counts with absolute values
//! from the listing, which also corrects counts left behind by a crash
//! between an object rename and its ledger update.
use super::*;
use ledger::NamespaceUsage;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub removed_objects: u64,
    pub retained_objects: u64,
    /// Unreferenced objects whose removal failed; retried by the next sweep.
    pub failed_removals: u64,
}

impl LockedVault {
    pub fn sweep(&mut self, now_ms: u64) -> io::Result<SweepReport> {
        // Roots must be durable before anything they no longer hold is deleted.
        self.flush()?;
        let mut marked: HashSet<(String, String)> = HashSet::new();
        let mut mark = |namespace: &str, objects: Vec<ObjRef>| {
            for object in objects {
                for sha in std::iter::once(object.sha256).chain(object.bases) {
                    marked.insert((namespace.to_owned(), sha));
                }
            }
        };
        for record in self.ledger.records.values() {
            if record.material != MaterialState::Saved {
                continue;
            }
            let namespace = record.namespace();
            mark(&namespace, self.record_objects(record)?);
            if let Some(snapshot) = &record.snapshot {
                mark(&namespace, self.snapshot_roots(&namespace, snapshot)?);
            }
        }
        for manifest in self.ledger.manifests.values() {
            let namespace = manifest.scope.namespace();
            mark(
                &namespace,
                self.manifest_roots(&namespace, &manifest.sha256)?,
            );
        }
        let mut report = SweepReport::default();
        let mut counts = BTreeMap::new();
        for namespace in self.storage.list_namespaces()? {
            self.storage.clear_temporary(&namespace)?;
            let mut usage = NamespaceUsage::default();
            for (name, len) in self.storage.list_objects(&namespace)? {
                let keep = marked.contains(&(namespace.clone(), name.clone()));
                if !keep {
                    match self.storage.remove_object(&namespace, &name) {
                        Ok(()) => {
                            report.removed_objects += 1;
                            continue;
                        }
                        Err(_) => report.failed_removals += 1,
                    }
                } else {
                    report.retained_objects += 1;
                }
                usage.objects += 1;
                usage.bytes += round_block(len);
            }
            if usage.objects > 0 {
                counts.insert(namespace, usage);
            }
        }
        self.ledger.namespaces = counts;
        self.ledger.last_sweep_at_ms = self.ledger.last_sweep_at_ms.max(now_ms);
        self.persist()?;
        Ok(report)
    }
}
