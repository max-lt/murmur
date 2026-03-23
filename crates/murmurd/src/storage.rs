//! Storage backend for murmurd: Fjall for DAG entries, filesystem for blobs.

use std::path::Path;
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm};
use anyhow::{Context, Result};
use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};
use murmur_types::BlobHash;
use tracing::{debug, info};

/// The keyspace name for DAG entries within the Fjall database.
const DAG_KEYSPACE: &str = "dag_entries";

/// The keyspace name for the blob push queue.
const PUSH_QUEUE_KEYSPACE: &str = "push_queue";

/// Persistent storage backed by Fjall (DAG) and filesystem (blobs).
pub struct Storage {
    /// Fjall database.
    db: Database,
    /// Keyspace for DAG entries (key = entry hash, value = serialized bytes).
    dag_ks: Keyspace,
    /// Keyspace for the blob push queue (key = blob hash, value = retry count as u32 LE).
    push_queue_ks: Keyspace,
    /// Directory for content-addressed blob files.
    blob_dir: std::path::PathBuf,
    /// Optional AES-256-GCM cipher for blob encryption at rest.
    blob_cipher: Option<Aes256Gcm>,
}

impl Storage {
    /// Open or create storage at the given paths.
    pub fn open(data_dir: &Path, blob_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir).context("create data_dir")?;
        std::fs::create_dir_all(blob_dir).context("create blob_dir")?;

        let db = Database::builder(data_dir)
            .open()
            .context("open fjall database")?;

        let dag_ks = db
            .keyspace(DAG_KEYSPACE, KeyspaceCreateOptions::default)
            .context("open dag_entries keyspace")?;

        let push_queue_ks = db
            .keyspace(PUSH_QUEUE_KEYSPACE, KeyspaceCreateOptions::default)
            .context("open push_queue keyspace")?;

        info!(?data_dir, ?blob_dir, "storage opened");

