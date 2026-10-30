//! Media types of the OCI and Docker v2 schema 2 image formats.
//!
//! Sources: OCI image-spec v1.1.1
//! [media-types.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/media-types.md)
//! (including its compatibility matrix with Docker) and
//! [Docker manifest v2 schema 2](https://github.com/distribution/distribution/blob/v2.8.3/docs/spec/manifest-v2-2.md).

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::Error;

/// A media type string, such as `application/vnd.oci.image.manifest.v1+json`.
///
/// Any string is accepted, since registries and artifacts use types this
/// crate does not know; the associated constants name the ones it does.
/// Two media types are equal when their strings are.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaType(Cow<'static, str>);

impl MediaType {
    /// OCI image manifest.
    pub const OCI_MANIFEST: Self = Self::from_static("application/vnd.oci.image.manifest.v1+json");
    /// OCI image index.
    pub const OCI_INDEX: Self = Self::from_static("application/vnd.oci.image.index.v1+json");
    /// OCI image config.
    pub const OCI_CONFIG: Self = Self::from_static("application/vnd.oci.image.config.v1+json");
    /// OCI layer, uncompressed tar.
    pub const OCI_LAYER: Self = Self::from_static("application/vnd.oci.image.layer.v1.tar");
    /// OCI layer, gzip-compressed tar.
    pub const OCI_LAYER_GZIP: Self =
        Self::from_static("application/vnd.oci.image.layer.v1.tar+gzip");
    /// OCI layer, zstd-compressed tar.
    pub const OCI_LAYER_ZSTD: Self =
        Self::from_static("application/vnd.oci.image.layer.v1.tar+zstd");
    /// OCI non-distributable (foreign) layer, uncompressed tar. Deprecated by
    /// the spec; still found in Windows base images.
    pub const OCI_FOREIGN_LAYER: Self =
        Self::from_static("application/vnd.oci.image.layer.nondistributable.v1.tar");
    /// OCI non-distributable (foreign) layer, gzip-compressed tar.
    pub const OCI_FOREIGN_LAYER_GZIP: Self =
        Self::from_static("application/vnd.oci.image.layer.nondistributable.v1.tar+gzip");
    /// OCI non-distributable (foreign) layer, zstd-compressed tar.
    pub const OCI_FOREIGN_LAYER_ZSTD: Self =
        Self::from_static("application/vnd.oci.image.layer.nondistributable.v1.tar+zstd");
    /// The OCI empty descriptor's content, `{}`.
    pub const OCI_EMPTY: Self = Self::from_static("application/vnd.oci.empty.v1+json");

    /// Docker image manifest, v2 schema 2.
    pub const DOCKER_MANIFEST: Self =
        Self::from_static("application/vnd.docker.distribution.manifest.v2+json");
    /// Docker manifest list (the Docker index).
    pub const DOCKER_MANIFEST_LIST: Self =
        Self::from_static("application/vnd.docker.distribution.manifest.list.v2+json");
    /// Docker image config.
    pub const DOCKER_CONFIG: Self =
        Self::from_static("application/vnd.docker.container.image.v1+json");
    /// Docker layer, uncompressed tar. Not in the schema 2 spec, but
    /// written by Docker, containerd and go-containerregistry.
    pub const DOCKER_LAYER: Self =
        Self::from_static("application/vnd.docker.image.rootfs.diff.tar");
    /// Docker layer, gzip-compressed tar.
    pub const DOCKER_LAYER_GZIP: Self =
        Self::from_static("application/vnd.docker.image.rootfs.diff.tar.gzip");
    /// Docker foreign layer, gzip-compressed tar (fetched from its `urls`).
    pub const DOCKER_FOREIGN_LAYER_GZIP: Self =
        Self::from_static("application/vnd.docker.image.rootfs.foreign.diff.tar.gzip");
    /// Docker v2 schema 1 manifest: recognized only to be refused.
    pub const DOCKER_MANIFEST_SCHEMA1: Self =
        Self::from_static("application/vnd.docker.distribution.manifest.v1+json");
    /// Signed Docker v2 schema 1 manifest: recognized only to be refused.
    pub const DOCKER_MANIFEST_SCHEMA1_SIGNED: Self =
        Self::from_static("application/vnd.docker.distribution.manifest.v1+prettyjws");

