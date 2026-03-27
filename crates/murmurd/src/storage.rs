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

/// Chunk size for streaming encryption (1 MiB).
const ENCRYPT_CHUNK_SIZE: usize = 1024 * 1024;

/// Magic header identifying the chunked encrypted blob format.
///
/// On-disk layout:
/// ```text
/// [4 bytes] magic: b"MCv1"
/// [12 bytes] base_nonce
/// [4 bytes] chunk_count: u32 LE
/// Per chunk:
///   [4 bytes] ciphertext_len: u32 LE (plaintext + 16-byte GCM tag)
///   [ciphertext_len bytes] ciphertext
/// ```
const CHUNKED_ENCRYPT_MAGIC: &[u8; 4] = b"MCv1";

/// Derive a per-chunk nonce by XOR-ing the chunk index into the base nonce.
///
/// This ensures each chunk gets a unique nonce for AES-256-GCM, which is
/// required for security (nonce reuse completely breaks GCM).
fn derive_chunk_nonce(base_nonce: &[u8; 12], chunk_index: u32) -> [u8; 12] {
    let mut nonce = *base_nonce;
    let index_bytes = chunk_index.to_le_bytes();
    for (i, b) in index_bytes.iter().enumerate() {
        nonce[i] ^= b;
    }
    nonce
}

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

    /// Load all persisted DAG entries sorted by (HLC, DeviceId) for deterministic replay.
    ///
    /// Fjall iteration order is not guaranteed to match insertion order, so we
    /// deserialize each entry to extract its HLC and device ID, then sort.
    /// This ensures dependent actions (e.g. DeviceRevoked) are always replayed
    /// after their prerequisites (e.g. DeviceApproved).
    pub fn load_all_dag_entries(&self) -> Result<Vec<Vec<u8>>> {
        use murmur_dag::DagEntry;

        let mut entries: Vec<(u64, murmur_types::DeviceId, Vec<u8>)> = Vec::new();
        for guard in self.dag_ks.iter() {
            let (_key, value) = guard.into_inner().context("iterate dag entries")?;
            let bytes = value.to_vec();
            let entry =
                DagEntry::from_bytes(&bytes).context("deserialize dag entry for sorting")?;
            entries.push((entry.hlc, entry.device_id, bytes));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let sorted: Vec<Vec<u8>> = entries.into_iter().map(|(_, _, bytes)| bytes).collect();
        info!(count = sorted.len(), "loaded dag entries from storage");
        Ok(sorted)
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
                if raw.len() >= 4 && raw[..4] == *CHUNKED_ENCRYPT_MAGIC {
                    // Chunked encrypted format (MCv1).
                    Self::decrypt_chunked(cipher, &raw)?
                } else {
                    // Legacy single-nonce format.
                    if raw.len() < 12 {
                        anyhow::bail!("encrypted blob too short (missing nonce)");
                    }
                    let (nonce_bytes, ciphertext) = raw.split_at(12);
                    let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
                    cipher
                        .decrypt(nonce, ciphertext)
                        .map_err(|e| anyhow::anyhow!("blob decryption failed: {e}"))?
                }
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

    // -----------------------------------------------------------------------
    // Streaming blob storage
    // -----------------------------------------------------------------------

    /// Directory for in-progress streaming blob transfers.
    fn stream_tmp_dir(&self) -> std::path::PathBuf {
        self.blob_dir.join(".tmp")
    }

    /// Temp file path for a streaming transfer in progress.
    fn stream_tmp_path(&self, blob_hash: BlobHash) -> std::path::PathBuf {
        self.stream_tmp_dir().join(blob_hash.to_string())
    }

    /// Start a streaming blob transfer (create temp file).
    pub fn stream_start(&self, blob_hash: BlobHash, _total_size: u64) -> Result<()> {
        let tmp_dir = self.stream_tmp_dir();
        std::fs::create_dir_all(&tmp_dir).context("create stream tmp dir")?;
        let tmp_path = self.stream_tmp_path(blob_hash);
        std::fs::File::create(&tmp_path).context("create stream tmp file")?;
        debug!(%blob_hash, "streaming blob transfer started");
        Ok(())
    }

    /// Append a chunk to a streaming transfer.
    pub fn stream_chunk(&self, blob_hash: BlobHash, offset: u64, data: &[u8]) -> Result<()> {
        use std::io::{Seek, Write};
        let tmp_path = self.stream_tmp_path(blob_hash);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp_path)
            .context("open stream tmp file for chunk write")?;
        file.seek(std::io::SeekFrom::Start(offset))
            .context("seek in stream tmp file")?;
        file.write_all(data).context("write chunk to stream tmp")?;
        Ok(())
    }

    /// Complete a streaming transfer: verify hash and finalize storage.
    ///
    /// For unencrypted blobs, renames the temp file to the final content-addressed
    /// path. For encrypted blobs, reads the temp file, encrypts, writes to final
    /// path, then removes the temp file.
    pub fn stream_complete(&self, blob_hash: BlobHash) -> Result<()> {
        let tmp_path = self.stream_tmp_path(blob_hash);

        // Streaming hash verification (never loads entire file into memory).
        let file = std::fs::File::open(&tmp_path).context("open stream tmp for verification")?;
        let actual = BlobHash::from_reader(file).context("streaming hash verification")?;
        if actual != blob_hash {
            std::fs::remove_file(&tmp_path).ok();
            anyhow::bail!(
                "streaming blob integrity check failed: expected {blob_hash}, got {actual}"
            );
        }

        let final_path = self.blob_path(blob_hash);
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).context("create blob parent dir")?;
        }

        if let Some(ref cipher) = self.blob_cipher {
            // Chunked streaming encryption: read and encrypt in ENCRYPT_CHUNK_SIZE
            // blocks, each with its own derived nonce. Memory stays bounded at
            // ~1 MiB regardless of file size.
            use std::io::{BufReader, Read, Write};

            let file = std::fs::File::open(&tmp_path).context("open stream tmp for encryption")?;
            let file_size = file.metadata().context("stream tmp metadata")?.len();
            let mut reader = BufReader::with_capacity(ENCRYPT_CHUNK_SIZE, file);

            let total_chunks = if file_size == 0 {
                0u32
            } else {
                file_size.div_ceil(ENCRYPT_CHUNK_SIZE as u64) as u32
            };
            let base_nonce_generic = Aes256Gcm::generate_nonce(&mut OsRng);
            let base_nonce: [u8; 12] = base_nonce_generic.into();

            let mut out_file =
                std::fs::File::create(&final_path).context("create encrypted blob file")?;

            // Write header: magic + base_nonce + chunk_count.
            out_file
                .write_all(CHUNKED_ENCRYPT_MAGIC)
                .context("write chunked magic")?;
            out_file
                .write_all(&base_nonce)
                .context("write chunked base nonce")?;
            out_file
                .write_all(&total_chunks.to_le_bytes())
                .context("write chunked chunk count")?;

            let mut buf = vec![0u8; ENCRYPT_CHUNK_SIZE];
            let mut chunk_index: u32 = 0;

            loop {
                // Fill the buffer (may require multiple reads).
                let mut n = 0;
                while n < ENCRYPT_CHUNK_SIZE {
                    let bytes_read = reader
                        .read(&mut buf[n..])
                        .context("read stream tmp chunk for encryption")?;
                    if bytes_read == 0 {
                        break;
                    }
                    n += bytes_read;
                }
                if n == 0 {
                    break;
                }

                let chunk_nonce = derive_chunk_nonce(&base_nonce, chunk_index);
                let nonce = aes_gcm::Nonce::from(chunk_nonce);
                let ciphertext = cipher
                    .encrypt(&nonce, &buf[..n])
                    .map_err(|e| anyhow::anyhow!("chunk {chunk_index} encryption failed: {e}"))?;

                let ct_len = ciphertext.len() as u32;
                out_file
                    .write_all(&ct_len.to_le_bytes())
                    .context("write chunk ciphertext length")?;
                out_file
                    .write_all(&ciphertext)
                    .context("write chunk ciphertext")?;

                chunk_index += 1;
            }

            std::fs::remove_file(&tmp_path).ok();
        } else {
            // Unencrypted: atomic rename.
            std::fs::rename(&tmp_path, &final_path).context("rename stream tmp to final")?;
        }

        debug!(%blob_hash, "streaming blob transfer complete");
        Ok(())
    }

    /// Abort a streaming transfer (clean up temp file).
    pub fn stream_abort(&self, blob_hash: BlobHash) {
        let tmp_path = self.stream_tmp_path(blob_hash);
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path).ok();
            debug!(%blob_hash, "streaming blob transfer aborted, temp cleaned up");
        }
    }

    /// Clean up any stale temp files from interrupted transfers.
    pub fn clean_stream_tmp(&self) -> Result<()> {
        let tmp_dir = self.stream_tmp_dir();
        if tmp_dir.exists() {
            let mut count = 0u64;
            for entry in std::fs::read_dir(&tmp_dir).context("read stream tmp dir")? {
                let entry = entry.context("read stream tmp entry")?;
                if entry.path().is_file() {
                    std::fs::remove_file(entry.path()).ok();
                    count += 1;
                }
            }
            if count > 0 {
                info!(count, "cleaned up stale streaming temp files");
            }
        }
        Ok(())
    }

    /// Decrypt a chunked encrypted blob (MCv1 format).
    fn decrypt_chunked(cipher: &Aes256Gcm, raw: &[u8]) -> Result<Vec<u8>> {
        // Header: 4 (magic) + 12 (base_nonce) + 4 (chunk_count) = 20 bytes.
        if raw.len() < 20 {
            anyhow::bail!("chunked encrypted blob too short for header");
        }
        let base_nonce: [u8; 12] = raw[4..16].try_into().unwrap();
        let chunk_count = u32::from_le_bytes(raw[16..20].try_into().unwrap());

        let mut offset = 20usize;
        let mut plaintext = Vec::new();

        for i in 0..chunk_count {
            if offset + 4 > raw.len() {
                anyhow::bail!("chunked encrypted blob truncated at chunk {i} length");
            }
            let ct_len = u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + ct_len > raw.len() {
                anyhow::bail!("chunked encrypted blob truncated at chunk {i} data");
            }
            let ciphertext = &raw[offset..offset + ct_len];
            offset += ct_len;

            let chunk_nonce = derive_chunk_nonce(&base_nonce, i);
            let nonce = aes_gcm::Nonce::from(chunk_nonce);
            let chunk_plaintext = cipher
                .decrypt(&nonce, ciphertext)
                .map_err(|e| anyhow::anyhow!("chunk {i} decryption failed: {e}"))?;
            plaintext.extend_from_slice(&chunk_plaintext);
        }

        Ok(plaintext)
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

    fn on_blob_stream_start(&self, blob_hash: BlobHash, total_size: u64) {
        if let Err(e) = self.storage.stream_start(blob_hash, total_size) {
            tracing::error!(error = %e, %blob_hash, "failed to start streaming blob");
        }
    }

    fn on_blob_chunk(&self, blob_hash: BlobHash, offset: u64, data: &[u8]) {
        if let Err(e) = self.storage.stream_chunk(blob_hash, offset, data) {
            tracing::error!(error = %e, %blob_hash, offset, "failed to write blob chunk");
        }
    }

    fn on_blob_stream_complete(&self, blob_hash: BlobHash) -> Result<(), String> {
        self.storage
            .stream_complete(blob_hash)
            .map_err(|e| e.to_string())
    }

    fn on_blob_stream_abort(&self, blob_hash: BlobHash) {
        self.storage.stream_abort(blob_hash);
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

    // --- Streaming blob storage tests (M15) ---

    #[test]
    fn test_streaming_blob_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let content = b"streaming blob test data";
        let hash = BlobHash::from_data(content);

        storage.stream_start(hash, content.len() as u64).unwrap();
        storage.stream_chunk(hash, 0, content).unwrap();
        storage.stream_complete(hash).unwrap();

        // Blob should be loadable via normal path.
        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert_eq!(loaded, content);
    }

    #[test]
    fn test_streaming_blob_multiple_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let content = b"chunk_one_chunk_two_chunk_three";
        let hash = BlobHash::from_data(content);

        storage.stream_start(hash, content.len() as u64).unwrap();
        // Write in 10-byte chunks.
        storage.stream_chunk(hash, 0, &content[..10]).unwrap();
        storage.stream_chunk(hash, 10, &content[10..20]).unwrap();
        storage.stream_chunk(hash, 20, &content[20..]).unwrap();
        storage.stream_complete(hash).unwrap();

        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert_eq!(loaded, content);
    }

    #[test]
    fn test_streaming_blob_corruption_detected() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let content = b"original data";
        let hash = BlobHash::from_data(content);

        storage.stream_start(hash, content.len() as u64).unwrap();
        // Write wrong data.
        storage.stream_chunk(hash, 0, b"corrupted dat").unwrap();
        let result = storage.stream_complete(hash);
        assert!(result.is_err());
    }

    #[test]
    fn test_streaming_blob_abort_cleans_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let content = b"will be aborted";
        let hash = BlobHash::from_data(content);

        storage.stream_start(hash, content.len() as u64).unwrap();
        storage.stream_chunk(hash, 0, content).unwrap();

        // Verify tmp file exists.
        let tmp_path = dir.path().join("blobs").join(".tmp").join(hash.to_string());
        assert!(tmp_path.exists());

        // Abort.
        storage.stream_abort(hash);
        assert!(!tmp_path.exists());
    }

    #[test]
    fn test_streaming_blob_complete_cleans_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let content = b"completed transfer";
        let hash = BlobHash::from_data(content);

        storage.stream_start(hash, content.len() as u64).unwrap();
        storage.stream_chunk(hash, 0, content).unwrap();
        storage.stream_complete(hash).unwrap();

        // Tmp file should be gone.
        let tmp_path = dir.path().join("blobs").join(".tmp").join(hash.to_string());
        assert!(!tmp_path.exists());

        // Blob should be at final path.
        let loaded = storage.load_blob(hash).unwrap();
        assert!(loaded.is_some());
    }

    #[test]
    fn test_streaming_blob_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        storage.set_blob_encryption_key(&[0x42; 32]);

        let content = b"encrypted streaming data";
        let hash = BlobHash::from_data(content);

        storage.stream_start(hash, content.len() as u64).unwrap();
        storage.stream_chunk(hash, 0, content).unwrap();
        storage.stream_complete(hash).unwrap();

        // Should decrypt correctly on load.
        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert_eq!(loaded, content);
    }

    #[test]
    fn test_clean_stream_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        // Create a stale temp file.
        let hash = BlobHash::from_data(b"stale");
        storage.stream_start(hash, 5).unwrap();
        storage.stream_chunk(hash, 0, b"stale").unwrap();

        let tmp_path = dir.path().join("blobs").join(".tmp").join(hash.to_string());
        assert!(tmp_path.exists());

        storage.clean_stream_tmp().unwrap();
        assert!(!tmp_path.exists());
    }

    #[test]
    fn test_streaming_multiple_concurrent_transfers() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();

        let content_a = b"blob alpha content here";
        let content_b = b"blob beta different content";
        let hash_a = BlobHash::from_data(content_a);
        let hash_b = BlobHash::from_data(content_b);

        // Start both transfers.
        storage
            .stream_start(hash_a, content_a.len() as u64)
            .unwrap();
        storage
            .stream_start(hash_b, content_b.len() as u64)
            .unwrap();

        // Interleave chunks.
        storage.stream_chunk(hash_a, 0, content_a).unwrap();
        storage.stream_chunk(hash_b, 0, content_b).unwrap();

        // Complete both.
        storage.stream_complete(hash_a).unwrap();
        storage.stream_complete(hash_b).unwrap();

        // Both blobs should be loadable.
        assert_eq!(
            storage.load_blob(hash_a).unwrap().unwrap(),
            content_a.to_vec()
        );
        assert_eq!(
            storage.load_blob(hash_b).unwrap().unwrap(),
            content_b.to_vec()
        );
    }

    #[test]
    fn test_platform_callbacks_streaming() {
        let dir = tempfile::tempdir().unwrap();
        let storage =
            Arc::new(Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap());
        let platform = FjallPlatform::new(storage.clone());

        let content = b"platform streaming test";
        let hash = BlobHash::from_data(content);

        use murmur_engine::PlatformCallbacks;
        platform.on_blob_stream_start(hash, content.len() as u64);
        platform.on_blob_chunk(hash, 0, content);
        let result = platform.on_blob_stream_complete(hash);
        assert!(result.is_ok());

        // Verify blob is stored.
        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert_eq!(loaded, content);
    }

    #[test]
    fn test_streaming_encrypted_multi_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        storage.set_blob_encryption_key(&[0x42; 32]);

        // 2.5 MiB — spans 3 encryption chunks (1 MiB each, last one 0.5 MiB).
        let size = 2 * 1024 * 1024 + 512 * 1024;
        let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let hash = BlobHash::from_data(&content);

        storage.stream_start(hash, content.len() as u64).unwrap();
        // Stream in 256 KiB chunks (smaller than encryption chunk).
        let write_chunk = 256 * 1024;
        let mut offset = 0u64;
        for chunk in content.chunks(write_chunk) {
            storage.stream_chunk(hash, offset, chunk).unwrap();
            offset += chunk.len() as u64;
        }
        storage.stream_complete(hash).unwrap();

        // Verify roundtrip.
        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert_eq!(loaded.len(), content.len());
        assert_eq!(loaded, content);

        // Verify on-disk file uses chunked format (starts with MCv1).
        let hex = hash.to_string();
        let blob_path = dir
            .path()
            .join("blobs")
            .join(&hex[..2])
            .join(&hex[2..4])
            .join(&hex);
        let raw = std::fs::read(&blob_path).unwrap();
        assert_eq!(&raw[..4], b"MCv1");
    }

    #[test]
    fn test_streaming_encrypted_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        storage.set_blob_encryption_key(&[0x42; 32]);

        let content = b"";
        let hash = BlobHash::from_data(content);

        storage.stream_start(hash, 0).unwrap();
        storage.stream_complete(hash).unwrap();

        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_legacy_encrypted_blob_still_loads() {
        // Blobs stored via store_blob (legacy single-nonce format) must still
        // load correctly even after the chunked format was introduced.
        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        storage.set_blob_encryption_key(&[0x42; 32]);

        let data = b"legacy encrypted data";
        let hash = BlobHash::from_data(data);

        // store_blob uses the old single-nonce format.
        storage.store_blob(hash, data).unwrap();

        // Verify the on-disk format is NOT chunked.
        let hex = hash.to_string();
        let blob_path = dir
            .path()
            .join("blobs")
            .join(&hex[..2])
            .join(&hex[2..4])
            .join(&hex);
        let raw = std::fs::read(&blob_path).unwrap();
        assert_ne!(&raw[..4], b"MCv1", "store_blob should use legacy format");

        // load_blob should still decrypt it correctly.
        let loaded = storage.load_blob(hash).unwrap().unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn test_chunked_encrypted_tampered_chunk_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap();
        storage.set_blob_encryption_key(&[0x42; 32]);

        let content = b"tamper test for chunked encryption";
        let hash = BlobHash::from_data(content);

        storage.stream_start(hash, content.len() as u64).unwrap();
        storage.stream_chunk(hash, 0, content).unwrap();
        storage.stream_complete(hash).unwrap();

        // Tamper with ciphertext bytes on disk (after the header).
        let hex = hash.to_string();
        let blob_path = dir
            .path()
            .join("blobs")
            .join(&hex[..2])
            .join(&hex[2..4])
            .join(&hex);
        let mut raw = std::fs::read(&blob_path).unwrap();
        // Flip a byte in the first chunk's ciphertext (after 4+12+4+4 = 24 byte header).
        if raw.len() > 25 {
            raw[25] ^= 0xff;
        }
        std::fs::write(&blob_path, &raw).unwrap();

        let result = storage.load_blob(hash);
        assert!(result.is_err());
    }

    #[test]
    fn test_derive_chunk_nonce_uniqueness() {
        let base = [0xAA; 12];
        let n0 = super::derive_chunk_nonce(&base, 0);
        let n1 = super::derive_chunk_nonce(&base, 1);
        let n2 = super::derive_chunk_nonce(&base, 2);
        assert_ne!(n0, n1);
        assert_ne!(n1, n2);
        assert_ne!(n0, n2);
        // Index 0 should leave base unchanged.
        assert_eq!(n0, base);
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
