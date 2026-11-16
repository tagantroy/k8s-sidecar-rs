//! ConfigMaps and Secrets reduced to what the sidecar needs.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::ResourceExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    ConfigMap,
    Secret,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ConfigMap => "configmap",
            Kind::Secret => "secret",
        }
    }
}

/// Identity of an object. The ordering decides which object wins when two write the same
/// file: the smallest key (namespace, kind, name), so the outcome doesn't depend on event order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjKey {
    pub namespace: String,
    pub kind: Kind,
    pub name: String,
}

impl std::fmt::Display for ObjKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.kind.as_str(), self.namespace, self.name)
    }
}

#[derive(Debug, Clone)]
pub struct Obj {
    pub key: ObjKey,
    pub annotations: BTreeMap<String, String>,
    /// Data key → contents (ConfigMap `data` and `binaryData`; Secret `data`, decoded).
    pub data: BTreeMap<String, Vec<u8>>,
}

pub trait IntoObj {
    fn into_obj(self) -> Obj;
    fn obj_key(&self) -> ObjKey;
}

fn key_of(r: &impl ResourceExt, kind: Kind) -> ObjKey {
    ObjKey {
        namespace: r.namespace().unwrap_or_default(),
        kind,
        name: r.name_any(),
    }
}

impl IntoObj for ConfigMap {
    fn obj_key(&self) -> ObjKey {
        key_of(self, Kind::ConfigMap)
    }
    fn into_obj(self) -> Obj {
        let key = self.obj_key();
        let mut data: BTreeMap<String, Vec<u8>> = self
            .data
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, v.into_bytes()))
            .collect();
        data.extend(
            self.binary_data
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, v.0)),
        );
        Obj {
            key,
            annotations: self.metadata.annotations.unwrap_or_default(),
            data,
        }
    }
}

impl IntoObj for Secret {
    fn obj_key(&self) -> ObjKey {
        key_of(self, Kind::Secret)
    }
    fn into_obj(self) -> Obj {
        let key = self.obj_key();
        let mut data: BTreeMap<String, Vec<u8>> = self
            .data
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, v.0))
            .collect();
        // Only present on objects that weren't read back from the API (tests, dry runs).
        data.extend(
            self.string_data
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, v.into_bytes())),
        );
        Obj {
            key,
            annotations: self.metadata.annotations.unwrap_or_default(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::ByteString;
    use kube::api::ObjectMeta;

    #[test]
    fn configmap_data_and_binary() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("d".into()),
                namespace: Some("ns".into()),
                ..Default::default()
            },
            data: Some([("a.json".to_string(), "{}".to_string())].into()),
            binary_data: Some([("b.bin".to_string(), ByteString(vec![0, 1]))].into()),
            ..Default::default()
        };
        let o = cm.into_obj();
        assert_eq!(o.key.to_string(), "configmap/ns/d");
        assert_eq!(o.data["a.json"], b"{}");
        assert_eq!(o.data["b.bin"], vec![0, 1]);
    }

    #[test]
    fn secret_data() {
        let s = Secret {
            metadata: ObjectMeta {
                name: Some("s".into()),
                namespace: Some("ns".into()),
                ..Default::default()
            },
            data: Some([("ds.yaml".to_string(), ByteString(b"x: 1".to_vec()))].into()),
            ..Default::default()
        };
        let o = s.into_obj();
        assert_eq!(o.key.kind, Kind::Secret);
        assert_eq!(o.data["ds.yaml"], b"x: 1");
    }
}
