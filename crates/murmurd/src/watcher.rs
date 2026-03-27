//! Filesystem watching with debouncing and echo suppression.
//!
//! `FolderWatcher` monitors subscribed folder directories for local changes and
//! translates them into engine operations. It also handles reverse sync (writing
//! files from the network to disk) and initial directory scans.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use murmur_types::FolderId;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::ignore::IgnoreFilter;

/// Debounce window: coalesce events within this duration.
const DEBOUNCE_MS: u64 = 500;

/// Echo suppression window: ignore events on paths written by reverse sync within this duration.
const ECHO_SUPPRESS_MS: u64 = 2000;

/// A pending filesystem event waiting for debounce to expire.
#[derive(Debug)]
struct PendingEvent {
    /// The kind of filesystem event.
    kind: FsEventKind,
    /// When the event was last seen.
    last_seen: Instant,
}

/// Simplified filesystem event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsEventKind {
    /// File was created or modified.
    CreateOrModify,
    /// File was removed.
    Remove,
}

/// A debounced, filtered filesystem event ready for processing.
#[derive(Debug, Clone)]
pub struct FolderEvent {
    /// The folder this event belongs to.
    pub folder_id: FolderId,
    /// Relative path within the folder.
    pub relative_path: String,
    /// The kind of event.
    pub kind: FsEventKind,
}

/// Tracks paths recently written by reverse sync to suppress echo events.
#[derive(Debug, Clone)]
pub struct EchoSuppressor {
    /// Map of (folder_id, relative_path) → timestamp of last reverse-sync write.
    recent_writes: Arc<Mutex<HashMap<(FolderId, String), Instant>>>,
}

