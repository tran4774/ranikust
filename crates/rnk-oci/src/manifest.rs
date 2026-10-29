//! Image manifests and indexes, in the OCI and Docker v2 schema 2 formats.
//!
//! The two formats share one shape, so one type covers both; the media types
//! inside say which format a document is in. Sources: OCI image-spec v1.1.1
//! [manifest.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md),
//! [image-index.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-index.md),
//! [Docker manifest v2 schema 2](https://github.com/distribution/distribution/blob/v2.8.3/docs/spec/manifest-v2-2.md).
//!
//! A pulled manifest is parsed to be read, never re-serialized to be pushed:
//! its digest covers the exact bytes the registry sent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::descriptor::Descriptor;
use crate::error::Error;
use crate::media_type::{ImageFormat, MediaType};
use crate::serde_util::null_as_default;

/// The only `schemaVersion` of OCI and Docker v2 schema 2 documents.
const SCHEMA_VERSION: u32 = 2;

/// An image manifest: one config and an ordered list of layers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageManifest {
    /// Always 2.
    pub schema_version: u32,
    /// The manifest's own media type. Required by Docker, optional in OCI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<MediaType>,
    /// The artifact type, when the manifest describes an OCI artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<MediaType>,
    /// The image config.
    pub config: Descriptor,
    /// The layers, base first. `null` reads as no layers.
    #[serde(deserialize_with = "null_as_default")]
    pub layers: Vec<Descriptor>,
    /// The manifest this one refers to (OCI referrers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Descriptor>,
    /// Arbitrary metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

impl ImageManifest {
    /// A schema 2 manifest in `format`, with its media type set.
    pub fn new(format: ImageFormat, config: Descriptor, layers: Vec<Descriptor>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            media_type: Some(format.manifest_media_type()),
            artifact_type: None,
            config,
            layers,
            subject: None,
            annotations: None,
        }
    }

    /// Parses a manifest and checks its schema version and media type.
    ///
    /// Unknown properties are ignored, as the OCI spec asks. Mixed media
    /// types are accepted here, so a base image can still be read; see
    /// [`ImageManifest::format`].
    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        check_schema_version(manifest.schema_version)?;
        check_media_type(
            manifest.media_type.as_ref(),
            MediaType::is_manifest,
            "an image manifest",
        )?;
        Ok(manifest)
    }

    /// The manifest's format, from the media types of the manifest, its
    /// config and its layers (as kaniko checks, `kaniko@44aa7ddc0
    /// pkg/image/transform.go:131-139`). Docker if any is a Docker type,
    /// otherwise OCI; an error if both formats appear.
    pub fn format(&self) -> Result<ImageFormat, Error> {
        let media_types = self
            .media_type
            .iter()
            .chain([&self.config.media_type])
            .chain(self.layers.iter().map(|layer| &layer.media_type));
        format_of(media_types, "manifest")
    }
}

/// An image index (a Docker manifest list): manifests for several platforms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageIndex {
    /// Always 2.
    pub schema_version: u32,
    /// The index's own media type. Required by Docker, optional in OCI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<MediaType>,
    /// The artifact type, when the index describes an OCI artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<MediaType>,
    /// The manifests, usually one per platform. `null` reads as none.
    #[serde(deserialize_with = "null_as_default")]
    pub manifests: Vec<Descriptor>,
    /// The manifest this one refers to (OCI referrers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Descriptor>,
    /// Arbitrary metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

impl ImageIndex {
    /// A schema 2 index in `format`, with its media type set.
    pub fn new(format: ImageFormat, manifests: Vec<Descriptor>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            media_type: Some(format.index_media_type()),
            artifact_type: None,
            manifests,
            subject: None,
            annotations: None,
        }
    }

    /// Parses an index and checks its schema version and media type.
    ///
    /// Unknown properties are ignored, as the OCI spec asks.
    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        let index: Self = serde_json::from_slice(bytes)?;
        check_schema_version(index.schema_version)?;
        check_media_type(
            index.media_type.as_ref(),
            MediaType::is_index,
            "an image index",
        )?;
        Ok(index)
    }

    /// The index's format, from the media types of the index and of the
    /// manifests it lists: Docker if any is a Docker type, otherwise OCI; an
    /// error if both formats appear.
    pub fn format(&self) -> Result<ImageFormat, Error> {
        let media_types = self
            .media_type
            .iter()
            .chain(self.manifests.iter().map(|manifest| &manifest.media_type));
        format_of(media_types, "index")
    }
}

fn check_schema_version(version: u32) -> Result<(), Error> {
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(Error::UnsupportedSchemaVersion(version))
    }
}

