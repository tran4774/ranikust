//! Descriptors and platforms.
//!
//! OCI image-spec v1.1.1
//! [descriptor.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/descriptor.md)
//! and [image-index.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-index.md#image-index-property-descriptions)
//! (`platform`); Docker v2 schema 2 descriptors are the same shape.
//!
//! Optional properties are `Option`s throughout this crate: absent or `null`
//! reads as `None` and is not written; a present value, even an empty one,
//! is kept and written back.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::media_type::MediaType;

/// Points at content by media type, digest and size.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    /// Media type of the referenced content.
    pub media_type: MediaType,
    /// Digest of the referenced content's exact bytes.
    pub digest: Digest,
    /// Size of the referenced content, in bytes.
    pub size: u64,
    /// URLs the content may also be fetched from (foreign layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    /// Arbitrary metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    /// The content itself, base64-encoded, kept as the string read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// Platform of the referenced manifest, in an index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    /// Artifact type of the referenced manifest, in an index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<MediaType>,
}

impl Descriptor {
    /// A descriptor with only the three required properties.
    pub fn new(media_type: MediaType, digest: Digest, size: u64) -> Self {
        Self {
            media_type,
            digest,
            size,
            urls: None,
            annotations: None,
            data: None,
            platform: None,
            artifact_type: None,
        }
    }
}

/// The platform an image runs on, as in an index entry or an image config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    /// CPU architecture, in Go's `GOARCH` spelling (`amd64`, `arm64`).
    pub architecture: String,
    /// Operating system, in Go's `GOOS` spelling (`linux`).
    pub os: String,
    /// Operating system version (Windows).
    #[serde(
        rename = "os.version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub os_version: Option<String>,
    /// Required operating system features (Windows).
    #[serde(
        rename = "os.features",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub os_features: Option<Vec<String>>,
    /// CPU variant (`v7` for `arm`, `v8` for `arm64`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Reserved by the OCI spec; written by some Docker tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
}

impl Platform {
    /// A platform with only an operating system and an architecture.
    pub fn new(os: impl Into<String>, architecture: impl Into<String>) -> Self {
        Self {
            architecture: architecture.into(),
            os: os.into(),
            os_version: None,
            os_features: None,
            variant: None,
            features: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const DIGEST: &str = "sha256:5b0bcabd1ed22e9fb1310cf6c2dec7cdef19f0ad69efa1f392e94a4333501270";

    #[test]
    fn minimal_descriptor_round_trip() {
        let text = format!(
            r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{DIGEST}","size":32654}}"#
        );
        let d: Descriptor = serde_json::from_str(&text).unwrap();
        let expected = Descriptor::new(
            MediaType::OCI_LAYER_GZIP,
            Digest::parse(DIGEST).unwrap(),
            32654,
        );
        assert_eq!(d, expected);
        assert_eq!(serde_json::to_string(&d).unwrap(), text);
    }

    #[test]
    fn full_descriptor_round_trip() {
        let value = json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": DIGEST,
            "size": 7682,
            "urls": ["https://example.com/blob"],
            "annotations": {"b": "2", "a": "1"},
            "data": "e30=",
            "platform": {
                "architecture": "amd64",
                "os": "windows",
                "os.version": "10.0.17763.1040",
                "os.features": ["win32k"],
                "variant": "v3",
                "features": ["sse4"]
            },
            "artifactType": "application/vnd.example+type"
        });
        let d: Descriptor = serde_json::from_value(value.clone()).unwrap();
        let platform = d.platform.as_ref().unwrap();
        assert_eq!(platform.os_version.as_deref(), Some("10.0.17763.1040"));
        assert_eq!(
            platform.os_features.as_deref(),
            Some(&["win32k".to_owned()][..])
        );
        assert_eq!(serde_json::to_value(&d).unwrap(), value);
    }

    #[test]
    fn null_and_empty_optionals() {
        let value = json!({
            "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
            "digest": DIGEST,
            "size": 1,
            "urls": null,
            "annotations": {}
        });
        let d: Descriptor = serde_json::from_value(value).unwrap();
        assert_eq!(d.urls, None);
        assert_eq!(d.annotations, Some(BTreeMap::new()));
        let back = serde_json::to_value(&d).unwrap();
        assert_eq!(back.get("urls"), None);
        assert_eq!(back.get("annotations"), Some(&json!({})));
    }

    #[test]
    fn rejects_malformed_descriptors() {
        let cases = [
            json!({"digest": DIGEST, "size": 1}),
            json!({"mediaType": "a/b", "size": 1}),
            json!({"mediaType": "a/b", "digest": DIGEST}),
            json!({"mediaType": "a/b", "digest": "sha256:abc", "size": 1}),
            json!({"mediaType": "a/b", "digest": DIGEST, "size": -1}),
            json!({"mediaType": "a/b", "digest": DIGEST, "size": 1.5}),
            json!({"mediaType": "a/b", "digest": DIGEST, "size": "1"}),
            json!({"mediaType": "a/b", "digest": DIGEST, "size": 1, "platform": {"os": "linux"}}),
            json!({"mediaType": "a/b", "digest": DIGEST, "size": 1, "annotations": {"a": 1}}),
            json!([]),
            Value::Null,
        ];
        for case in cases {
            assert!(
                serde_json::from_value::<Descriptor>(case.clone()).is_err(),
                "{case}"
            );
        }
    }

    #[test]
    fn ignores_unknown_properties() {
        let value = json!({"mediaType": "a/b", "digest": DIGEST, "size": 1, "future": [1, 2]});
        let d: Descriptor = serde_json::from_value(value).unwrap();
        assert_eq!(d.size, 1);
    }
}