    /// Pairs of equivalent media types, OCI first, Docker second: the OCI
    /// image-spec's compatibility matrix.
    const EQUIVALENTS: [(Self, Self); 6] = [
        (Self::OCI_MANIFEST, Self::DOCKER_MANIFEST),
        (Self::OCI_INDEX, Self::DOCKER_MANIFEST_LIST),
        (Self::OCI_CONFIG, Self::DOCKER_CONFIG),
        (Self::OCI_LAYER, Self::DOCKER_LAYER),
        (Self::OCI_LAYER_GZIP, Self::DOCKER_LAYER_GZIP),
        (
            Self::OCI_FOREIGN_LAYER_GZIP,
            Self::DOCKER_FOREIGN_LAYER_GZIP,
        ),
    ];

    /// Every known layer media type and its compression.
    const LAYERS: [(Self, Compression); 9] = [
        (Self::OCI_LAYER, Compression::Uncompressed),
        (Self::OCI_LAYER_GZIP, Compression::Gzip),
        (Self::OCI_LAYER_ZSTD, Compression::Zstd),
        (Self::OCI_FOREIGN_LAYER, Compression::Uncompressed),
        (Self::OCI_FOREIGN_LAYER_GZIP, Compression::Gzip),
        (Self::OCI_FOREIGN_LAYER_ZSTD, Compression::Zstd),
        (Self::DOCKER_LAYER, Compression::Uncompressed),
        (Self::DOCKER_LAYER_GZIP, Compression::Gzip),
        (Self::DOCKER_FOREIGN_LAYER_GZIP, Compression::Gzip),
    ];

    /// A media type from a static string, usable in constants.
    pub const fn from_static(media_type: &'static str) -> Self {
        Self(Cow::Borrowed(media_type))
    }

    /// A media type from any string.
    pub fn new(media_type: impl Into<String>) -> Self {
        Self(Cow::Owned(media_type.into()))
    }

    /// The media type string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The image format whose vendor tree the type belongs to
    /// (`application/vnd.oci.` or `application/vnd.docker.`), if any.
    ///
    /// Like kaniko's mixing check (`kaniko@44aa7ddc0
    /// pkg/image/transform.go:131-139`), this looks at the vendor prefix
    /// only, so it also classifies types this crate has no constant for.
    pub fn format(&self) -> Option<ImageFormat> {
        if self.0.starts_with("application/vnd.oci.") {
            Some(ImageFormat::Oci)
        } else if self.0.starts_with("application/vnd.docker.") {
            Some(ImageFormat::Docker)
        } else {
            None
        }
    }

    /// Whether this is an image manifest: OCI, or Docker v2 schema 2.
    pub fn is_manifest(&self) -> bool {
        *self == Self::OCI_MANIFEST || *self == Self::DOCKER_MANIFEST
    }

    /// Whether this is an image index: OCI, or a Docker manifest list.
    pub fn is_index(&self) -> bool {
        *self == Self::OCI_INDEX || *self == Self::DOCKER_MANIFEST_LIST
    }

    /// Whether this is an image config: OCI or Docker.
    pub fn is_config(&self) -> bool {
        *self == Self::OCI_CONFIG || *self == Self::DOCKER_CONFIG
    }

    /// The compression of a known layer media type, or `None` for any other
    /// type.
    pub fn layer_compression(&self) -> Option<Compression> {
        Self::LAYERS
            .iter()
            .find(|(media_type, _)| media_type == self)
            .map(|&(_, compression)| compression)
    }

    /// Whether this is a known layer media type, of either format.
    pub fn is_layer(&self) -> bool {
        self.layer_compression().is_some()
    }

    /// Whether this is a foreign (non-distributable) layer, which registries
    /// may not hold and which is fetched from the descriptor's `urls`.
    pub fn is_foreign_layer(&self) -> bool {
        *self == Self::OCI_FOREIGN_LAYER
            || *self == Self::OCI_FOREIGN_LAYER_GZIP
            || *self == Self::OCI_FOREIGN_LAYER_ZSTD
            || *self == Self::DOCKER_FOREIGN_LAYER_GZIP
    }

    /// The equivalent media type in `format`.
    ///
    /// A type already in `format`, or in neither vendor tree, is returned
    /// unchanged. A type of the other format without an equivalent is an
    /// error: zstd layers and Docker v2 schema 1, for example. kaniko maps
    /// OCI zstd layers to Docker gzip and the Docker foreign gzip layer to
    /// the uncompressed OCI one (`kaniko@44aa7ddc0
    /// pkg/executor/build.go:1237-1268`); both would mislabel the blob, so
    /// this follows the OCI compatibility matrix instead.
    pub fn to_format(&self, format: ImageFormat) -> Result<Self, Error> {
        match self.format() {
            None => return Ok(self.clone()),
            Some(own) if own == format => return Ok(self.clone()),
            Some(_) => {}
        }
        Self::EQUIVALENTS
            .iter()
            .find_map(|(oci, docker)| match format {
                ImageFormat::Oci => (docker == self).then(|| oci.clone()),
                ImageFormat::Docker => (oci == self).then(|| docker.clone()),
            })
            .ok_or_else(|| Error::NoEquivalentMediaType {
                media_type: self.clone(),
                format,
            })
    }
}

