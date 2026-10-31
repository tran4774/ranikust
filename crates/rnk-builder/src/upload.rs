//! Pushing built layers while the build goes on (ADR-0055,
//! `design/streaming.md#built-layers-pushed-as-soon-as-they-are-exported`).
//!
//! The target's layers go to every destination as soon as they are
//! exported, at most `jobs` at once, and their files are released once
//! every destination has them; the final [`crate::push`] then finds them
//! there and sends the config and the manifest. At most `jobs` more wait
//! their turn: a stage that exports faster than the registry takes them
//! waits, rather than piling up files (memory, on a `memfd` store). A
//! failed build cancels the uploads and releases the files still waiting.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
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
    /// Released: removed from `store`'s build directory, or, for an
    /// anonymous file, released through its handle.
    Free(Arc<Store>),
}

/// One layer to push: its bytes are a file, by path or by handle.
struct Job {
    blob: Descriptor,
    bytes: LayerBytes,
}

/// Pushes built layers to the destinations while the build runs.
///
/// The uploads run on a thread of their own, driven by the build's
/// runtime: the registry client's futures are not `Send`, so they cannot be
/// tasks; a thread blocking on them is.
#[derive(Debug)]
pub struct Uploader {
    jobs: Mutex<Option<tokio::sync::mpsc::Sender<Job>>>,
    done: Mutex<Option<tokio::sync::oneshot::Receiver<Result<BTreeSet<Digest>>>>>,
    /// Closed (dropped) to cancel the uploads.
    stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    seen: Mutex<BTreeSet<Digest>>,
    /// For a layer handed over after the uploads stopped.
    release: Release,
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
    /// `jobs` uploads at once and `jobs` more waiting, on `handle`'s
    /// runtime.
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
        let (send, receive) = tokio::sync::mpsc::channel::<Job>(jobs.get());
        let (report, done) = tokio::sync::oneshot::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let handle = handle.clone();
        let kept = release.clone();
        std::thread::Builder::new()
            .name("rnk-upload".to_owned())
            .spawn(move || {
                let uploads = Uploads {
                    client: &client,
                    destinations: &destinations,
                    release: &release,
                };
                let r = handle.block_on(uploads.run(jobs, receive, stopped));
                // The build gave up waiting only if it failed already.
                let _unread = report.send(r);
            })
            .map_err(|e| Error::Invalid(format!("starting the upload thread: {e}")))?;
        Ok(Self {
            jobs: Mutex::new(Some(send)),
            done: Mutex::new(Some(done)),
            stop: Mutex::new(Some(stop)),
            seen: Mutex::new(BTreeSet::new()),
            release: kept,
        })
    }

    /// Hands over an exported layer, waiting while `jobs` layers already
    /// wait for an upload: call it from a blocking thread (a stage's),
    /// never from async code. A layer already handed over (the same
    /// digest) is not sent twice. A layer without a file (streamed from its
    /// registry) is not a built layer and is ignored.
    pub fn submit(&self, layer: &BlobLayer) {
        if let LayerBytes::Registry(_) = &layer.bytes {
            return;
        }
        let first = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(layer.blob.digest.clone());
        if !first {
            return;
        }
        // A clone, so that waiting for room never holds the lock that
        // `finish` and `cancel` take.
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let job = Job {
            blob: layer.blob.clone(),
            bytes: layer.bytes.clone(),
        };
        // A closed channel means the uploader stopped on an error, which
        // `finish` reports, or was cancelled: the layer will not go.
        let refused = match jobs {
            Some(jobs) => jobs.blocking_send(job).err().map(|e| e.0),
            None => Some(job),
        };
        if let (Some(job), Release::Free(store)) = (refused, &self.release) {
            free(store, &job.bytes);
        }
    }

    /// Cancels the uploads after a failed build: those running are
    /// dropped (the registry discards their sessions), those waiting are
    /// not started, and, under [`Release::Free`], the files of both are
    /// released. Returns once the uploader's thread is done with the
    /// runtime.
    pub async fn cancel(&self) {
        drop(
            self.stop
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let _ignored = self.finish().await;
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

/// What every upload shares.
struct Uploads<'a> {
    client: &'a Arc<Client>,
    destinations: &'a [Reference],
    release: &'a Release,
}

impl Uploads<'_> {
    /// The uploader's loop: up to `jobs` layers at once, each to every
    /// destination in order, then released. Ends when every job handed
    /// over is done, at the first failure, or when `stopped` is closed;
    /// on a failure or a stop the files not uploaded are released.
    async fn run(
        &self,
        jobs: NonZeroUsize,
        mut receive: tokio::sync::mpsc::Receiver<Job>,
        mut stopped: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<BTreeSet<Digest>> {
        let mut running = FuturesUnordered::new();
        // The files of the uploads running, by digest.
        let mut pending: BTreeMap<Digest, LayerBytes> = BTreeMap::new();
        let mut uploaded = BTreeSet::new();
        let mut open = true;
        let end = loop {
            // Every job handed over is done. (The stop branch never
            // disables itself, so `select!`'s `else` cannot say it.)
            if !open && running.is_empty() {
                break Ok(uploaded);
            }
            tokio::select! {
                biased;
                _ = &mut stopped => {
                    break Err(Error::Invalid("the uploads were cancelled".to_owned()));
                }
                Some(r) = running.next(), if !running.is_empty() => match r {
                    Ok(digest) => {
                        pending.remove(&digest);
                        uploaded.insert(digest);
                    }
                    // The first failure ends the uploads: the build reports it.
                    Err(e) => break Err(e),
                },
                job = receive.recv(), if open && running.len() < jobs.get() => match job {
                    Some(job) => {
                        pending.insert(job.blob.digest.clone(), job.bytes.clone());
                        running.push(self.upload(job));
                    }
                    None => open = false,
                },
            }
        };
        if end.is_err() {
            // Nothing reads these files any more: the running uploads go
            // first, then what waits, then the files of both.
            drop(running);
            receive.close();
            while let Ok(job) = receive.try_recv() {
                pending.insert(job.blob.digest, job.bytes);
            }
            if let Release::Free(store) = self.release {
                for bytes in pending.values() {
                    free(store, bytes);
                }
            }
        }
        end
    }

    /// One layer to every destination, the later ones on the same registry
    /// mounting it from the first; then its file released.
    async fn upload(&self, job: Job) -> Result<Digest> {
        let span = debug_span!("rnk_builder.upload", bytes = job.blob.size);
        let body = match &job.bytes {
            LayerBytes::File(path) => BlobBody::File(path.clone()),
            LayerBytes::Anon(blob) => BlobBody::Opened(Arc::new(blob.clone())),
            LayerBytes::Registry(_) => {
                return Err(Error::Invalid(
                    "a streamed layer handed to the uploader".to_owned(),
                ));
            }
        };
        let first = self.destinations.first();
        async {
            for destination in self.destinations {
                match first.filter(|f| *f != destination) {
                    Some(from) => {
                        self.client
                            .mount_or_push(destination, from, &job.blob, &body)
                            .await?
                    }
                    None => self.client.push_blob(destination, &job.blob, &body).await?,
                };
            }
            Ok::<_, Error>(())
        }
        .instrument(span)
        .await?;
        if let Release::Free(store) = self.release {
            free(store, &job.bytes);
        }
        Ok(job.blob.digest)
    }
}

/// Releases a layer's file: an anonymous one through its handle, one in
/// the build directory removed. A file that cannot be removed stays until
/// the build directory goes: logged, not fatal. Any other file (a blob of
/// the store) is left alone.
fn free(store: &Store, bytes: &LayerBytes) {
    match bytes {
        LayerBytes::Anon(blob) => {
            blob.release();
        }
        LayerBytes::File(path) => {
            if path.starts_with(store.build_dir())
                && let Err(e) = std::fs::remove_file(path)
            {
                tracing::debug!("keeping the pushed layer {}: {e}", path.display());
            }
        }
        LayerBytes::Registry(_) => {}
    }
}