        Ok(Self {
            db,
            dag_ks,
            push_queue_ks,
            blob_dir: blob_dir.to_path_buf(),
            blob_cipher: None,
        })
    }

    /// Enable blob encryption at rest with the given 32-byte AES-256 key.
    pub fn set_blob_encryption_key(&mut self, key: &[u8; 32]) {
        self.blob_cipher = Some(Aes256Gcm::new_from_slice(key).expect("valid 32-byte key"));
        info!("blob encryption at rest enabled");
    }

    /// Persist a DAG entry (keyed by its hash, first 32 bytes of entry_bytes).
    pub fn persist_dag_entry(&self, entry_bytes: &[u8]) -> Result<()> {
        if entry_bytes.len() < 32 {
            anyhow::bail!("entry_bytes too short to contain hash");
        }
        // The hash is the first field in the postcard-serialized DagEntry.
        // We use the first 32 bytes as the key for content-addressable lookup.
        let hash = &entry_bytes[..32];
        self.dag_ks
            .insert(hash, entry_bytes)
            .context("persist dag entry")?;
        debug!(hash = hex_short(hash), "dag entry persisted");
        Ok(())
    }

    /// Load all persisted DAG entries (for feeding into the engine on startup).
    pub fn load_all_dag_entries(&self) -> Result<Vec<Vec<u8>>> {
        let mut entries = Vec::new();
        for guard in self.dag_ks.iter() {
            let (_key, value) = guard.into_inner().context("iterate dag entries")?;
            entries.push(value.to_vec());
        }
        info!(count = entries.len(), "loaded dag entries from storage");
        Ok(entries)
    }

    /// Store a blob to the filesystem (content-addressed).
    ///
    /// If blob encryption is enabled, the data is encrypted with AES-256-GCM
    /// before writing. The 12-byte nonce is prepended to the ciphertext.
    pub fn store_blob(&self, blob_hash: BlobHash, data: &[u8]) -> Result<()> {
        let path = self.blob_path(blob_hash);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create blob parent dir")?;
        }

        let bytes_to_write = if let Some(ref cipher) = self.blob_cipher {
            let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
            let ciphertext = cipher
                .encrypt(&nonce, data)
                .map_err(|e| anyhow::anyhow!("blob encryption failed: {e}"))?;
            // Prepend nonce (12 bytes) to ciphertext.
            let mut out = Vec::with_capacity(12 + ciphertext.len());
            out.extend_from_slice(&nonce);
            out.extend_from_slice(&ciphertext);
            out
        } else {
            data.to_vec()
        };

        std::fs::write(&path, &bytes_to_write).context("write blob")?;
        debug!(%blob_hash, "blob stored");
        Ok(())
    }

    /// Load a blob from the filesystem.
    ///
    /// If blob encryption is enabled, the data is decrypted after reading.
    pub fn load_blob(&self, blob_hash: BlobHash) -> Result<Option<Vec<u8>>> {
        let path = self.blob_path(blob_hash);
        if path.exists() {
            let raw = std::fs::read(&path).context("read blob")?;

            let data = if let Some(ref cipher) = self.blob_cipher {
                if raw.len() < 12 {
                    anyhow::bail!("encrypted blob too short (missing nonce)");
                }
                let (nonce_bytes, ciphertext) = raw.split_at(12);
                let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
                cipher
                    .decrypt(nonce, ciphertext)
                    .map_err(|e| anyhow::anyhow!("blob decryption failed: {e}"))?
            } else {
                raw
            };

            Ok(Some(data))
        } else {
            Ok(None)
        }
    }

    /// Verify a blob's integrity after reading.
    pub fn load_blob_verified(&self, blob_hash: BlobHash) -> Result<Option<Vec<u8>>> {
        match self.load_blob(blob_hash)? {
            Some(data) => {
                let actual = BlobHash::from_data(&data);
                if actual != blob_hash {
                    anyhow::bail!(
                        "blob integrity check failed: expected {blob_hash}, got {actual}"
                    );
                }
                Ok(Some(data))
            }
            None => Ok(None),
        }
    }

    /// Verify all stored blobs' integrity on startup.
    ///
    /// Iterates every blob file in the blob directory, re-hashes the contents
    /// with blake3, and compares against the filename (which encodes the
    /// expected hash). Returns a list of corrupted blob hashes.
    pub fn verify_all_blobs(&self) -> Result<Vec<BlobHash>> {
        let mut corrupted = Vec::new();
        let mut checked = 0u64;

        if !self.blob_dir.exists() {
            return Ok(corrupted);
        }

        // Walk the blob directory tree: blob_dir/<2>/<2>/<64-hex>
        for top_entry in std::fs::read_dir(&self.blob_dir).context("read blob_dir")? {
            let top_entry = top_entry.context("read blob_dir entry")?;
            if !top_entry.file_type().context("file type")?.is_dir() {
                continue;
            }
            for mid_entry in std::fs::read_dir(top_entry.path()).context("read blob subdir")? {
                let mid_entry = mid_entry.context("read blob subdir entry")?;
                if !mid_entry.file_type().context("file type")?.is_dir() {
                    continue;
                }
                for blob_entry in
                    std::fs::read_dir(mid_entry.path()).context("read blob leaf dir")?
                {
                    let blob_entry = blob_entry.context("read blob file entry")?;
                    let path = blob_entry.path();
                    if !path.is_file() {
                        continue;
                    }

                    // The filename is the hex-encoded expected hash.
                    let filename = match path.file_name().and_then(|n| n.to_str()) {
                        Some(name) => name.to_string(),
                        None => continue,
                    };

                    let expected_bytes = match hex::decode(&filename) {
                        Ok(bytes) if bytes.len() == 32 => {
                            let mut arr = [0u8; 32];
                            arr.copy_from_slice(&bytes);
                            arr
                        }
                        _ => continue, // Skip non-hash files.
                    };
                    let expected_hash = BlobHash::from_bytes(expected_bytes);

                    let data = std::fs::read(&path).context("read blob for verification")?;
                    let actual_hash = BlobHash::from_data(&data);

                    if actual_hash != expected_hash {
                        tracing::warn!(
                            expected = %expected_hash,
                            actual = %actual_hash,
                            ?path,
                            "blob integrity check failed"
                        );
                        corrupted.push(expected_hash);
                    }
                    checked += 1;
                }
            }
        }

        info!(
            checked,
            corrupted = corrupted.len(),
            "blob integrity check complete"
        );
        Ok(corrupted)
    }

    // -----------------------------------------------------------------------
    // Push queue
    // -----------------------------------------------------------------------

    /// Enqueue a blob hash for syncing to peers.
    pub fn push_queue_add(&self, blob_hash: BlobHash) -> Result<()> {
        let retry_count: u32 = 0;
        self.push_queue_ks
            .insert(blob_hash.as_bytes(), retry_count.to_le_bytes())
            .context("enqueue blob for push")?;
        debug!(%blob_hash, "blob enqueued for push");
        Ok(())
    }

    /// Remove a blob hash from the push queue (e.g., after successful sync).
    pub fn push_queue_remove(&self, blob_hash: BlobHash) -> Result<()> {
        self.push_queue_ks
            .remove(blob_hash.as_bytes())
            .context("dequeue blob from push queue")?;
        Ok(())
    }

    /// Get all pending items in the push queue with their retry counts.
    pub fn push_queue_items(&self) -> Result<Vec<(BlobHash, u32)>> {
        let mut items = Vec::new();
        for guard in self.push_queue_ks.iter() {
            let (key, value) = guard.into_inner().context("iterate push queue")?;
            if key.len() == 32 {
                let mut hash_bytes = [0u8; 32];
                hash_bytes.copy_from_slice(&key);
                let retry_count = if value.len() == 4 {
                    u32::from_le_bytes(value[..4].try_into().unwrap())
                } else {
                    0
                };
                items.push((BlobHash::from_bytes(hash_bytes), retry_count));
            }
        }
        Ok(items)
    }

    /// Increment the retry count for a queued blob.
    pub fn push_queue_increment_retry(&self, blob_hash: BlobHash) -> Result<()> {
        if let Some(value) = self
            .push_queue_ks
            .get(blob_hash.as_bytes())
            .context("read push queue entry")?
        {
            let current = if value.len() == 4 {
                u32::from_le_bytes(value[..4].try_into().unwrap())
            } else {
                0
            };
            let next = current.saturating_add(1);
            self.push_queue_ks
                .insert(blob_hash.as_bytes(), next.to_le_bytes())
                .context("update push queue retry count")?;
        }
        Ok(())
    }

    /// Flush the Fjall database to ensure durability.
    pub fn flush(&self) -> Result<()> {
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    /// Content-addressed blob path: `blob_dir/ab/cd/<full_hex>`.
    fn blob_path(&self, blob_hash: BlobHash) -> std::path::PathBuf {
        let hex = blob_hash.to_string();
        self.blob_dir.join(&hex[..2]).join(&hex[2..4]).join(&hex)
    }
}