impl fmt::Debug for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MediaType").field(&self.as_str()).finish()
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for MediaType {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for MediaType {
    fn from(media_type: &str) -> Self {
        Self::new(media_type)
    }
}

impl From<String> for MediaType {
    fn from(media_type: String) -> Self {
        Self::new(media_type)
    }
}

impl Serialize for MediaType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MediaType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// The image format ranikust writes: OCI, or Docker v2 schema 2.
///
/// There is no `Default`: the caller chooses. A build writes OCI whatever
/// its base, as BuildKit (Q-006, `ADR-0032`; `rnk_builder::image_format`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    /// The OCI image format.
    Oci,
    /// The Docker image manifest v2, schema 2.
    Docker,
}

impl ImageFormat {
    /// The media type of an image manifest in this format.
    pub const fn manifest_media_type(self) -> MediaType {
        match self {
            Self::Oci => MediaType::OCI_MANIFEST,
            Self::Docker => MediaType::DOCKER_MANIFEST,
        }
    }

    /// The media type of an image index in this format.
    pub const fn index_media_type(self) -> MediaType {
        match self {
            Self::Oci => MediaType::OCI_INDEX,
            Self::Docker => MediaType::DOCKER_MANIFEST_LIST,
        }
    }

    /// The media type of an image config in this format.
    pub const fn config_media_type(self) -> MediaType {
        match self {
            Self::Oci => MediaType::OCI_CONFIG,
            Self::Docker => MediaType::DOCKER_CONFIG,
        }
    }

    /// The media type of a (distributable) layer with `compression`.
    ///
    /// Docker has no zstd layer type: that combination is an error, as in
    /// kaniko (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:177-188`).
    pub fn layer_media_type(self, compression: Compression) -> Result<MediaType, Error> {
        match (self, compression) {
            (Self::Oci, Compression::Uncompressed) => Ok(MediaType::OCI_LAYER),
            (Self::Oci, Compression::Gzip) => Ok(MediaType::OCI_LAYER_GZIP),
            (Self::Oci, Compression::Zstd) => Ok(MediaType::OCI_LAYER_ZSTD),
            (Self::Docker, Compression::Uncompressed) => Ok(MediaType::DOCKER_LAYER),
            (Self::Docker, Compression::Gzip) => Ok(MediaType::DOCKER_LAYER_GZIP),
            (Self::Docker, Compression::Zstd) => Err(Error::UnsupportedCompression {
                format: self,
                compression,
            }),
        }
    }
}

impl fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Oci => "OCI",
            Self::Docker => "Docker",
        })
    }
}