impl EchoSuppressor {
    /// Create a new echo suppressor.
    pub fn new() -> Self {
        Self {
            recent_writes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record that a file was written by reverse sync.
    pub fn mark_written(&self, folder_id: FolderId, path: &str) {
        let mut map = self.recent_writes.lock().unwrap();
        map.insert((folder_id, path.to_string()), Instant::now());
    }

    /// Check if a path was recently written by reverse sync (within echo window).
    pub fn is_echo(&self, folder_id: FolderId, path: &str) -> bool {
        let mut map = self.recent_writes.lock().unwrap();
        if let Some(ts) = map.get(&(folder_id, path.to_string())) {
            if ts.elapsed() < Duration::from_millis(ECHO_SUPPRESS_MS) {
                return true;
            }
            // Expired — clean up.
            map.remove(&(folder_id, path.to_string()));
        }
        false
    }

    /// Remove expired entries.
    #[allow(dead_code)]
    pub fn cleanup(&self) {
        let mut map = self.recent_writes.lock().unwrap();
        let cutoff = Duration::from_millis(ECHO_SUPPRESS_MS);
        map.retain(|_, ts| ts.elapsed() < cutoff);
    }
}

/// Manages filesystem watchers for all subscribed folders.
pub struct FolderWatcher {
    /// Active notify watchers, keyed by folder ID.
    watchers: HashMap<FolderId, RecommendedWatcher>,
    /// Ignore filters per folder.
    filters: HashMap<FolderId, IgnoreFilter>,
    /// Local directory paths per folder.
    local_paths: HashMap<FolderId, PathBuf>,
    /// Echo suppressor shared with reverse sync.
    echo_suppressor: EchoSuppressor,
    /// Pending events waiting for debounce.
    pending: Arc<Mutex<HashMap<(FolderId, String), PendingEvent>>>,
    /// Channel for sending debounced events to the processing loop.
    event_tx: mpsc::UnboundedSender<FolderEvent>,
}

impl FolderWatcher {
    /// Create a new `FolderWatcher`.
    ///
    /// Returns the watcher and a receiver for debounced filesystem events.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<FolderEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let watcher = Self {
            watchers: HashMap::new(),
            filters: HashMap::new(),
            local_paths: HashMap::new(),
            echo_suppressor: EchoSuppressor::new(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
        };
        (watcher, event_rx)
    }

    /// Get a reference to the echo suppressor (for reverse sync to mark writes).
    pub fn echo_suppressor(&self) -> &EchoSuppressor {
        &self.echo_suppressor
    }

    /// Start watching a folder directory.
    pub fn watch_folder(
        &mut self,
        folder_id: FolderId,
        local_path: &Path,
    ) -> Result<(), notify::Error> {
        // Build ignore filter.
        let filter = IgnoreFilter::new(local_path);
        self.filters.insert(folder_id, filter);
        self.local_paths.insert(folder_id, local_path.to_path_buf());

        // Set up notify watcher with a channel that feeds into our pending map.
        let pending = Arc::clone(&self.pending);
        let root = local_path.to_path_buf();
        let fid = folder_id;

        let mut watcher = notify::recommended_watcher(
            move |res: Result<notify::Event, notify::Error>| match res {
                Ok(event) => {
                    let kind = match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => FsEventKind::CreateOrModify,
                        EventKind::Remove(_) => FsEventKind::Remove,
                        _ => return,
                    };
                    for path in &event.paths {
                        if let Ok(rel) = path.strip_prefix(&root) {
                            let rel_str = rel.to_string_lossy().replace('\\', "/");
                            if rel_str.is_empty() {
                                continue;
                            }
                            let mut map = pending.lock().unwrap();
                            map.insert(
                                (fid, rel_str),
                                PendingEvent {
                                    kind,
                                    last_seen: Instant::now(),
                                },
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "filesystem watcher error");
                }
            },
        )?;

        watcher.watch(local_path, RecursiveMode::Recursive)?;
        self.watchers.insert(folder_id, watcher);

        info!(
            folder_id = %hex::encode(folder_id.as_bytes()),
            path = %local_path.display(),
            "started watching folder"
        );

        Ok(())
    }

    /// Stop watching a folder.
    #[allow(dead_code)]
    pub fn unwatch_folder(&mut self, folder_id: FolderId) {
        if let Some(mut watcher) = self.watchers.remove(&folder_id)
            && let Some(path) = self.local_paths.get(&folder_id)
        {
            let _ = watcher.unwatch(path);
        }
        self.filters.remove(&folder_id);
        self.local_paths.remove(&folder_id);
        // Clean up pending events for this folder.
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|(fid, _), _| *fid != folder_id);
    }

    /// Drain any debounced events that are ready (older than DEBOUNCE_MS).
    ///
    /// Call this periodically (e.g., every 100ms) from a tokio task.
    pub fn drain_ready(&self) {
        let now = Instant::now();
        let debounce = Duration::from_millis(DEBOUNCE_MS);

        let mut pending = self.pending.lock().unwrap();
        let ready: Vec<_> = pending
            .iter()
            .filter(|(_, ev)| now.duration_since(ev.last_seen) >= debounce)
            .map(|(key, ev)| (key.clone(), ev.kind))
            .collect();

        for ((folder_id, rel_path), kind) in ready {
            pending.remove(&(folder_id, rel_path.clone()));

            // Check ignore filter.
            if let Some(filter) = self.filters.get(&folder_id) {
                let is_dir = kind == FsEventKind::Remove
                    || self
                        .local_paths
                        .get(&folder_id)
                        .map(|root| root.join(&rel_path).is_dir())
                        .unwrap_or(false);

                if filter.is_ignored(Path::new(&rel_path), is_dir) {
                    debug!(path = %rel_path, "ignoring filtered path");
                    continue;
                }
            }

            // Check echo suppression.
            if self.echo_suppressor.is_echo(folder_id, &rel_path) {
                debug!(path = %rel_path, "suppressing echo event");
                continue;
            }

            // Skip directories — we only sync files.
            if let Some(root) = self.local_paths.get(&folder_id) {
                let full_path = root.join(&rel_path);
                if full_path.is_dir() {
                    continue;
                }
            }

            let event = FolderEvent {
                folder_id,
                relative_path: rel_path,
                kind,
            };

            if self.event_tx.send(event).is_err() {
                error!("folder event receiver dropped");
                return;
            }
        }
    }

    /// Reload the ignore filter for a folder (e.g., when `.murmurignore` changes).
    #[allow(dead_code)]
    pub fn reload_filter(&mut self, folder_id: FolderId) {
        if let Some(path) = self.local_paths.get(&folder_id) {
            self.filters.insert(folder_id, IgnoreFilter::new(path));
            debug!(
                folder_id = %hex::encode(folder_id.as_bytes()),
                "reloaded ignore filter"
            );
        }
    }

    /// Get the local path for a watched folder.
    #[allow(dead_code)]
    pub fn local_path(&self, folder_id: &FolderId) -> Option<&Path> {
        self.local_paths.get(folder_id).map(|p| p.as_path())
    }

    /// Check if a folder is being watched.
    #[allow(dead_code)]
    pub fn is_watching(&self, folder_id: &FolderId) -> bool {
        self.watchers.contains_key(folder_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_echo_suppressor_basic() {
        let suppressor = EchoSuppressor::new();
        let fid = FolderId::from_bytes([1u8; 32]);

        assert!(!suppressor.is_echo(fid, "file.txt"));
        suppressor.mark_written(fid, "file.txt");
        assert!(suppressor.is_echo(fid, "file.txt"));

        // Different path is not suppressed.
        assert!(!suppressor.is_echo(fid, "other.txt"));
    }

    #[test]
    fn test_echo_suppressor_different_folders() {
        let suppressor = EchoSuppressor::new();
        let fid1 = FolderId::from_bytes([1u8; 32]);
        let fid2 = FolderId::from_bytes([2u8; 32]);

        suppressor.mark_written(fid1, "file.txt");
        assert!(suppressor.is_echo(fid1, "file.txt"));
        assert!(!suppressor.is_echo(fid2, "file.txt"));
    }

    #[test]
    fn test_folder_watcher_creation() {
        let (watcher, _rx) = FolderWatcher::new();
        let fid = FolderId::from_bytes([1u8; 32]);
        assert!(!watcher.is_watching(&fid));
    }

    #[test]
    fn test_watch_and_unwatch() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watcher, _rx) = FolderWatcher::new();
        let fid = FolderId::from_bytes([1u8; 32]);

        watcher.watch_folder(fid, dir.path()).unwrap();
        assert!(watcher.is_watching(&fid));
        assert_eq!(watcher.local_path(&fid), Some(dir.path()));

        watcher.unwatch_folder(fid);
        assert!(!watcher.is_watching(&fid));
        assert_eq!(watcher.local_path(&fid), None);
    }

    #[tokio::test]
    async fn test_file_create_detected() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watcher, mut rx) = FolderWatcher::new();
        let fid = FolderId::from_bytes([1u8; 32]);

