//! Pushing built layers while the build goes on (ADR-0055,
//! `design/streaming.md#built-layers-pushed-as-soon-as-they-are-exported`).
//!
//! The target's layers go to every destination as soon as they are
//! exported, at most `jobs` at once, and their files are released once
//! every destination has them; the final [`crate::push`] then finds them
//! there and sends the config and the manifest.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use rnk_oci::{Descriptor, Digest, Reference};
use rnk_registry::{BlobBody, Client};
use rnk_store::Store;
use tracing::{Instrument as _, debug_span};

use crate::error::{Error, Result};
use crate::layer::{BlobLayer, LayerBytes};

/// What happens to a layer's file once every destination holds it.
#[derive(Clone, Debug)]
pub enum Release {
    /// Kept: an output written to a file needs the bytes at the end.
    Keep,
    /// Released: removed from the build directory, or, for an anonymous
    /// file, its descriptor closed by `store`.
    Free(Arc<Store>),
}

/// One layer to push.
struct Job {
    blob: Descriptor,
    path: PathBuf,
}

/// Pushes built layers to the destinations while the build runs.
///
/// The uploads run on a thread of their own, driven by the build's
/// runtime: the registry client's futures are not `Send`, so they cannot be
/// tasks; a thread blocking on them is.
#[derive(Debug)]
pub struct Uploader {
    jobs: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Job>>>,
    done: Mutex<Option<tokio::sync::oneshot::Receiver<Result<BTreeSet<Digest>>>>>,
    seen: Mutex<BTreeSet<Digest>>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("blob", &self.blob.digest)
            .finish()
    }
}

impl Uploader {
    /// Starts the uploader for `destinations` (not empty), with at most
    /// `jobs` uploads at once, on `handle`'s runtime.
    ///
    /// # Errors
    ///
    /// When its thread cannot start.
    pub fn start(
        client: Arc<Client>,
        destinations: Vec<Reference>,
        jobs: NonZeroUsize,
        release: Release,
        handle: &tokio::runtime::Handle,
    ) -> Result<Self> {
        let (send, receive) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (report, done) = tokio::sync::oneshot::channel();
        let handle = handle.clone();
        std::thread::Builder::new()
            .name("rnk-upload".to_owned())
            .spawn(move || {
                let r = handle.block_on(run(&client, &destinations, jobs, &release, receive));
                // The build gave up waiting only if it failed already.
                let _unread = report.send(r);
            })
            .map_err(|e| Error::Invalid(format!("starting the upload thread: {e}")))?;
        Ok(Self {
            jobs: Mutex::new(Some(send)),
            done: Mutex::new(Some(done)),
            seen: Mutex::new(BTreeSet::new()),
        })
    }

    /// Hands over an exported layer. A layer already handed over (the same
    /// digest) is not sent twice. A layer without a file (streamed from its
    /// registry) is not a built layer and is ignored.
    pub fn submit(&self, layer: &BlobLayer) {
        let LayerBytes::File(path) = &layer.bytes else {
            return;
        };
        let first = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(layer.blob.digest.clone());
        if !first {
            return;
        }
        let jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(jobs) = jobs.as_ref() {
            // A closed channel means the uploader stopped on an error,
            // which `finish` reports.
            let _closed = jobs.send(Job {
                blob: layer.blob.clone(),
                path: path.clone(),
            });
        }
    }

    /// Waits for every upload handed over, and returns the digests every
    /// destination now holds.
    ///
    /// # Errors
    ///
    /// The first upload that failed.
    pub async fn finish(&self) -> Result<BTreeSet<Digest>> {
        drop(
            self.jobs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let done = self
            .done
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| Error::Invalid("the uploads were already awaited".to_owned()))?;
        done.await
            .map_err(|_| Error::Invalid("the upload thread stopped".to_owned()))?
    }
}

/// The uploader's loop: up to `jobs` layers at once, each to every
/// destination in order, then released.
async fn run(
    client: &Arc<Client>,
    destinations: &[Reference],
    jobs: NonZeroUsize,
    release: &Release,
    mut receive: tokio::sync::mpsc::UnboundedReceiver<Job>,
) -> Result<BTreeSet<Digest>> {
    let mut running = FuturesUnordered::new();
    let mut uploaded = BTreeSet::new();
    let mut open = true;
    loop {
        tokio::select! {
            job = receive.recv(), if open && running.len() < jobs.get() => match job {
                Some(job) => running.push(upload(client, destinations, release, job)),
                None => open = false,
            },
            Some(r) = running.next(), if !running.is_empty() => {
                // The first failure ends the uploads: the build reports it.
                uploaded.insert(r?);
            }
            else => break,
        }
    }
    Ok(uploaded)
}

/// One layer to every destination, the later ones on the same registry
/// mounting it from the first; then its file released.
async fn upload(
    client: &Client,
    destinations: &[Reference],
    release: &Release,
    job: Job,
) -> Result<Digest> {
    let span = debug_span!("rnk_builder.upload", bytes = job.blob.size);
    let body = BlobBody::File(job.path.clone());
    let first = destinations.first();
    async {
        for destination in destinations {
            match first.filter(|f| *f != destination) {
                Some(from) => {
                    client
                        .mount_or_push(destination, from, &job.blob, &body)
                        .await?
                }
                None => client.push_blob(destination, &job.blob, &body).await?,
            };
        }
        Ok::<_, Error>(())
    }
    .instrument(span)
    .await?;
    if let Release::Free(store) = release {
        free(store, &job.path);
    }
    Ok(job.blob.digest)
}

/// Releases a layer's file: closes an anonymous one, removes one in the
/// build directory. A file that cannot be removed stays until the build
/// directory goes: logged, not fatal.
fn free(store: &Store, path: &std::path::Path) {
    if store.release_anonymous_file(path) {
        return;
    }
    if path.starts_with(store.build_dir())
        && let Err(e) = std::fs::remove_file(path)
    {
        tracing::debug!("keeping the pushed layer {}: {e}", path.display());
    }
}