/// How a layer blob is compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Compression {
    /// A plain tar.
    Uncompressed,
    /// gzip.
    Gzip,
    /// zstd: OCI only.
    Zstd,
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Uncompressed => "uncompressed",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_KNOWN: [MediaType; 18] = [
        MediaType::OCI_MANIFEST,
        MediaType::OCI_INDEX,
        MediaType::OCI_CONFIG,
        MediaType::OCI_LAYER,
        MediaType::OCI_LAYER_GZIP,
        MediaType::OCI_LAYER_ZSTD,
        MediaType::OCI_FOREIGN_LAYER,
        MediaType::OCI_FOREIGN_LAYER_GZIP,
        MediaType::OCI_FOREIGN_LAYER_ZSTD,
        MediaType::OCI_EMPTY,
        MediaType::DOCKER_MANIFEST,
        MediaType::DOCKER_MANIFEST_LIST,
        MediaType::DOCKER_CONFIG,
        MediaType::DOCKER_LAYER,
        MediaType::DOCKER_LAYER_GZIP,
        MediaType::DOCKER_FOREIGN_LAYER_GZIP,
        MediaType::DOCKER_MANIFEST_SCHEMA1,
        MediaType::DOCKER_MANIFEST_SCHEMA1_SIGNED,
    ];

    #[test]
    fn layer_media_type_per_format_and_compression() {
        use Compression::{Gzip, Uncompressed, Zstd};
        use ImageFormat::{Docker, Oci};
        let table = [
            (
                Oci,
                Uncompressed,
                Some("application/vnd.oci.image.layer.v1.tar"),
            ),
            (
                Oci,
                Gzip,
                Some("application/vnd.oci.image.layer.v1.tar+gzip"),
            ),
            (
                Oci,
                Zstd,
                Some("application/vnd.oci.image.layer.v1.tar+zstd"),
            ),
            (
                Docker,
                Uncompressed,
                Some("application/vnd.docker.image.rootfs.diff.tar"),
            ),
            (
                Docker,
                Gzip,
                Some("application/vnd.docker.image.rootfs.diff.tar.gzip"),
            ),
            (Docker, Zstd, None),
        ];
        for (format, compression, expected) in table {
            let got = format.layer_media_type(compression);
            match expected {
                Some(expected) => {
                    let got = got.unwrap();
                    assert_eq!(got.as_str(), expected);
                    assert_eq!(got.format(), Some(format));
                    assert_eq!(got.layer_compression(), Some(compression));
                    assert!(got.is_layer() && !got.is_foreign_layer());
                }
                None => assert!(matches!(
                    got,
                    Err(Error::UnsupportedCompression { format: f, compression: c })
                        if f == format && c == compression
                )),
            }
        }
    }

    #[test]
    fn document_media_types_per_format() {
        for format in [ImageFormat::Oci, ImageFormat::Docker] {
            assert!(format.manifest_media_type().is_manifest());
            assert!(format.index_media_type().is_index());
            assert!(format.config_media_type().is_config());
            for mt in [
                format.manifest_media_type(),
                format.index_media_type(),
                format.config_media_type(),
            ] {
                assert_eq!(mt.format(), Some(format), "{mt}");
            }
        }
    }

    #[test]
    fn classifies_by_vendor_prefix() {
        for mt in ALL_KNOWN {
            let expected = if mt.as_str().contains(".oci.") {
                ImageFormat::Oci
            } else {
                ImageFormat::Docker
            };
            assert_eq!(mt.format(), Some(expected), "{mt}");
        }
        assert_eq!(
            MediaType::from("application/vnd.oci.something.new+json").format(),
            Some(ImageFormat::Oci)
        );
        assert_eq!(MediaType::from("application/json").format(), None);
        assert_eq!(MediaType::from("").format(), None);
        assert!(!MediaType::from("application/vnd.oci.image.layer.v1.tar+lz4").is_layer());
    }

    #[test]
    fn foreign_layers() {
        let all = ALL_KNOWN;
        let foreign: Vec<_> = all.iter().filter(|m| m.is_foreign_layer()).collect();
        assert_eq!(
            foreign,
            [
                &MediaType::OCI_FOREIGN_LAYER,
                &MediaType::OCI_FOREIGN_LAYER_GZIP,
                &MediaType::OCI_FOREIGN_LAYER_ZSTD,
                &MediaType::DOCKER_FOREIGN_LAYER_GZIP,
            ]
        );
        assert!(foreign.iter().all(|m| m.is_layer()));
    }

    #[test]
    fn converts_along_the_compatibility_matrix() {
        for (oci, docker) in MediaType::EQUIVALENTS {
            assert_eq!(oci.to_format(ImageFormat::Docker).unwrap(), docker);
            assert_eq!(docker.to_format(ImageFormat::Oci).unwrap(), oci);
            assert_eq!(oci.to_format(ImageFormat::Oci).unwrap(), oci);
            assert_eq!(docker.to_format(ImageFormat::Docker).unwrap(), docker);
        }
        for mt in [
            MediaType::OCI_LAYER_ZSTD,
            MediaType::OCI_FOREIGN_LAYER,
            MediaType::OCI_FOREIGN_LAYER_ZSTD,
            MediaType::OCI_EMPTY,
        ] {
            assert!(mt.to_format(ImageFormat::Docker).is_err(), "{mt}");
        }
        for mt in [
            MediaType::DOCKER_MANIFEST_SCHEMA1,
            MediaType::DOCKER_MANIFEST_SCHEMA1_SIGNED,
        ] {
            assert!(mt.to_format(ImageFormat::Oci).is_err(), "{mt}");
        }
        let other = MediaType::from("application/vnd.example.thing+json");
        assert_eq!(other.to_format(ImageFormat::Docker).unwrap(), other);
    }

    #[test]
    fn constants_equal_owned_strings() {
        for mt in ALL_KNOWN {
            assert_eq!(MediaType::new(mt.as_str()), mt);
        }
    }

    #[test]
    fn serde_as_plain_string() {
        let json = serde_json::to_string(&MediaType::OCI_MANIFEST).unwrap();
        assert_eq!(json, "\"application/vnd.oci.image.manifest.v1+json\"");
        let back: MediaType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, MediaType::OCI_MANIFEST);
        assert!(serde_json::from_str::<MediaType>("1").is_err());
    }
}