        watcher.watch_folder(fid, dir.path()).unwrap();

        // Create a file.
        fs::write(dir.path().join("hello.txt"), b"hello").unwrap();

        // Wait for debounce.
        tokio::time::sleep(Duration::from_millis(700)).await;
        watcher.drain_ready();

        let event = rx.try_recv().unwrap();
        assert_eq!(event.folder_id, fid);
        assert_eq!(event.relative_path, "hello.txt");
        assert_eq!(event.kind, FsEventKind::CreateOrModify);
    }

    #[tokio::test]
    async fn test_ignored_file_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watcher, mut rx) = FolderWatcher::new();
        let fid = FolderId::from_bytes([1u8; 32]);

        watcher.watch_folder(fid, dir.path()).unwrap();

        // Create an ignored file.
        fs::write(dir.path().join(".DS_Store"), b"stuff").unwrap();

        // Wait for debounce.
        tokio::time::sleep(Duration::from_millis(700)).await;
        watcher.drain_ready();

        // Should not produce an event.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_echo_suppressed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watcher, mut rx) = FolderWatcher::new();
        let fid = FolderId::from_bytes([1u8; 32]);

        watcher.watch_folder(fid, dir.path()).unwrap();

        // Mark as written by reverse sync BEFORE the file event.
        watcher.echo_suppressor().mark_written(fid, "synced.txt");

        // Create the file (simulating reverse sync write).
        fs::write(dir.path().join("synced.txt"), b"from network").unwrap();

        // Wait for debounce.
        tokio::time::sleep(Duration::from_millis(700)).await;
        watcher.drain_ready();

        // Should be suppressed.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_debounce_coalesces_rapid_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watcher, mut rx) = FolderWatcher::new();
        let fid = FolderId::from_bytes([1u8; 32]);

        watcher.watch_folder(fid, dir.path()).unwrap();

        // Rapid writes to the same file.
        for i in 0..5 {
            fs::write(dir.path().join("rapid.txt"), format!("write {i}")).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Wait for debounce after last write.
        tokio::time::sleep(Duration::from_millis(700)).await;
        watcher.drain_ready();

        // Should produce exactly one event.
        let event = rx.try_recv().unwrap();
        assert_eq!(event.relative_path, "rapid.txt");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_custom_murmurignore_respected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".murmurignore"), "*.log\n").unwrap();

        let (mut watcher, mut rx) = FolderWatcher::new();
        let fid = FolderId::from_bytes([1u8; 32]);

        watcher.watch_folder(fid, dir.path()).unwrap();

        // Give the watcher a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create a .log file (should be ignored) and a .txt file (should be reported).
        fs::write(dir.path().join("debug.log"), b"log data").unwrap();
        fs::write(dir.path().join("readme.txt"), b"hello").unwrap();

        tokio::time::sleep(Duration::from_millis(700)).await;
        watcher.drain_ready();

        let event = rx.try_recv().unwrap();
        assert_eq!(event.relative_path, "readme.txt");
        // No more events (debug.log was ignored).
        assert!(rx.try_recv().is_err());
    }
}
