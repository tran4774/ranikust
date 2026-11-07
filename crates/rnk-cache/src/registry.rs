//! The layer cache in a registry repository (`--cache-repo`, ADR-0005):
//! one tag per entry, each a one-layer OCI image ([ADR-0113]).
//!
//! [ADR-0113]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use rnk_oci::{Descriptor, Reference, Timestamp};
use rnk_registry::{BlobBody, Client, PushOutcome, RawManifest};
use tracing::field::Empty;
use tracing::{Instrument as _, debug_span};

use crate::cache::{BuiltLayer, CachedLayer, LayerCache, Lookup, Miss, Pushed, Source};
use crate::entry;
use crate::error::{Error, excerpt};
use crate::key::EntryId;

/// A registry repository holding cached layers.
///
/// It sends everything through the build's [`Client`], so the cache
/// repository gets a destination's credentials, trust and `--limit`
/// bounds: the caller treats its registry as a destination's in the
/// client's options (credentials over verified connections only on `/`,
/// ADR-0051, ADR-0077). Only the operator names it, never the Dockerfile.
#[derive(Clone, Debug)]
pub struct RegistryCache {
    client: Arc<Client>,
    repository: Reference,
    ttl: Duration,
}

impl RegistryCache {
    /// The cache in `repository`, whose entries are hits for `ttl` after
    /// their push (`--cache-ttl`).
    ///
    /// # Errors
    ///
    /// [`Error::Repository`] when `repository` has a tag or a digest: each
    /// entry has a tag of its own.
    pub fn new(client: Arc<Client>, repository: Reference, ttl: Duration) -> Result<Self, Error> {
        if repository.tag().is_some() || repository.digest().is_some() {
            return Err(Error::Repository {
                reference: excerpt(&repository.to_string()),
                reason: "a cache repository has no tag or digest",
            });
        }
        Ok(Self {
            client,
            repository,
            ttl,
        })
    }

    /// The repository with `entry`'s tag.
    fn tagged(&self, entry: &EntryId) -> Result<Reference, Error> {
        let tag = entry.tag();
        self.repository
            .clone()
            .with_tag(&tag)
            .map_err(|source| Error::Document {
                entry: format!("{}:{tag}", self.repository),
                source,
            })
    }

    async fn find(&self, entry: &EntryId) -> Lookup {
        let reference = match self.tagged(entry) {
            Ok(reference) => reference,
            Err(e) => return Lookup::Miss(Miss::Failed(e)),
        };
        let failed = |source| {
            Lookup::Miss(Miss::Failed(Error::Registry {
                entry: reference.to_string(),
                source: Box::new(source),
            }))
        };
        let refused = |refusal| {
            Lookup::Miss(Miss::Failed(Error::Refused {
                entry: reference.to_string(),
                refusal,
            }))
        };
        let manifest = match self.client.find_manifest(&reference).await {
            Ok(Some(manifest)) => manifest,
            Ok(None) => return Lookup::Miss(Miss::Absent),
            Err(e) => return failed(e),
        };
        let form = match entry::check_manifest(entry, manifest.media_type(), manifest.bytes()) {
            Ok(form) => form,
            Err(refusal) => return refused(refusal),
        };
        // The `config` limit is checked against the descriptor before the
        // request, and the read stops at the descriptor's size.
        let mut config = Vec::new();
        if let Err(e) = self
            .client
            .pull_config(&reference, &form.config, &mut config)
            .await
        {
            return failed(e);
        }
        let (diff_id, created) = match entry::check_config(&config) {
            Ok(checked) => checked,
            Err(refusal) => return refused(refusal),
        };
        // A `created` in the future never expires, as kaniko compares it.
        let expires = created.to_system_time().checked_add(self.ttl);
        if expires.is_some_and(|at| at < SystemTime::now()) {
            return Lookup::Miss(Miss::Expired);
        }
        Lookup::Hit(CachedLayer {
            source: Source::Registry(reference),
            blob: form.layer,
            diff_id,
        })
    }

    async fn push_entry(
        &self,
        entry: &EntryId,
        layer: &BuiltLayer,
        bytes_out: &mut u64,
    ) -> Result<PushOutcome, Error> {
        let reference = self.tagged(entry)?;
        let name = reference.to_string();
        let compression = entry.encoding().compression;
        let media_type = &layer.blob.media_type;
        if media_type.layer_compression() != Some(compression) || media_type.is_foreign_layer() {
            return Err(Error::Encoding {
                entry: name,
                media_type: excerpt(media_type.as_str()),
                encoding: *entry.encoding(),
            });
        }
        let registry = |source| Error::Registry {
            entry: name.clone(),
            source: Box::new(source),
        };
        let document = |source| Error::Document {
            entry: name.clone(),
            source,
        };
        let blob = Descriptor::new(
            entry::layer_type(entry),
            layer.blob.digest.clone(),
            layer.blob.size,
        );
        let outcome = match &layer.mount_from {
            Some(from) => {
                self.client
                    .mount_or_push(&reference, from, &blob, &layer.body)
                    .await
            }
            None => self.client.push_blob(&reference, &blob, &layer.body).await,
        }
        .map_err(registry)?;
        if outcome == PushOutcome::Uploaded {
            *bytes_out += blob.size;
        }
        let created = Timestamp::from_system_time(SystemTime::now());
        let config = entry::config(&layer.diff_id, &layer.platform, created).map_err(document)?;
        let body = BlobBody::Bytes(Bytes::from(config.bytes));
        let pushed = self
            .client
            .push_blob(&reference, &config.descriptor, &body)
            .await
            .map_err(registry)?;
        if pushed == PushOutcome::Uploaded {
            *bytes_out += config.descriptor.size;
        }
        let manifest = entry::manifest(entry, config.descriptor, blob).map_err(document)?;
        let manifest = RawManifest::new(manifest.descriptor.media_type, manifest.bytes);
        // Last, so that the tag never names an entry whose blobs are missing.
        self.client
            .push_manifest(&reference, &manifest)
            .await
            .map_err(registry)?;
        *bytes_out += manifest.size();
        Ok(outcome)
    }
}

impl LayerCache for RegistryCache {
    async fn lookup(&self, entry: &EntryId) -> Lookup {
        let span = debug_span!("rnk_cache.lookup", hit = Empty);
        let found = self.find(entry).instrument(span.clone()).await;
        span.record("hit", matches!(found, Lookup::Hit(_)));
        found
    }

    async fn push(&self, entry: &EntryId, layer: &BuiltLayer) -> Pushed {
        let span = debug_span!("rnk_cache.push", bytes_out = Empty);
        let mut bytes_out = 0;
        let pushed = self
            .push_entry(entry, layer, &mut bytes_out)
            .instrument(span.clone())
            .await;
        span.record("bytes_out", bytes_out);
        match pushed {
            Ok(outcome) => Pushed::Done(outcome),
            Err(e) => Pushed::Failed(e),
        }
    }
}