fn check_media_type(
    media_type: Option<&MediaType>,
    expected: fn(&MediaType) -> bool,
    what: &'static str,
) -> Result<(), Error> {
    match media_type {
        Some(found) if !expected(found) => Err(Error::UnexpectedMediaType {
            expected: what,
            found: found.clone(),
        }),
        Some(_) | None => Ok(()),
    }
}

fn format_of<'a>(
    media_types: impl Iterator<Item = &'a MediaType>,
    document: &'static str,
) -> Result<ImageFormat, Error> {
    let (mut oci, mut docker) = (false, false);
    for media_type in media_types {
        match media_type.format() {
            Some(ImageFormat::Oci) => oci = true,
            Some(ImageFormat::Docker) => docker = true,
            None => {}
        }
    }
    match (oci, docker) {
        (true, true) => Err(Error::MixedFormats { document }),
        (false, true) => Ok(ImageFormat::Docker),
        (true, false) | (false, false) => Ok(ImageFormat::Oci),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::digest::Digest;

    const DOCKER_MANIFEST: &str = include_str!("../testdata/docker-manifest.json");
    const DOCKER_MANIFEST_LIST: &str = include_str!("../testdata/docker-manifest-list.json");
    const OCI_MANIFEST: &str = include_str!("../testdata/oci-manifest.json");
    const OCI_ARTIFACT_MANIFEST: &str = include_str!("../testdata/oci-artifact-manifest.json");
    const OCI_INDEX: &str = include_str!("../testdata/oci-index.json");

    fn digest(n: u8) -> Digest {
        Digest::from_sha256_bytes(&[n; 32])
    }

    /// Serializing the parsed document gives back the same JSON value: no
    /// property is lost or changed.
    fn assert_value_round_trip<T: Serialize>(parsed: &T, text: &str) {
        let original: Value = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), original);
    }

    #[test]
    fn docker_manifest_fixture() {
        let m = ImageManifest::from_json(DOCKER_MANIFEST.as_bytes()).unwrap();
        assert_eq!(m.media_type, Some(MediaType::DOCKER_MANIFEST));
        assert_eq!(m.config.media_type, MediaType::DOCKER_CONFIG);
        assert_eq!(m.config.size, 7023);
        assert_eq!(m.layers.len(), 3);
        assert!(m.layers[2].media_type.is_foreign_layer());
        assert_eq!(m.layers[2].urls.as_ref().map(Vec::len), Some(1));
        assert_eq!(m.format().unwrap(), ImageFormat::Docker);
        assert_value_round_trip(&m, DOCKER_MANIFEST);
    }

    #[test]
    fn oci_manifest_fixtures() {
        let m = ImageManifest::from_json(OCI_MANIFEST.as_bytes()).unwrap();
        assert_eq!(m.format().unwrap(), ImageFormat::Oci);
        assert_eq!(m.layers[1].media_type, MediaType::OCI_LAYER_ZSTD);
        assert_eq!(m.annotations.as_ref().map(BTreeMap::len), Some(2));
        assert_value_round_trip(&m, OCI_MANIFEST);

        let a = ImageManifest::from_json(OCI_ARTIFACT_MANIFEST.as_bytes()).unwrap();
        assert_eq!(a.format().unwrap(), ImageFormat::Oci);
        assert_eq!(a.config.media_type, MediaType::OCI_EMPTY);
        assert_eq!(a.config.data.as_deref(), Some("e30="));
        assert!(a.artifact_type.is_some() && a.subject.is_some());
        assert_value_round_trip(&a, OCI_ARTIFACT_MANIFEST);
    }

    #[test]
    fn index_fixtures() {
        let list = ImageIndex::from_json(DOCKER_MANIFEST_LIST.as_bytes()).unwrap();
        assert_eq!(list.format().unwrap(), ImageFormat::Docker);
        assert_eq!(list.manifests.len(), 4);
        let windows = list.manifests[3].platform.as_ref().unwrap();
        assert_eq!(windows.os, "windows");
        assert_eq!(windows.os_version.as_deref(), Some("10.0.20348.2340"));
        assert_value_round_trip(&list, DOCKER_MANIFEST_LIST);

        let index = ImageIndex::from_json(OCI_INDEX.as_bytes()).unwrap();
        assert_eq!(index.format().unwrap(), ImageFormat::Oci);
        assert_eq!(
            index.manifests[1]
                .platform
                .as_ref()
                .unwrap()
                .variant
                .as_deref(),
            Some("v8")
        );
        assert_value_round_trip(&index, OCI_INDEX);
    }

    #[test]
    fn new_documents_carry_their_format() {
        for format in [ImageFormat::Oci, ImageFormat::Docker] {
            let config = Descriptor::new(format.config_media_type(), digest(1), 10);
            let layer = Descriptor::new(
                format.layer_media_type(crate::Compression::Gzip).unwrap(),
                digest(2),
                20,
            );
            let m = ImageManifest::new(format, config, vec![layer]);
            assert_eq!(m.format().unwrap(), format);
            let bytes = serde_json::to_vec(&m).unwrap();
            assert_eq!(ImageManifest::from_json(&bytes).unwrap(), m);

            let size = u64::try_from(bytes.len()).unwrap();
            let manifest = Descriptor::new(format.manifest_media_type(), digest(3), size);
            let index = ImageIndex::new(format, vec![manifest]);
            assert_eq!(index.format().unwrap(), format);
            let bytes = serde_json::to_vec(&index).unwrap();
            assert_eq!(ImageIndex::from_json(&bytes).unwrap(), index);
        }
    }

    #[test]
    fn detects_mixed_formats() {
        let mut m = ImageManifest::from_json(DOCKER_MANIFEST.as_bytes()).unwrap();
        m.layers[0].media_type = MediaType::OCI_LAYER_GZIP;
        assert!(matches!(
            m.format(),
            Err(Error::MixedFormats {
                document: "manifest"
            })
        ));

        let mut m = ImageManifest::from_json(OCI_MANIFEST.as_bytes()).unwrap();
        m.media_type = None;
        m.config.media_type = MediaType::DOCKER_CONFIG;
        assert!(m.format().is_err());

        let mut index = ImageIndex::from_json(OCI_INDEX.as_bytes()).unwrap();
        index.manifests[0].media_type = MediaType::DOCKER_MANIFEST;
        assert!(matches!(
            index.format(),
            Err(Error::MixedFormats { document: "index" })
        ));

        // Types of neither vendor tree do not count.
        let mut a = ImageManifest::from_json(OCI_ARTIFACT_MANIFEST.as_bytes()).unwrap();
        a.media_type = None;
        a.config.media_type = MediaType::from("application/vnd.example.config+json");
        assert_eq!(a.format().unwrap(), ImageFormat::Oci);
    }

    #[test]
    fn rejects_wrong_documents() {
        // An index is not a manifest, and the other way round.
        assert!(ImageManifest::from_json(OCI_INDEX.as_bytes()).is_err());
        assert!(ImageIndex::from_json(OCI_MANIFEST.as_bytes()).is_err());

        let mut value: Value = serde_json::from_str(DOCKER_MANIFEST).unwrap();
        value["schemaVersion"] = json!(1);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            ImageManifest::from_json(&bytes),
            Err(Error::UnsupportedSchemaVersion(1))
        ));

        value["schemaVersion"] = json!(2);
        value["mediaType"] = json!("application/vnd.docker.distribution.manifest.list.v2+json");
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            ImageManifest::from_json(&bytes),
            Err(Error::UnexpectedMediaType { .. })
        ));

        let mut value: Value = serde_json::from_str(OCI_INDEX).unwrap();
        value["mediaType"] = json!("application/vnd.oci.image.manifest.v1+json");
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            ImageIndex::from_json(&bytes),
            Err(Error::UnexpectedMediaType { .. })
        ));
    }

    #[test]
    fn tolerates_null_lists_and_unknown_properties() {
        let text = json!({
            "schemaVersion": 2,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest(1), "size": 2},
            "layers": null,
            "annotations": null,
            "somethingNew": {"nested": true}
        });
        let m = ImageManifest::from_json(text.to_string().as_bytes()).unwrap();
        assert!(m.layers.is_empty() && m.annotations.is_none() && m.media_type.is_none());
        let index = ImageIndex::from_json(br#"{"schemaVersion":2,"manifests":null}"#).unwrap();
        assert!(index.manifests.is_empty());
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        let cases: [&[u8]; 9] = [
            b"",
            b"{",
            b"null",
            b"[]",
            b"{\"schemaVersion\":2}",
            b"{\"schemaVersion\":-2,\"manifests\":[]}",
            b"{\"schemaVersion\":4294967298,\"manifests\":[]}",
            b"{\"schemaVersion\":2,\"manifests\":{}}",
            b"\xff\xfe",
        ];
        for case in cases {
            assert!(ImageManifest::from_json(case).is_err(), "{case:?}");
            assert!(ImageIndex::from_json(case).is_err(), "{case:?}");
        }
        // Deep nesting stops at serde_json's recursion limit.
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(ImageManifest::from_json(deep.as_bytes()).is_err());
    }
}
