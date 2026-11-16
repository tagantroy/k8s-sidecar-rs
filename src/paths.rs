//! Where an object's files go. Annotation values and data keys come from any namespace
//! the sidecar watches, so they must never lead outside the configured folder.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum PathError {
    /// The target-directory annotation escapes the folder (`..`, or an absolute path outside it).
    OutsideFolder(String),
    /// The data key can't be used as a file name.
    BadKey(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::OutsideFolder(v) => {
                write!(f, "target directory {v:?} is outside the folder")
            }
            PathError::BadKey(k) => write!(f, "data key {k:?} is not a valid file name"),
        }
    }
}

/// The directory for an object: `folder`, or the annotation's directory inside it.
/// Relative annotation values are joined to `folder`; absolute ones must lie inside it.
pub fn target_dir(folder: &Path, annotation: Option<&str>) -> Result<PathBuf, PathError> {
    let Some(value) = annotation.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(folder.to_path_buf());
    };
    let path = Path::new(value);
    let mut out = folder.to_path_buf();
    let rel = if path.is_absolute() {
        path.strip_prefix(folder)
            .map_err(|_| PathError::OutsideFolder(value.to_string()))?
    } else {
        path
    };
    for c in rel.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PathError::OutsideFolder(value.to_string()));
            }
        }
    }
    Ok(out)
}

/// The file name for a data key, optionally made unique per object as kiwigrid does:
/// `namespace_<ns>.<kind>_<name>.<key>`.
pub fn file_name(key: &str, unique: Option<(&str, &str, &str)>) -> Result<String, PathError> {
    let bad = key.is_empty() || key == "." || key == ".." || key.contains(['/', '\\', '\0']);
    if bad {
        return Err(PathError::BadKey(key.to_string()));
    }
    Ok(match unique {
        Some((namespace, kind, name)) => format!("namespace_{namespace}.{kind}_{name}.{key}"),
        None => key.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: &str = "/tmp/dashboards";

    fn dir(a: Option<&str>) -> Result<PathBuf, PathError> {
        target_dir(Path::new(F), a)
    }

    #[test]
    fn no_annotation_is_the_folder() {
        assert_eq!(dir(None).unwrap(), PathBuf::from(F));
        assert_eq!(dir(Some("  ")).unwrap(), PathBuf::from(F));
    }

    #[test]
    fn relative_and_absolute_inside() {
        assert_eq!(
            dir(Some("Kubernetes")).unwrap(),
            PathBuf::from("/tmp/dashboards/Kubernetes")
        );
        assert_eq!(
            dir(Some("./a/b")).unwrap(),
            PathBuf::from("/tmp/dashboards/a/b")
        );
        assert_eq!(
            dir(Some("/tmp/dashboards/Home")).unwrap(),
            PathBuf::from("/tmp/dashboards/Home")
        );
    }

    #[test]
    fn escapes_are_refused() {
        for v in [
            "..",
            "../etc",
            "a/../../etc",
            "/etc",
            "/tmp/dashboards/../x",
            "/tmp/dashboardsX",
        ] {
            assert!(dir(Some(v)).is_err(), "{v} should be refused");
        }
    }

    #[test]
    fn keys() {
        assert_eq!(file_name("node.json", None).unwrap(), "node.json");
        assert_eq!(
            file_name("node.json", Some(("monitoring", "configmap", "dash"))).unwrap(),
            "namespace_monitoring.configmap_dash.node.json"
        );
        for k in ["", ".", "..", "a/b", "a\\b"] {
            assert!(file_name(k, None).is_err(), "{k:?} should be refused");
        }
    }
}
