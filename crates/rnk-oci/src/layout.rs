//! The index files of the two image outputs written to disk: a docker-save
//! tarball's `manifest.json` and an OCI image layout's `oci-layout` and
//! `index.json`. Types and names only; `rnk-builder` writes the files.
//!
//! The tarball follows go-containerregistry's `tarball.Write`, which kaniko's
//! `--tar-path` uses (`kaniko@44aa7ddc0
//! vendor/github.com/google/go-containerregistry/pkg/v1/tarball/write.go`):
//! the config is stored under its digest (`sha256:<hex>`), each layer as
//! `<hex>.tar.gz`. The layout follows OCI image-spec v1.1.1
//! [image-layout.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-layout.md).
//! Kept here, with `ConfigEdit`, as pure types (Q-007, answered in
//! `docs/open-questions.md`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::descriptor::Descriptor;
use crate::digest::Digest;
use crate::media_type::Compression;
use crate::serde_util::null_as_default;

/// One image of a docker-save `manifest.json`, which is a JSON array of
/// these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DockerTarImage {
    /// Path of the config in the tar.
    pub config: String,
    /// Tags to load the image as, `registry/repository:tag`. `null` (as
    /// `docker save` writes for an untagged image) reads as none.
    #[serde(deserialize_with = "null_as_default")]
    pub repo_tags: Vec<String>,
    /// Paths of the layers in the tar, base first. `null` reads as none.
    #[serde(deserialize_with = "null_as_default")]
    pub layers: Vec<String>,
    /// Foreign layers that are not in the tar, by `diff_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_sources: Option<BTreeMap<Digest, Descriptor>>,
}

impl DockerTarImage {
    /// The config's path in the tar, as go-containerregistry names it: its
    /// digest.
    pub fn config_path(config: &Digest) -> String {
        config.to_string()
    }

    /// A layer's path in the tar, as go-containerregistry names it: the
    /// blob digest's hex and an extension for its compression.
    pub fn layer_path(blob: &Digest, compression: Compression) -> String {
        let extension = match compression {
            Compression::Gzip => ".tar.gz",
            Compression::Zstd => ".tar.zst",
            Compression::Uncompressed => ".tar",
        };
        format!("{}{extension}", blob.encoded())
    }
}

/// The content of a layout's `oci-layout` file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciLayout {
    /// Always [`OciLayout::VERSION`].
    pub image_layout_version: String,
}

impl OciLayout {
    /// The `oci-layout` file's name.
    pub const FILE: &'static str = "oci-layout";
    /// The layout's index file; its content is an
    /// [`ImageIndex`](crate::ImageIndex).
    pub const INDEX_FILE: &'static str = "index.json";
    /// The only layout version.
    pub const VERSION: &'static str = "1.0.0";
    /// The annotation that names an image in `index.json`.
    pub const REF_NAME_ANNOTATION: &'static str = "org.opencontainers.image.ref.name";

    /// The `oci-layout` content of this version.
    pub fn new() -> Self {
        Self {
            image_layout_version: Self::VERSION.to_owned(),
        }
    }

    /// A blob's path in the layout: `blobs/<algorithm>/<encoded>`.
    pub fn blob_path(digest: &Digest) -> String {
        format!("blobs/{}/{}", digest.algorithm(), digest.encoded())
    }
}

impl Default for OciLayout {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::canonical::to_canonical_json;
    use crate::descriptor::Descriptor;
    use crate::manifest::ImageIndex;
    use crate::media_type::{ImageFormat, MediaType};

    #[test]
    fn docker_tar_manifest_as_go_containerregistry() {
        let config = Digest::from_sha256_bytes(&[1; 32]);
        let layer = Digest::from_sha256_bytes(&[2; 32]);
        let image = DockerTarImage {
            config: DockerTarImage::config_path(&config),
            repo_tags: vec!["registry.local/app:latest".to_owned()],
            layers: vec![DockerTarImage::layer_path(&layer, Compression::Gzip)],
            layer_sources: None,
        };
        assert_eq!(image.config, config.as_str());
        assert_eq!(image.layers[0], format!("{}.tar.gz", layer.encoded()));
        assert_eq!(
            serde_json::to_value([&image]).unwrap(),
            json!([{
                "Config": config.as_str(),
                "RepoTags": ["registry.local/app:latest"],
                "Layers": [format!("{}.tar.gz", layer.encoded())]
            }])
        );
        let back: Vec<DockerTarImage> =
            serde_json::from_slice(&to_canonical_json(&[&image]).unwrap()).unwrap();
        assert_eq!(back, vec![image]);
    }

    #[test]
    fn null_tags_and_layers_read_as_empty() {
        let images: Vec<DockerTarImage> =
            serde_json::from_str(r#"[{"Config":"c.json","RepoTags":null,"Layers":null}]"#).unwrap();
        assert!(images[0].repo_tags.is_empty() && images[0].layers.is_empty());
    }

    #[test]
    fn foreign_layers_are_keyed_by_diff_id() {
        let diff_id = Digest::from_sha256_bytes(&[3; 32]);
        let mut d = Descriptor::new(
            MediaType::DOCKER_FOREIGN_LAYER_GZIP,
            Digest::from_sha256_bytes(&[4; 32]),
            10,
        );
        d.urls = Some(vec!["https://example.com/l".to_owned()]);
        let image = DockerTarImage {
            config: "c".to_owned(),
            repo_tags: Vec::new(),
            layers: Vec::new(),
            layer_sources: Some(BTreeMap::from([(diff_id.clone(), d)])),
        };
        let value = serde_json::to_value(&image).unwrap();
        assert!(value["LayerSources"][diff_id.as_str()]["urls"].is_array());
    }

    #[test]
    fn layout_files() {
        assert_eq!(
            to_canonical_json(&OciLayout::new()).unwrap(),
            br#"{"imageLayoutVersion":"1.0.0"}"#
        );
        let d = Digest::from_sha256_bytes(&[5; 32]);
        assert_eq!(
            OciLayout::blob_path(&d),
            format!("blobs/sha256/{}", d.encoded())
        );

        let mut manifest = Descriptor::new(MediaType::OCI_MANIFEST, d, 300);
        manifest.annotations = Some(BTreeMap::from([(
            OciLayout::REF_NAME_ANNOTATION.to_owned(),
            "latest".to_owned(),
        )]));
        let index = ImageIndex::new(ImageFormat::Oci, vec![manifest]);
        let back = ImageIndex::from_json(&to_canonical_json(&index).unwrap()).unwrap();
        assert_eq!(back, index);
    }
}
