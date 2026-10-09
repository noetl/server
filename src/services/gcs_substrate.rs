//! A GCS-backed `DurableSubstrate` — [noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460) A2.
//!
//! # What this makes true, and what it does not
//!
//! Prod runs **RF=1**: the embedded event log's only replica is a local PVC, so
//! `survives_node_loss` is `false` and `single_point_of_failure` is `1` (A1 measured and
//! published that). Giving the engine a second replica in a `Remote` failure domain changes
//! that — **for sealed parts only**.
//!
//! ⚠⚠ The precise claim, because the imprecise version of it is dangerous:
//!
//! - **Sealed parts** are uploaded to every replica, so after node loss they are
//!   recoverable from GCS. For those, `survives_node_loss` becomes true.
//! - **The unsealed tail is still RF=1.** Records appended since the last seal exist only in
//!   the local writer. Losing the node loses them. That exposure is what B3 (a streaming
//!   tail replica) closes, and B2 (age-based sealing) shrinks in the meantime.
//! - Replication is **asynchronous** by construction: `register_and_upload` hands an
//!   `UploadJob` to a background channel and the append returns immediately. So even for
//!   sealed parts there is a window between "sealed" and "uploaded" during which the only
//!   copy is local.
//!
//! Therefore recovery from node loss yields a **bounded-loss consistent prefix** of the log —
//! everything up to the last part that finished uploading. It is **not** "no data loss", and
//! this module must never be described that way.
//!
//! The asynchrony is not a shortcut: the measured in-region GCS put latency is **p50 77 ms,
//! p99 234 ms** against a ~4 ms local fsync, so a synchronous remote write on the append
//! path would dominate it. That measurement is what #460 decision (i) turned on.
//!
//! # Why this lives in the server, not in `ehdb-l0`
//!
//! `ehdb-l0` has no HTTP client and a deliberately lean dependency set (serde, ehdb-core,
//! twox-hash). Adding `reqwest` to the storage core to reach one backend would be real
//! weight on every consumer. `DurableSubstrate` is the extension point for exactly this, and
//! the server already owns a prod-proven GCS client. So the core stays pure and the
//! integration lives where the client already is.

use std::sync::Arc;

use ehdb_core::{EhdbError, Result as EhdbResult};
use ehdb_l0::failure_domain::FailureDomain;
use ehdb_l0::DurableSubstrate;

use crate::services::object_backend::GcsBackend;

/// The media type every substrate object is written with. Parts and manifests are opaque
/// frames to GCS.
const OCTET_STREAM: &str = "application/octet-stream";

pub struct GcsSubstrate {
    backend: Arc<GcsBackend>,
    /// Key prefix inside the bucket, normalised to end in `/` (or empty).
    prefix: String,
    /// A runtime owned by this substrate.
    ///
    /// ⚠⚠ The trait is **synchronous** and the client is async, so something has to bridge
    /// them. Using the ambient runtime via `Handle::block_on` panics ("cannot block the
    /// current thread from within a runtime"), and `open_replicated` runs on a tokio worker
    /// during startup — so the naive bridge would panic at boot, not under load.
    ///
    /// Owning a runtime means the future always runs somewhere that is not the caller's
    /// thread, whatever the caller's context is.
    rt: tokio::runtime::Runtime,
}

impl std::fmt::Debug for GcsSubstrate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcsSubstrate")
            .field("bucket", &self.backend.bucket())
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl GcsSubstrate {
    /// Open a substrate against `bucket`, optionally under `prefix`.
    ///
    /// `endpoint` selects real GCS or an emulator, exactly as the archive tier does.
    pub fn open(endpoint: &str, bucket: &str, prefix: &str) -> std::result::Result<Self, String> {
        if bucket.trim().is_empty() {
            return Err("gcs substrate: bucket is empty".to_string());
        }
        let backend = GcsBackend::open_for_archive(endpoint, bucket)?;
        // ⚠ Two worker threads, not one: `flush_and_wait_uploads` can have several uploads
        // outstanding, and a single-worker runtime would serialise them behind each other
        // while the caller blocks on the first.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ehdb-gcs-substrate")
            .enable_all()
            .build()
            .map_err(|e| format!("gcs substrate: runtime: {e}"))?;
        let prefix = normalise_prefix(prefix);
        Ok(Self {
            backend: Arc::new(backend),
            prefix,
            rt,
        })
    }

    fn key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    /// Run an async operation to completion from a synchronous context.
    ///
    /// The future is spawned onto this substrate's own runtime and the caller waits on a
    /// channel. When the caller is itself a multi-threaded tokio worker, the wait is wrapped
    /// in `block_in_place` so the scheduler can move other tasks off this thread — checked
    /// by flavour, because `block_in_place` itself panics on a current-thread runtime.
    fn blocking<F, T>(&self, fut: F) -> EhdbResult<T>
    where
        F: std::future::Future<Output = EhdbResult<T>> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
        });
        let received = match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| rx.recv())
            }
            _ => rx.recv(),
        };
        match received {
            Ok(r) => r,
            // The sender was dropped without sending: the spawned task panicked. Surface it
            // as a storage error rather than hanging or unwrapping.
            Err(_) => Err(EhdbError::Storage(
                "gcs substrate: the upload task ended without a result".into(),
            )),
        }
    }
}

