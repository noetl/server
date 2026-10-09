//! The archive tier's storage surface — [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459) P2.
//!
//! A narrow trait over "put / get / list keys", so the archival logic is testable against
//! an in-process fake **and** runs against the real `GcsBackend` client that is already
//! live in prod (21,569 objects on the results bucket).
//!
//! ⚠ Narrow on purpose. `ObjectBackend::put` takes a `&DbPool` that its GCS arm ignores;
//! threading a database handle through the archive tier to satisfy a signature would make
//! every test need a database to exercise code that never touches one.
//!
//! ⚠⚠ There is **no `delete` on this trait.** The archive is the surviving copy once the
//! hot part is pruned, so the archival layer must not be able to remove it. Reclaiming
//! archived objects is a separate, later decision with its own authority — not something
//! the writer of those objects can do by accident.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

/// Put / get / list over an object store.
#[async_trait]
pub trait ArchiveStore: Send + Sync {
    async fn put(&self, key: &str, media_type: &str, bytes: &[u8]) -> Result<(), String>;
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    /// Keys under `prefix`, capped at `limit`.
    async fn list(&self, prefix: &str, limit: usize) -> Result<Vec<String>, String>;
    /// Where this store writes, for logs and for the archive manifest.
    fn describe(&self) -> String;
}

#[async_trait]
impl ArchiveStore for crate::services::object_backend::GcsBackend {
    async fn put(&self, key: &str, media_type: &str, bytes: &[u8]) -> Result<(), String> {
        crate::services::object_backend::GcsBackend::put(self, key, media_type, bytes)
            .await
            .map_err(|e| e.to_string())
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        crate::services::object_backend::GcsBackend::get(self, key)
            .await
            .map(|o| o.map(|row| row.bytes))
            .map_err(|e| e.to_string())
    }
    async fn list(&self, prefix: &str, limit: usize) -> Result<Vec<String>, String> {
        crate::services::object_backend::GcsBackend::list(self, prefix, limit)
            .await
            .map_err(|e| e.to_string())
    }
    fn describe(&self) -> String {
        format!("gcs:{}", self.bucket())
    }
}

/// An in-process store for tests, with **deliberate fault injection**.
///
/// ⭐ The injection points exist because the properties that matter here are failure
/// properties: a prune must not happen when the archive is corrupt, truncated or missing.
/// A fake that can only succeed cannot prove any of them, and a verification step that has
/// never seen a bad object is a word rather than a check.
#[derive(Default)]
pub struct FakeArchiveStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Keys whose `put` must fail, simulating an upload that never landed.
    fail_put: Mutex<Vec<String>>,
    /// Keys whose `get` must return `None`, simulating an object lost after upload.
    vanish_on_read: Mutex<Vec<String>>,
    /// Keys whose `get` returns corrupted bytes, simulating silent corruption.
    corrupt_on_read: Mutex<Vec<String>>,
}

impl FakeArchiveStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn fail_put_for(&self, key: &str) {
        self.fail_put.lock().unwrap().push(key.to_string());
    }
    pub fn vanish(&self, key: &str) {
        self.vanish_on_read.lock().unwrap().push(key.to_string());
    }
    pub fn corrupt(&self, key: &str) {
        self.corrupt_on_read.lock().unwrap().push(key.to_string());
    }
    /// Remove an object outright — "the upload never happened".
    pub fn remove(&self, key: &str) -> bool {
        self.objects.lock().unwrap().remove(key).is_some()
    }
    pub fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }
    pub fn len(&self) -> usize {
        self.objects.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ArchiveStore for FakeArchiveStore {
    async fn put(&self, key: &str, _media_type: &str, bytes: &[u8]) -> Result<(), String> {
        if self.fail_put.lock().unwrap().iter().any(|k| k == key) {
            return Err(format!("injected put failure for {key}"));
        }
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), bytes.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        if self.vanish_on_read.lock().unwrap().iter().any(|k| k == key) {
            return Ok(None);
        }
        let got = self.objects.lock().unwrap().get(key).cloned();
        if self.corrupt_on_read.lock().unwrap().iter().any(|k| k == key) {
            // Flip one byte: the length is unchanged, so a size check would pass and only a
            // digest can catch it. That is the point of the control.
            return Ok(got.map(|mut b| {
                if let Some(first) = b.first_mut() {
                    *first ^= 0xff;
                }
                b
            }));
        }
        Ok(got)
    }
    async fn list(&self, prefix: &str, limit: usize) -> Result<Vec<String>, String> {
        Ok(self
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .take(limit)
            .cloned()
            .collect())
    }
    fn describe(&self) -> String {
        "fake".into()
    }
}
