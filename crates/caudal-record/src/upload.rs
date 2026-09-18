//! Mirroring recordings to object storage (S3, R2, GCS, or `file://` for
//! tests) with `object_store`.
//!
//! The recorder only enqueues paths; one worker uploads them in order, so a
//! slow or failing bucket never blocks recording. Local files stay the
//! source of truth: a job reads its file when it runs (the newest playlist
//! wins), a file deleted in the meantime is skipped, and a job that keeps
//! failing is retried with backoff and finally given up with an error log.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

/// Files above this go up in parts (S3's minimum part size).
const PART: usize = 5 * 1024 * 1024;
const ATTEMPTS: u32 = 8;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Job {
    local: PathBuf,
    /// `<stream>/<id>/<file>`.
    key: String,
}

pub(crate) struct Uploader {
    queue: Mutex<VecDeque<Job>>,
    notify: Notify,
    /// Jobs queued or running, for tests and shutdown logs.
    busy: Mutex<usize>,
}

impl Uploader {
    /// Parses `url` and starts the worker. Credentials and settings come
    /// from the environment (`AWS_*`, `GOOGLE_*`, ...).
    pub fn start(url: &str) -> Result<Arc<Self>, String> {
        // reqwest is built without a bundled TLS provider (aws-lc is C);
        // ring is the one Caudal uses everywhere. Already installed is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let parsed = url::Url::parse(url).map_err(|e| format!("invalid upload_url {url:?}: {e}"))?;
        let env = std::env::vars().map(|(k, v)| (k.to_ascii_lowercase(), v));
        let (store, prefix) =
            object_store::parse_url_opts(&parsed, env).map_err(|e| format!("upload_url {url:?}: {e}"))?;
        let up = Arc::new(Self { queue: Mutex::default(), notify: Notify::new(), busy: Mutex::new(0) });
        tokio::spawn(worker(up.clone(), Arc::from(store), prefix));
        Ok(up)
    }

    /// Queues `local` for upload as `key`. A job for the same file that has
    /// not started yet moves to the back, so a playlist always goes up after
    /// the segments it lists.
    pub fn enqueue(&self, local: PathBuf, key: String) {
        let job = Job { local, key };
        {
            let mut q = self.queue.lock();
            let before = q.len();
            q.retain(|j| j.local != job.local);
            let mut busy = self.busy.lock();
            *busy -= before - q.len();
            *busy += 1;
            q.push_back(job);
        }
        self.notify.notify_one();
    }

    /// Jobs queued or in flight.
    pub fn pending(&self) -> usize {
        *self.busy.lock()
    }
}

async fn worker(up: Arc<Uploader>, store: Arc<dyn ObjectStore>, prefix: ObjPath) {
    loop {
        let job = up.queue.lock().pop_front();
        let Some(job) = job else {
            up.notify.notified().await;
            continue;
        };
        let dest = if prefix.as_ref().is_empty() {
            ObjPath::from(job.key.as_str())
        } else {
            ObjPath::from(format!("{}/{}", prefix.as_ref(), job.key))
        };
        let mut backoff = Duration::from_secs(1);
        for attempt in 1..=ATTEMPTS {
            match upload(&*store, &job.local, &dest).await {
                Ok(()) => {
                    tracing::debug!(key = %job.key, "uploaded");
                    break;
                }
                Err(Failure::Gone) => {
                    tracing::debug!(key = %job.key, "local file gone; not uploaded");
                    break;
                }
                Err(Failure::Other(e)) if attempt == ATTEMPTS => {
                    tracing::error!(key = %job.key, error = %e, "upload failed; giving up (the local copy is kept)");
                }
                Err(Failure::Other(e)) => {
                    tracing::warn!(key = %job.key, error = %e, attempt, "upload failed; retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
        *up.busy.lock() -= 1;
    }
}

enum Failure {
    Gone,
    Other(String),
}

async fn upload(store: &dyn ObjectStore, local: &std::path::Path, dest: &ObjPath) -> Result<(), Failure> {
    let mut file = match tokio::fs::File::open(local).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Failure::Gone),
        Err(e) => return Err(Failure::Other(e.to_string())),
    };
    let len = file.metadata().await.map_err(|e| Failure::Other(e.to_string()))?.len();
    if len as usize <= PART {
        let mut buf = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buf).await.map_err(|e| Failure::Other(e.to_string()))?;
        store.put(dest, PutPayload::from(buf)).await.map_err(|e| Failure::Other(e.to_string()))?;
        return Ok(());
    }
    let mut mp = store.put_multipart(dest).await.map_err(|e| Failure::Other(e.to_string()))?;
    let res: Result<(), String> = async {
        loop {
            let mut part = Vec::with_capacity(PART);
            (&mut file).take(PART as u64).read_to_end(&mut part).await.map_err(|e| e.to_string())?;
            if part.is_empty() {
                break;
            }
            mp.put_part(PutPayload::from(Bytes::from(part))).await.map_err(|e| e.to_string())?;
        }
        mp.complete().await.map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        let _ = mp.abort().await;
        return Err(Failure::Other(e));
    }
    Ok(())
}