/// Short hex string for logging.
fn hex_short(bytes: &[u8]) -> String {
    let full: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if full.len() > 16 {
        format!("{}…", &full[..16])
    } else {
        full
    }
}

/// Platform callbacks implementation backed by [`Storage`].
pub struct FjallPlatform {
    storage: Arc<Storage>,
}

impl FjallPlatform {
    /// Create a new platform callbacks instance.
    pub fn new(storage: Arc<Storage>) -> Self {
        Self { storage }
    }
}

impl murmur_engine::PlatformCallbacks for FjallPlatform {
    fn on_dag_entry(&self, entry_bytes: Vec<u8>) {
        if let Err(e) = self.storage.persist_dag_entry(&entry_bytes) {
            tracing::error!(error = %e, "failed to persist DAG entry");
        }
    }

    fn on_blob_received(&self, blob_hash: BlobHash, data: Vec<u8>) {
        if let Err(e) = self.storage.store_blob(blob_hash, &data) {
            tracing::error!(error = %e, %blob_hash, "failed to store blob");
        }
    }

    fn on_blob_needed(&self, blob_hash: BlobHash) -> Option<Vec<u8>> {
        match self.storage.load_blob_verified(blob_hash) {
            Ok(data) => data,
            Err(e) => {
                tracing::error!(error = %e, %blob_hash, "failed to load blob");
                None
            }
        }
    }

