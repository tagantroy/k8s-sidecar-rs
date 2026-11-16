//! Keeps the folder in line with the objects seen so far.
//!
//! Only hashes are kept in memory, not file contents. When two objects write the same
//! path, the smallest `ObjKey` wins (deterministic, independent of event order). If the
//! winner goes away while others still claim the path, the caller re-reads the next
//! owner from the API (`Outcome::refetch`) and applies it again.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::metrics::Metrics;
use crate::object::{Obj, ObjKey};
use crate::paths;

#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub folder: PathBuf,
    pub folder_annotation: String,
    pub unique_filenames: bool,
    pub file_mode: Option<u32>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Something on disk changed: a reload is due.
    pub changed: bool,
    /// Objects to read again from the API and `apply` (they now own a contested file).
    pub refetch: Vec<ObjKey>,
}

impl Outcome {
    pub fn merge(&mut self, other: Outcome) {
        self.changed |= other.changed;
        self.refetch.extend(other.refetch);
    }
}

pub struct Syncer {
    cfg: SyncConfig,
    /// Object → the paths it claims, with the hash of its content for each.
    objects: HashMap<ObjKey, BTreeMap<PathBuf, u64>>,
    /// Path → every object claiming it.
    owners: HashMap<PathBuf, BTreeSet<ObjKey>>,
    /// Path → hash of what this process last wrote (or found identical on disk).
    written: HashMap<PathBuf, u64>,
    metrics: Metrics,
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

impl Syncer {
    pub fn new(cfg: SyncConfig, metrics: Metrics) -> Self {
        Syncer {
            cfg,
            objects: HashMap::new(),
            owners: HashMap::new(),
            written: HashMap::new(),
            metrics,
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &ObjKey> {
        self.objects.keys()
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The files an object should produce: path → contents.
    fn desired(&self, obj: &Obj) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let dir = match paths::target_dir(
            &self.cfg.folder,
            obj.annotations
                .get(&self.cfg.folder_annotation)
                .map(String::as_str),
        ) {
            Ok(d) => d,
            Err(e) => {
                warn!(object = %obj.key, error = %e, "skipping object");
                self.metrics.skipped("bad_target_dir");
                return out;
            }
        };
        let unique = self.cfg.unique_filenames.then(|| {
            (
                obj.key.namespace.as_str(),
                obj.key.kind.as_str(),
                obj.key.name.as_str(),
            )
        });
        for (key, value) in &obj.data {
            if key.ends_with(".url") {
                warn!(object = %obj.key, key, "skipping key: downloading .url keys is not supported");
                self.metrics.skipped("url_key");
                continue;
            }
            match paths::file_name(key, unique) {
                Ok(name) => {
                    out.insert(dir.join(name), value.clone());
                }
                Err(e) => {
                    warn!(object = %obj.key, error = %e, "skipping key");
                    self.metrics.skipped("bad_key");
                }
            }
        }
        out
    }

    pub fn apply(&mut self, obj: &Obj) -> Outcome {
        let desired = self.desired(obj);
        let mut outcome = Outcome::default();

        let old: Vec<PathBuf> = self
            .objects
            .get(&obj.key)
            .map(|m| {
                m.keys()
                    .filter(|p| !desired.contains_key(*p))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for path in old {
            outcome.merge(self.release(&obj.key, &path));
        }

        let mut claimed = BTreeMap::new();
        for (path, content) in desired {
            let h = hash(&content);
            claimed.insert(path.clone(), h);
            let owners = self.owners.entry(path.clone()).or_default();
            owners.insert(obj.key.clone());
            let winner = owners.first().expect("just inserted").clone();
            if winner != obj.key {
                warn!(path = %path.display(), object = %obj.key, winner = %winner, "file is provided by more than one object; the first one wins");
                self.metrics.conflict();
                continue;
            }
            outcome.changed |= self.write(&path, &content, h);
        }
        self.objects.insert(obj.key.clone(), claimed);
        self.metrics.set_objects(self.objects.len());
        outcome
    }

    pub fn remove(&mut self, key: &ObjKey) -> Outcome {
        let mut outcome = Outcome::default();
        if let Some(paths) = self.objects.remove(key) {
            for path in paths.into_keys() {
                outcome.merge(self.release(key, &path));
            }
            debug!(object = %key, "removed");
        }
        self.metrics.set_objects(self.objects.len());
        outcome
    }

    /// `key` no longer claims `path`: delete the file, or hand it to the next owner.
    fn release(&mut self, key: &ObjKey, path: &Path) -> Outcome {
        let mut outcome = Outcome::default();
        let Some(owners) = self.owners.get_mut(path) else {
            return outcome;
        };
        let was_winner = owners.first() == Some(key);
        owners.remove(key);
        match owners.first().cloned() {
            None => {
                self.owners.remove(path);
                outcome.changed |= self.delete(path);
            }
            Some(next) if was_winner => outcome.refetch.push(next),
            Some(_) => {}
        }
        outcome
    }

    fn write(&mut self, path: &Path, content: &[u8], h: u64) -> bool {
        match self.written.get(path) {
            Some(prev) if *prev == h => return false,
            None if std::fs::read(path).is_ok_and(|on_disk| on_disk == content) => {
                // Left by an earlier run of this container (the folder outlives restarts).
                self.written.insert(path.to_path_buf(), h);
                return false;
            }
            _ => {}
        }
        match write_atomic(path, content, self.cfg.file_mode) {
            Ok(()) => {
                info!(path = %path.display(), bytes = content.len(), "wrote");
                self.written.insert(path.to_path_buf(), h);
                self.metrics.written();
                true
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "write failed");
                self.metrics.io_error();
                false
            }
        }
    }

    fn delete(&mut self, path: &Path) -> bool {
        self.written.remove(path);
        match std::fs::remove_file(path) {
            Ok(()) => {
                info!(path = %path.display(), "deleted");
                self.metrics.deleted();
                remove_empty_dirs(path, &self.cfg.folder);
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "delete failed");
                self.metrics.io_error();
                false
            }
        }
    }
}

/// Write via a temporary file in the same directory and rename, so readers never see a
/// half-written file.
fn write_atomic(path: &Path, content: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Remove directories left empty by a delete, up to (not including) the folder.
fn remove_empty_dirs(path: &Path, folder: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == folder || !d.starts_with(folder) || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Kind;

    fn obj(ns: &str, name: &str, files: &[(&str, &str)], annotations: &[(&str, &str)]) -> Obj {
        Obj {
            key: ObjKey {
                namespace: ns.into(),
                kind: Kind::ConfigMap,
                name: name.into(),
            },
            annotations: annotations
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            data: files
                .iter()
                .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                .collect(),
        }
    }

    fn syncer(dir: &Path, unique: bool) -> Syncer {
        Syncer::new(
            SyncConfig {
                folder: dir.to_path_buf(),
                folder_annotation: "grafana_folder".into(),
                unique_filenames: unique,
                file_mode: Some(0o640),
            },
            Metrics::new(),
        )
    }

    fn read(dir: &Path, rel: &str) -> Option<String> {
        std::fs::read_to_string(dir.join(rel)).ok()
    }

    #[test]
    fn write_update_remove() {
        let t = tempfile::tempdir().unwrap();
        let mut s = syncer(t.path(), false);
        let a = obj("ns", "a", &[("x.json", "1"), ("y.json", "2")], &[]);
        assert!(s.apply(&a).changed);
        assert_eq!(read(t.path(), "x.json").as_deref(), Some("1"));

        // Same content again: nothing to do, no reload.
        assert_eq!(s.apply(&a), Outcome::default());

        // y.json dropped, x.json changed.
        let a2 = obj("ns", "a", &[("x.json", "3")], &[]);
        assert!(s.apply(&a2).changed);
        assert_eq!(read(t.path(), "x.json").as_deref(), Some("3"));
        assert_eq!(read(t.path(), "y.json"), None);

        assert!(s.remove(&a2.key).changed);
        assert_eq!(read(t.path(), "x.json"), None);
        assert!(s.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_applied() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let mut s = syncer(t.path(), false);
        s.apply(&obj("ns", "a", &[("x.json", "1")], &[]));
        let mode = std::fs::metadata(t.path().join("x.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn annotation_folder_and_cleanup() {
        let t = tempfile::tempdir().unwrap();
        let mut s = syncer(t.path(), false);
        let a = obj(
            "ns",
            "a",
            &[("x.json", "1")],
            &[("grafana_folder", "Home/Rooms")],
        );
        s.apply(&a);
        assert_eq!(read(t.path(), "Home/Rooms/x.json").as_deref(), Some("1"));

        // Moved to another folder: old file and its now-empty directories go.
        let a2 = obj(
            "ns",
            "a",
            &[("x.json", "1")],
            &[("grafana_folder", "Other")],
        );
        assert!(s.apply(&a2).changed);
        assert_eq!(read(t.path(), "Other/x.json").as_deref(), Some("1"));
        assert!(!t.path().join("Home").exists());
    }

    #[test]
    fn escaping_annotation_writes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let mut s = syncer(t.path(), false);
        let a = obj(
            "ns",
            "a",
            &[("x.json", "1")],
            &[("grafana_folder", "../../etc")],
        );
        assert_eq!(s.apply(&a), Outcome::default());
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
    }

    #[test]
    fn url_keys_are_skipped() {
        let t = tempfile::tempdir().unwrap();
        let mut s = syncer(t.path(), false);
        s.apply(&obj(
            "ns",
            "a",
            &[("dash.url", "https://example.com/d.json"), ("ok.json", "1")],
            &[],
        ));
        assert_eq!(read(t.path(), "dash.url"), None);
        assert_eq!(read(t.path(), "ok.json").as_deref(), Some("1"));
    }

    #[test]
    fn unique_filenames() {
        let t = tempfile::tempdir().unwrap();
        let mut s = syncer(t.path(), true);
        s.apply(&obj("a", "d", &[("x.json", "1")], &[]));
        s.apply(&obj("b", "d", &[("x.json", "2")], &[]));
        assert_eq!(
            read(t.path(), "namespace_a.configmap_d.x.json").as_deref(),
            Some("1")
        );
        assert_eq!(
            read(t.path(), "namespace_b.configmap_d.x.json").as_deref(),
            Some("2")
        );
    }

    #[test]
    fn conflicts_smallest_key_wins_and_hands_over() {
        let t = tempfile::tempdir().unwrap();
        let mut s = syncer(t.path(), false);
        let b = obj("b", "d", &[("x.json", "from b")], &[]);
        let a = obj("a", "d", &[("x.json", "from a")], &[]);

        s.apply(&b);
        assert_eq!(read(t.path(), "x.json").as_deref(), Some("from b"));
        // a sorts first: it takes over regardless of arrival order.
        assert!(s.apply(&a).changed);
        assert_eq!(read(t.path(), "x.json").as_deref(), Some("from a"));
        // An update to the loser doesn't touch the file.
        assert!(!s.apply(&obj("b", "d", &[("x.json", "b v2")], &[])).changed);
        assert_eq!(read(t.path(), "x.json").as_deref(), Some("from a"));

        // The winner goes: the file stays, and b must be re-read to take it over.
        let out = s.remove(&a.key);
        assert_eq!(out.refetch, vec![b.key.clone()]);
        assert!(!out.changed);
        assert!(s.apply(&obj("b", "d", &[("x.json", "b v2")], &[])).changed);
        assert_eq!(read(t.path(), "x.json").as_deref(), Some("b v2"));

        // The last owner goes: the file goes.
        assert!(s.remove(&b.key).changed);
        assert_eq!(read(t.path(), "x.json"), None);
    }

    #[test]
    fn identical_file_from_earlier_run_is_not_rewritten() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("x.json"), "1").unwrap();
        let mut s = syncer(t.path(), false);
        assert!(!s.apply(&obj("ns", "a", &[("x.json", "1")], &[])).changed);
        assert!(s.apply(&obj("ns", "a", &[("x.json", "2")], &[])).changed);
    }
}