/// Normalise a prefix to `""` or something ending in exactly one `/`.
///
/// ⚠ A prefix that silently lost or doubled its separator would place objects at a different
/// path than the one an operator configured — and because `list_prefix` uses the same
/// function, the mismatch would be invisible: writes and reads would agree with each other
/// while disagreeing with the bucket layout the operator expects.
pub fn normalise_prefix(prefix: &str) -> String {
    let t = prefix.trim().trim_matches('/');
    if t.is_empty() {
        String::new()
    } else {
        format!("{t}/")
    }
}

fn to_ehdb(e: crate::error::AppError) -> EhdbError {
    EhdbError::Storage(e.to_string())
}

impl DurableSubstrate for GcsSubstrate {
    /// ⚠⚠ This is the whole point of the module.
    ///
    /// `FailureDomain::label()` collapses `Remote` to its **provider**, so two different GCS
    /// buckets are ONE failure domain — correctly, since they share an availability fate.
    /// Declaring `Remote` is what makes `survives_node_loss` true and what lets
    /// `validate_replica_domains` accept a [local, gcs] set as genuinely independent.
    ///
    /// Declaring it wrongly would be worse than not declaring it: `Undeclared` is refused by
    /// the replica-domain check, whereas a false `Remote` would be accepted and would make
    /// the single-point-of-failure gauge read 0 on a store that still has one copy.
    fn failure_domain(&self) -> FailureDomain {
        FailureDomain::Remote {
            provider: "gcs".to_string(),
            bucket: self.backend.bucket().to_string(),
        }
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> EhdbResult<bool> {
        let b = self.backend.clone();
        let k = self.key(key);
        let body = bytes.to_vec();
        self.blocking(async move {
            b.put_if_absent(&k, OCTET_STREAM, &body)
                .await
                .map_err(to_ehdb)
        })
    }

    fn put_overwrite(&self, key: &str, bytes: &[u8]) -> EhdbResult<()> {
        let b = self.backend.clone();
        let k = self.key(key);
        let body = bytes.to_vec();
        self.blocking(async move { b.put(&k, OCTET_STREAM, &body).await.map_err(to_ehdb) })
    }

    fn get_range(&self, key: &str, offset: u64, len: u64) -> EhdbResult<Vec<u8>> {
        let b = self.backend.clone();
        let k = self.key(key);
        // The over-read refusal lives in `GcsBackend::get_range`, which errors rather than
        // returning a short read — see its doc comment for why that is load-bearing.
        self.blocking(async move { b.get_range(&k, offset, len).await.map_err(to_ehdb) })
    }

    fn get_all(&self, key: &str) -> EhdbResult<Vec<u8>> {
        let b = self.backend.clone();
        let k = self.key(key);
        let shown = k.clone();
        self.blocking(async move {
            match b.get(&k).await.map_err(to_ehdb)? {
                Some(row) => Ok(row.bytes),
                // ⚠ `NotFound`, not an empty vec. An absent object and a zero-length object
                // are different facts, and conflating them would let a missing part read as
                // a present-but-empty one.
                None => Err(EhdbError::NotFound(shown)),
            }
        })
    }

    fn exists(&self, key: &str) -> EhdbResult<bool> {
        let b = self.backend.clone();
        let k = self.key(key);
        self.blocking(async move { Ok(b.get(&k).await.map_err(to_ehdb)?.is_some()) })
    }

    fn list_prefix(&self, prefix: &str) -> EhdbResult<Vec<String>> {
        let b = self.backend.clone();
        let full = self.key(prefix);
        let strip = self.prefix.clone();
        // ⚠ The cap is explicit and large. `GcsBackend::list` takes a limit, and a silently
        // truncated listing is the shape that made `collect_candidates` unsound for the
        // prune floor (#459): a short list reads as a complete one. A manifest recovery that
        // saw only part of the bucket would rebuild a short log and call it whole.
        const MAX_KEYS: usize = 100_000;
        self.blocking(async move {
            let keys = b.list(&full, MAX_KEYS).await.map_err(to_ehdb)?;
            if keys.len() >= MAX_KEYS {
                return Err(EhdbError::Storage(format!(
                    "gcs substrate: listing {full} hit the {MAX_KEYS}-key cap; refusing to \
                     return a possibly-truncated listing as a complete one"
                )));
            }
            Ok(keys
                .into_iter()
                .map(|k| k.strip_prefix(&strip).unwrap_or(&k).to_string())
                .collect())
        })
    }

    fn delete(&self, key: &str) -> EhdbResult<()> {
        let b = self.backend.clone();
        let k = self.key(key);
        self.blocking(async move {
            b.delete(&k).await.map_err(to_ehdb)?;
            Ok(())
        })
    }
}