    fn on_event(&self, event: murmur_engine::EngineEvent) {
        info!(?event, "engine event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_open_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("db");
        let blob_dir = dir.path().join("blobs");

        let _storage = Storage::open(&data_dir, &blob_dir).unwrap();

        assert!(data_dir.exists());
        assert!(blob_dir.exists());
    }

    #[test]
    fn test_blob_store_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let data = b"hello world";
        let hash = BlobHash::from_data(data);

        storage.store_blob(hash, data).unwrap();
        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn test_blob_load_verified() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let data = b"verified content";
        let hash = BlobHash::from_data(data);

        storage.store_blob(hash, data).unwrap();
        let loaded = storage.load_blob_verified(hash).unwrap().unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn test_blob_load_missing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let hash = BlobHash::from_data(b"nonexistent");
        assert!(storage.load_blob(hash).unwrap().is_none());
    }

    #[test]
    fn test_dag_entry_persist_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        use ed25519_dalek::SigningKey;
        use murmur_dag::Dag;
        use murmur_types::{Action, DeviceId, DeviceRole};
        let sk = SigningKey::from_bytes(&rand::random());
        let device_id = DeviceId::from_verifying_key(&sk.verifying_key());
        let mut dag = Dag::new(device_id, sk);

        let entry = dag.append(Action::DeviceApproved {
            device_id,
            role: DeviceRole::Backup,
        });
        let entry_bytes = entry.to_bytes();

        storage.persist_dag_entry(&entry_bytes).unwrap();
        storage.flush().unwrap();

