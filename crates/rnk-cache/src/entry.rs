//! A cache entry's documents ([ADR-0113]): a one-layer OCI image manifest
//! whose annotations name the key, its version and the encoding, and a
//! config whose `created` is the push time. Written for a push, checked on
//! a lookup; the registry cache and the layout cache share them.
//!
//! [ADR-0113]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md

use std::collections::BTreeMap;

use rnk_oci::{
    Compression, Descriptor, Digest, ImageConfig, ImageFormat, ImageManifest, MediaType, RootFs,
    Timestamp,
};
use sha2::{Digest as _, Sha256};

use crate::error::{Refusal, excerpt};
use crate::key::EntryId;

/// The annotation naming the entry's key: its SHA-256 in lower-case hex.
pub const KEY_ANNOTATION: &str = "io.github.tran4774.ranikust.cache.key";
/// The annotation naming the key's version, in decimal.
pub const VERSION_ANNOTATION: &str = "io.github.tran4774.ranikust.cache.version";
/// The annotation naming the layer's encoding, in [`Encoding`]'s
/// `Display` form.
///
/// [`Encoding`]: crate::Encoding
pub const ENCODING_ANNOTATION: &str = "io.github.tran4774.ranikust.cache.encoding";

/// The OCI layer media type of the entry's encoding.
pub(crate) fn layer_type(entry: &EntryId) -> MediaType {
    match entry.encoding().compression {
        Compression::Uncompressed => MediaType::OCI_LAYER,
        Compression::Gzip => MediaType::OCI_LAYER_GZIP,
        Compression::Zstd => MediaType::OCI_LAYER_ZSTD,
    }
}

/// The annotations `entry`'s manifest carries.
fn annotations(entry: &EntryId) -> [(&'static str, String); 3] {
    let key = entry.key();
    [
        (
            KEY_ANNOTATION,
            Digest::from_sha256_bytes(key.hash()).encoded().to_owned(),
        ),
        (VERSION_ANNOTATION, key.version().to_string()),
        (ENCODING_ANNOTATION, entry.encoding().to_string()),
    ]
}

/// A document and its descriptor.
pub(crate) struct Document {
    pub(crate) descriptor: Descriptor,
    pub(crate) bytes: Vec<u8>,
}

fn document(media_type: MediaType, bytes: Vec<u8>) -> Document {
    let hash: [u8; 32] = Sha256::digest(&bytes).into();
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Document {
        descriptor: Descriptor::new(media_type, Digest::from_sha256_bytes(&hash), size),
        bytes,
    }
}

/// The entry's config: the one layer's diff ID and `created`, the push
/// time, which `--cache-ttl` counts from. No platform, as kaniko's entries
/// have none: the key holds it.
pub(crate) fn config(diff_id: &Digest, created: Timestamp) -> Result<Document, rnk_oci::Error> {
    let config = ImageConfig {
        created: Some(created.to_string()),
        rootfs: RootFs {
            kind: "layers".to_owned(),
            diff_ids: vec![diff_id.clone()],
        },
        ..ImageConfig::default()
    };
    let bytes = rnk_oci::to_canonical_json(&config)?;
    Ok(document(MediaType::OCI_CONFIG, bytes))
}

/// The entry's manifest over `config` and `layer`, which has the entry
/// encoding's OCI media type.
pub(crate) fn manifest(
    entry: &EntryId,
    config: Descriptor,
    layer: Descriptor,
) -> Result<Document, rnk_oci::Error> {
    let mut manifest = ImageManifest::new(ImageFormat::Oci, config, vec![layer]);
    let annotations = annotations(entry).map(|(name, value)| (name.to_owned(), value));
    manifest.annotations = Some(BTreeMap::from(annotations));
    let bytes = rnk_oci::to_canonical_json(&manifest)?;
    Ok(document(MediaType::OCI_MANIFEST, bytes))
}

/// What a checked manifest points at.
pub(crate) struct Form {
    pub(crate) config: Descriptor,
    pub(crate) layer: Descriptor,
}

/// Checks that `bytes`, found under `entry`'s tag as `media_type`, is the
/// manifest this build would push for `entry`: an OCI image manifest of
/// one layer of the entry's encoding, an OCI config, and annotations
/// naming this key, version and encoding.
pub(crate) fn check_manifest(
    entry: &EntryId,
    media_type: &MediaType,
    bytes: &[u8],
) -> Result<Form, Refusal> {
    if *media_type != MediaType::OCI_MANIFEST {
        return Err(Refusal::NotAnImageManifest {
            media_type: excerpt(media_type.as_str()),
        });
    }
    let manifest = ImageManifest::from_json(bytes).map_err(|e| Refusal::Malformed {
        document: "manifest",
        reason: excerpt(&e.to_string()),
    })?;
    if manifest.media_type.as_ref() != Some(&MediaType::OCI_MANIFEST) {
        return Err(Refusal::NotAnImageManifest {
            media_type: excerpt(manifest.media_type.as_ref().map_or("", MediaType::as_str)),
        });
    }
    if manifest.artifact_type.is_some() || manifest.subject.is_some() {
        return Err(Refusal::NotAnImage);
    }
    let ImageManifest {
        config,
        mut layers,
        annotations: found,
        ..
    } = manifest;
    let found = found.unwrap_or_default();
    for (name, want) in annotations(entry) {
        if found.get(name) != Some(&want) {
            return Err(Refusal::Annotation { name });
        }
    }
    if config.media_type != MediaType::OCI_CONFIG {
        return Err(Refusal::ConfigType {
            media_type: excerpt(config.media_type.as_str()),
        });
    }
    let layer = match layers.pop() {
        Some(layer) if layers.is_empty() => layer,
        Some(_) => {
            return Err(Refusal::Layers {
                count: layers.len() + 1,
            });
        }
        None => return Err(Refusal::Layers { count: 0 }),
    };
    if layer.media_type != layer_type(entry) {
        return Err(Refusal::LayerType {
            media_type: excerpt(layer.media_type.as_str()),
        });
    }
    // The entry's `urls`, `data` and annotations never reach the caller's
    // image or pull.
    let layer = Descriptor::new(layer.media_type, layer.digest, layer.size);
    Ok(Form { config, layer })
}

/// The one diff ID and the `created` time of an entry's config.
pub(crate) fn check_config(bytes: &[u8]) -> Result<(Digest, Timestamp), Refusal> {
    let config = ImageConfig::from_json(bytes).map_err(|e| Refusal::Malformed {
        document: "config",
        reason: excerpt(&e.to_string()),
    })?;
    let ImageConfig {
        created, rootfs, ..
    } = config;
    let RootFs { kind, mut diff_ids } = rootfs;
    let diff_id = match diff_ids.pop() {
        Some(diff_id) if diff_ids.is_empty() && kind == "layers" => diff_id,
        _ => return Err(Refusal::DiffIds),
    };
    let created = created.as_deref().ok_or_else(|| Refusal::Created {
        reason: "missing".to_owned(),
    })?;
    // rnk-oci's message already quotes the input as a short excerpt.
    let created = Timestamp::parse_rfc3339(created).map_err(|e| Refusal::Created {
        reason: e.to_string(),
    })?;
    Ok((diff_id, created))
}