        let all = storage.load_all_dag_entries().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], entry_bytes);
    }

    #[test]
    fn test_dag_persist_multiple_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        use ed25519_dalek::SigningKey;
        use murmur_dag::Dag;
        use murmur_types::{Action, DeviceId, DeviceRole};
        let sk = SigningKey::from_bytes(&rand::random());
        let device_id = DeviceId::from_verifying_key(&sk.verifying_key());
        let mut dag = Dag::new(device_id, sk);

        let e1 = dag.append(Action::DeviceApproved {
            device_id,
            role: DeviceRole::Backup,
        });
        let e2 = dag.append(Action::DeviceNameChanged {
            device_id,
            name: "NAS".to_string(),
        });

        storage.persist_dag_entry(&e1.to_bytes()).unwrap();
        storage.persist_dag_entry(&e2.to_bytes()).unwrap();
        storage.flush().unwrap();

        // Drop the first storage to release the lock before reopening.
        drop(storage);

        // Reopen storage and reload.
        let storage2 = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        let all = storage2.load_all_dag_entries().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_encrypted_blob_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        storage.set_blob_encryption_key(&[0x42; 32]);

        let data = b"secret document content";
        let hash = BlobHash::from_data(data);

        storage.store_blob(hash, data).unwrap();
        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert_eq!(loaded, data);

        // Verify that the raw file on disk is NOT the plaintext.
        let raw_path = dir
            .path()
            .join("blobs")
            .join(&hash.to_string()[..2])
            .join(&hash.to_string()[2..4])
            .join(hash.to_string());
        let raw = std::fs::read(raw_path).unwrap();
        assert_ne!(raw, data);
        // Raw should be nonce (12) + ciphertext (plaintext + 16 tag).
        assert_eq!(raw.len(), 12 + data.len() + 16);
    }

    #[test]
    fn test_encrypted_blob_tampered_ciphertext_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        storage.set_blob_encryption_key(&[0x42; 32]);

        let data = b"tamper test data";
        let hash = BlobHash::from_data(data);
        storage.store_blob(hash, data).unwrap();

        // Tamper with the ciphertext on disk.
        let raw_path = dir
            .path()
            .join("blobs")
            .join(&hash.to_string()[..2])
            .join(&hash.to_string()[2..4])
            .join(hash.to_string());
        let mut raw = std::fs::read(&raw_path).unwrap();
        // Flip a byte in the ciphertext (after the 12-byte nonce).
        raw[15] ^= 0xff;
        std::fs::write(&raw_path, &raw).unwrap();

        let result = storage.load_blob(hash);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypted_blob_wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap();

        // Store with one key.
        {
            let mut storage =
                Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
            storage.set_blob_encryption_key(&[0x42; 32]);
            let data = b"key mismatch test";
            let hash = BlobHash::from_data(data);
            storage.store_blob(hash, data).unwrap();
        }

        // Try to load with a different key.
        let mut storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        storage.set_blob_encryption_key(&[0x99; 32]);
        let hash = BlobHash::from_data(b"key mismatch test");
        let result = storage.load_blob(hash);
        assert!(result.is_err());
    }

    #[test]
    fn test_push_queue_add_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let hash1 = BlobHash::from_data(b"file one");
        let hash2 = BlobHash::from_data(b"file two");

        storage.push_queue_add(hash1).unwrap();
        storage.push_queue_add(hash2).unwrap();

        let items = storage.push_queue_items().unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|(h, c)| *h == hash1 && *c == 0));
        assert!(items.iter().any(|(h, c)| *h == hash2 && *c == 0));
    }

    #[test]
    fn test_push_queue_remove() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let hash = BlobHash::from_data(b"removable");
        storage.push_queue_add(hash).unwrap();
        assert_eq!(storage.push_queue_items().unwrap().len(), 1);

        storage.push_queue_remove(hash).unwrap();
        assert_eq!(storage.push_queue_items().unwrap().len(), 0);
    }

    #[test]
    fn test_push_queue_increment_retry() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let hash = BlobHash::from_data(b"retry me");
        storage.push_queue_add(hash).unwrap();

        storage.push_queue_increment_retry(hash).unwrap();
        storage.push_queue_increment_retry(hash).unwrap();

        let items = storage.push_queue_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1, 2);
    }

    #[test]
    fn test_push_queue_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();

        let hash = BlobHash::from_data(b"persistent");

        {
            let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
            storage.push_queue_add(hash).unwrap();
            storage.flush().unwrap();
        }

        // Reopen storage — queue should still have the item.
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        let items = storage.push_queue_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, hash);
    }

    #[test]
    fn test_platform_callbacks_dag_entry() {
        let dir = tempfile::tempdir().unwrap();
        let storage =
            Arc::new(Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap());
        let platform = FjallPlatform::new(storage.clone());

        use ed25519_dalek::SigningKey;
        use murmur_dag::Dag;
        use murmur_types::{Action, DeviceId, DeviceRole};
        let sk = SigningKey::from_bytes(&rand::random());
        let device_id = DeviceId::from_verifying_key(&sk.verifying_key());
        let mut dag = Dag::new(device_id, sk);

        let entry = dag.append(Action::DeviceApproved {
            device_id,
            role: DeviceRole::Backup,
        });

        use murmur_engine::PlatformCallbacks;
        platform.on_dag_entry(entry.to_bytes());
        storage.flush().unwrap();

        let all = storage.load_all_dag_entries().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_verify_all_blobs_clean() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        // Store a few blobs.
        let data1 = b"blob one content";
        let data2 = b"blob two content";
        let hash1 = BlobHash::from_data(data1);
        let hash2 = BlobHash::from_data(data2);

        storage.store_blob(hash1, data1).unwrap();
        storage.store_blob(hash2, data2).unwrap();

        let corrupted = storage.verify_all_blobs().unwrap();
        assert!(corrupted.is_empty());
    }

    #[test]
    fn test_verify_all_blobs_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let data = b"original content";
        let hash = BlobHash::from_data(data);

        storage.store_blob(hash, data).unwrap();

        // Tamper with the blob on disk.
        let blob_path = dir
            .path()
            .join("blobs")
            .join(&hash.to_string()[..2])
            .join(&hash.to_string()[2..4])
            .join(hash.to_string());
        std::fs::write(&blob_path, b"tampered content").unwrap();

        let corrupted = storage.verify_all_blobs().unwrap();
        assert_eq!(corrupted.len(), 1);
        assert_eq!(corrupted[0], hash);
    }

    #[test]
    fn test_platform_callbacks_blob() {
        let dir = tempfile::tempdir().unwrap();
        let storage =
            Arc::new(Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap());
        let platform = FjallPlatform::new(storage.clone());

        let data = b"blob data for callback test";
        let hash = BlobHash::from_data(data);

        use murmur_engine::PlatformCallbacks;
        platform.on_blob_received(hash, data.to_vec());

        let loaded = platform.on_blob_needed(hash);
        assert_eq!(loaded, Some(data.to_vec()));
    }
}
