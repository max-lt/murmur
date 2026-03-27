//! Signed append-only DAG for Murmur, adapted from Shoal's LogTree.
//!
//! The DAG records all mutations (device joins, file additions, access grants)
//! as signed, hash-chained entries. It is **in-memory only** — the platform is
//! responsible for persisting entries and feeding them back on startup via
//! [`Dag::load_entry`].
//!
//! # Key concepts
//!
//! - Each [`DagEntry`] references its parent entries by hash, forming a DAG.
//! - Entries are signed with Ed25519 and verified on receive.
//! - Tips are entries with no children — they represent the current frontier.
//! - When multiple tips exist (concurrent branches), [`Dag::maybe_merge`]
//!   produces a merge entry that collapses them.
//! - [`MaterializedState`] is a derived cache rebuilt by replaying the DAG.

mod entry;
mod error;
mod state;

pub use entry::DagEntry;
pub use error::DagError;
pub use state::MaterializedState;

use std::collections::{HashMap, HashSet, VecDeque};

use ed25519_dalek::SigningKey;
use murmur_types::{Action, DeviceId, HybridClock};
use tracing::debug;

/// An in-memory signed append-only DAG.
///
/// Owns the entry store, tip set, HLC, and device identity. The platform
/// persists entries externally and feeds them back via [`Dag::load_entry`].
pub struct Dag {
    /// All entries indexed by hash.
    entries: HashMap<[u8; 32], DagEntry>,
    /// Current tip hashes (entries with no children).
    tips: HashSet<[u8; 32]>,
    /// Hybrid logical clock for this device.
    clock: HybridClock,
    /// This device's ID.
    device_id: DeviceId,
    /// This device's signing key.
    signing_key: SigningKey,
    /// Materialized state derived from DAG replay.
    state: MaterializedState,
}

impl Dag {
    /// Create a new empty DAG for the given device.
    pub fn new(device_id: DeviceId, signing_key: SigningKey) -> Self {
        Self {
            entries: HashMap::new(),
            tips: HashSet::new(),
            clock: HybridClock::new(),
            device_id,
            signing_key,
            state: MaterializedState::new(),
        }
    }

    /// Load a previously persisted entry on startup.
    ///
    /// Verifies the entry's hash and signature, adds it to the store, and
    /// applies it to the materialized state. Parents need not be loaded in
    /// order — tip tracking is rebuilt from the full set of loaded entries.
    pub fn load_entry(&mut self, entry: DagEntry) -> Result<(), DagError> {
        entry.verify_hash()?;
        entry.verify_signature()?;

        let hash = entry.hash;

        // Witness the HLC to keep our clock up to date.
        self.clock.witness(entry.hlc);

        // Apply to materialized state.
        self.state.apply(&entry);

        self.entries.insert(hash, entry);

        // Rebuild tips: we do a simple rebuild after all loads.
        // For incremental loading this is fine — the caller should call
        // `rebuild_tips()` after loading all entries if performance matters.
        self.rebuild_tips();

        Ok(())
    }

    /// Rebuild the tip set from scratch.
    ///
    /// An entry is a tip if no other entry references it as a parent.
    pub fn rebuild_tips(&mut self) {
        let all_hashes: HashSet<[u8; 32]> = self.entries.keys().copied().collect();
        let mut referenced: HashSet<[u8; 32]> = HashSet::new();
        for entry in self.entries.values() {
            for parent in &entry.parents {
                referenced.insert(*parent);
            }
        }
        self.tips = all_hashes.difference(&referenced).copied().collect();
    }

    /// Append a new action to the DAG.
    ///
    /// Ticks the HLC, uses current tips as parents, signs the entry, stores it,
    /// updates tips, and applies to materialized state. Returns the new entry
    /// for the platform to persist.
    pub fn append(&mut self, action: Action) -> DagEntry {
        let hlc = self.clock.tick();
        let parents: Vec<[u8; 32]> = self.tips.iter().copied().collect();

        let entry = DagEntry::new_signed(
            hlc,
            self.device_id,
            action,
            parents.clone(),
            &self.signing_key,
        );

        debug!(
            hash = %hex_short(&entry.hash),
            device = %self.device_id,
            "dag: appended entry"
        );

        // Update tips: remove parents, add new entry.
        for p in &parents {
            self.tips.remove(p);
        }
        self.tips.insert(entry.hash);

        // Apply to state.
        self.state.apply(&entry);

        self.entries.insert(entry.hash, entry.clone());
        entry
    }

    /// Receive an entry from a remote peer.
    ///
    /// Verifies hash, signature, and authorization, checks that all parents
    /// exist, stores it, updates tips, and applies to materialized state.
    /// Returns the entry for platform persistence.
    pub fn receive_entry(&mut self, entry: DagEntry) -> Result<DagEntry, DagError> {
        entry.verify_hash()?;
        entry.verify_signature()?;

        // Skip if already known (before authorization, since we already accepted it).
        if self.entries.contains_key(&entry.hash) {
            return Ok(entry);
        }

        // Authorize: check that the signing device is allowed to perform this action.
        self.authorize_entry(&entry)?;

        // Check that all parents exist locally.
        let missing: Vec<[u8; 32]> = entry
            .parents
            .iter()
            .filter(|p| !self.entries.contains_key(*p))
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(DagError::MissingParents(missing));
        }

        // Witness remote HLC.
        self.clock.witness(entry.hlc);

        // Update tips: remove parents that are now non-tips, add new entry.
        for p in &entry.parents {
            self.tips.remove(p);
        }
        self.tips.insert(entry.hash);

        // Apply to state.
        self.state.apply(&entry);

        let hash = entry.hash;
        self.entries.insert(hash, entry.clone());

        debug!(
            hash = %hex_short(&hash),
            "dag: received remote entry"
        );

        Ok(entry)
    }

    /// Apply a batch of sync entries in topological order.
    ///
    /// Sorts entries so that parents come before children, then applies each.
    /// Returns the entries that were actually new (not already known).
    pub fn apply_sync_entries(
        &mut self,
        entries: Vec<DagEntry>,
    ) -> Result<Vec<DagEntry>, DagError> {
        let sorted = topological_sort(entries)?;
        let mut new_entries = Vec::new();
        for entry in sorted {
            if !self.entries.contains_key(&entry.hash) {
                self.receive_entry(entry.clone())?;
                new_entries.push(entry);
            }
        }
        Ok(new_entries)
    }

    /// Compute the delta of entries the remote is missing.
    ///
    /// Given the remote's tip set, walks backward from our tips and returns
    /// all entries not reachable from the remote tips, in topological order.
    pub fn compute_delta(&self, remote_tips: &HashSet<[u8; 32]>) -> Vec<DagEntry> {
        // BFS backward from our tips, stopping at entries the remote already has.
        // An entry is "known to remote" if it's in remote_tips or all paths from
        // it lead to remote tips.

        // First, find all entries reachable from remote tips (what the remote has).
        let remote_known = self.reachable_from(remote_tips);

        // Then collect everything we have that they don't, in topological order.
        let mut delta_hashes: HashSet<[u8; 32]> = HashSet::new();
        for hash in self.entries.keys() {
            if !remote_known.contains(hash) {
                delta_hashes.insert(*hash);
            }
        }

        // Topological sort of the delta entries.
        let delta_entries: Vec<DagEntry> = delta_hashes
            .iter()
            .filter_map(|h| self.entries.get(h).cloned())
            .collect();

        topological_sort(delta_entries).unwrap_or_default()
    }

    /// Auto-merge when multiple tips exist.
    ///
    /// If there are 2+ tips, creates a `Merge` entry that references all tips
    /// as parents, collapsing them into a single tip. Returns the merge entry
    /// if one was created.
    pub fn maybe_merge(&mut self) -> Option<DagEntry> {
        if self.tips.len() < 2 {
            return None;
        }
        debug!(tips = self.tips.len(), "dag: auto-merging tips");
        Some(self.append(Action::Merge))
    }

    /// The current tip hashes.
    pub fn tips(&self) -> &HashSet<[u8; 32]> {
        &self.tips
    }

    /// The current materialized state.
    pub fn state(&self) -> &MaterializedState {
        &self.state
    }

    /// Number of entries in the DAG.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the DAG is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get an entry by hash.
    pub fn get_entry(&self, hash: &[u8; 32]) -> Option<&DagEntry> {
        self.entries.get(hash)
    }

    /// Get all entries (for serialization / transfer).
    pub fn all_entries(&self) -> Vec<DagEntry> {
        self.entries.values().cloned().collect()
    }

    /// Tick the HLC and return the new timestamp.
    ///
    /// Useful when the caller needs an HLC value without appending a DAG entry
    /// (e.g. for generating deterministic IDs).
    pub fn clock_tick(&mut self) -> u64 {
        self.clock.tick()
    }

    /// This device's ID.
    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// Check whether the signing device is authorized to perform the entry's action.
    ///
    /// Rules:
    /// - `DeviceJoinRequest` is always allowed (that's how new devices announce).
    /// - If no devices are approved yet (genesis), a self-approval is allowed.
    /// - All other actions require the signing device to be approved.
    fn authorize_entry(&self, entry: &DagEntry) -> Result<(), DagError> {
        // Join requests are always permitted — unapproved devices must be able to
        // announce themselves.
        if matches!(&entry.action, Action::DeviceJoinRequest { .. }) {
            return Ok(());
        }

        // Genesis case: if no devices are approved yet, allow a self-approval.
        // This bootstraps the network — the first device approves itself.
        let any_approved = self.state.devices.values().any(|d| d.approved);
        if !any_approved
            && let Action::DeviceApproved { device_id, .. } = &entry.action
            && *device_id == entry.device_id
        {
            return Ok(());
        }

        // General rule: the signing device must be approved.
        match self.state.devices.get(&entry.device_id) {
            Some(info) if info.approved => Ok(()),
            _ => Err(DagError::Unauthorized {
                device_id: entry.device_id.to_string(),
                action: entry.action.action_name().to_string(),
            }),
        }
    }

    /// Create a snapshot entry that captures the current materialized state.
    ///
    /// The snapshot serializes the full `MaterializedState` and records its
    /// blake3 hash in an `Action::Snapshot` entry. Returns the snapshot entry
    /// and the serialized state bytes (which the platform should persist
    /// alongside the entry for fast-forward loading).
    pub fn create_snapshot(&mut self) -> (DagEntry, Vec<u8>) {
        let state_bytes = self.state.to_bytes();
        let state_hash = *blake3::hash(&state_bytes).as_bytes();

        let entry = self.append(Action::Snapshot { state_hash });
        debug!(
            hash = %hex_short(&entry.hash),
            entries = self.entries.len(),
            "dag: created snapshot"
        );
        (entry, state_bytes)
    }

    /// Load from a snapshot: replace the current state with a deserialized
    /// snapshot, then add the snapshot entry itself.
    ///
    /// After calling this, the caller should load any entries appended *after*
    /// the snapshot via [`Dag::load_entry`].
    ///
    /// Verifies that the state bytes match the hash recorded in the snapshot
    /// entry before applying.
    pub fn load_snapshot(
        &mut self,
        snapshot_entry: DagEntry,
        state_bytes: &[u8],
    ) -> Result<(), DagError> {
        snapshot_entry.verify_hash()?;
        snapshot_entry.verify_signature()?;

        // Extract and verify the state hash.
        let expected_hash = match &snapshot_entry.action {
            Action::Snapshot { state_hash } => *state_hash,
            _ => {
                return Err(DagError::Deserialization(
                    "expected Snapshot action".to_string(),
                ));
            }
        };

        let actual_hash = *blake3::hash(state_bytes).as_bytes();
        if actual_hash != expected_hash {
            return Err(DagError::InvalidHash);
        }

        // Deserialize and replace the state.
        let state = MaterializedState::from_bytes(state_bytes)?;
        self.state = state;

        // Witness the HLC.
        self.clock.witness(snapshot_entry.hlc);

        // Add the snapshot entry to the store.
        let hash = snapshot_entry.hash;
        self.entries.insert(hash, snapshot_entry);
        self.tips = HashSet::from([hash]);

        Ok(())
    }

    /// Check whether the DAG should create a snapshot.
    ///
    /// Returns `true` if the number of entries since the last snapshot (or
    /// since the beginning) exceeds `interval`.
    pub fn should_snapshot(&self, interval: usize) -> bool {
        if interval == 0 {
            return false;
        }
        // Find the HLC of the most recent snapshot entry.
        let last_snapshot_hlc = self
            .entries
            .values()
            .filter(|e| matches!(e.action, Action::Snapshot { .. }))
            .map(|e| e.hlc)
            .max();
        // Count entries with HLC after the last snapshot (or all if no snapshot).
        let entries_since = match last_snapshot_hlc {
            Some(snap_hlc) => self.entries.values().filter(|e| e.hlc > snap_hlc).count(),
            None => self.entries.len(),
        };
        entries_since >= interval
    }

    /// BFS reachable set from a given set of starting hashes.
    fn reachable_from(&self, start: &HashSet<[u8; 32]>) -> HashSet<[u8; 32]> {
        let mut visited = HashSet::new();
        let mut queue: VecDeque<[u8; 32]> = start.iter().copied().collect();
        while let Some(hash) = queue.pop_front() {
            if !visited.insert(hash) {
                continue;
            }
            if let Some(entry) = self.entries.get(&hash) {
                for parent in &entry.parents {
                    if !visited.contains(parent) {
                        queue.push_back(*parent);
                    }
                }
            }
        }
        visited
    }
}

/// Topological sort using Kahn's algorithm.
///
/// Returns entries ordered so that parents come before children.
fn topological_sort(entries: Vec<DagEntry>) -> Result<Vec<DagEntry>, DagError> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let entry_map: HashMap<[u8; 32], DagEntry> =
        entries.iter().map(|e| (e.hash, e.clone())).collect();
    let hashes: HashSet<[u8; 32]> = entry_map.keys().copied().collect();

    // Compute in-degree (only counting edges within this batch).
    let mut in_degree: HashMap<[u8; 32], usize> = HashMap::new();
    for hash in &hashes {
        in_degree.entry(*hash).or_insert(0);
    }
    for entry in entry_map.values() {
        for parent in &entry.parents {
            // Only count parents that are in this batch.
            if hashes.contains(parent) {
                *in_degree.entry(entry.hash).or_insert(0) += 1;
            }
        }
    }

    // Seed the queue with entries that have no in-batch parents.
    let mut queue: VecDeque<[u8; 32]> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(h, _)| *h)
        .collect();

    // Sort queue for deterministic output (by HLC then DeviceId).
    let mut queue_vec: Vec<[u8; 32]> = queue.drain(..).collect();
    queue_vec.sort_by(|a, b| {
        let ea = &entry_map[a];
        let eb = &entry_map[b];
        ea.hlc.cmp(&eb.hlc).then(ea.device_id.cmp(&eb.device_id))
    });
    queue = queue_vec.into_iter().collect();

    let mut result = Vec::with_capacity(entry_map.len());

    while let Some(hash) = queue.pop_front() {
        let entry = &entry_map[&hash];
        result.push(entry.clone());

        // Find children in this batch (entries that have `hash` as a parent).
        let mut children: Vec<[u8; 32]> = Vec::new();
        for (h, e) in &entry_map {
            if e.parents.contains(&hash) && hashes.contains(h) {
                let deg = in_degree.get_mut(h).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    children.push(*h);
                }
            }
        }
        // Sort children for determinism.
        children.sort_by(|a, b| {
            let ea = &entry_map[a];
            let eb = &entry_map[b];
            ea.hlc.cmp(&eb.hlc).then(ea.device_id.cmp(&eb.device_id))
        });
        for c in children {
            queue.push_back(c);
        }
    }

    Ok(result)
}

/// Format a hash as a short hex string for logging.
fn hex_short(hash: &[u8; 32]) -> String {
    hash.iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use murmur_types::*;
    fn make_keypair() -> (DeviceId, SigningKey) {
        let sk = SigningKey::from_bytes(&rand::random());
        let id = DeviceId::from_verifying_key(&sk.verifying_key());
        (id, sk)
    }

    fn make_dag() -> Dag {
        let (id, sk) = make_keypair();
        Dag::new(id, sk)
    }

    fn sample_file_metadata(device_id: DeviceId) -> FileMetadata {
        FileMetadata {
            blob_hash: BlobHash::from_data(b"test file content"),
            folder_id: FolderId::from_bytes([0xaa; 32]),
            path: "photo.jpg".to_string(),
            size: 1024,
            mime_type: Some("image/jpeg".to_string()),
            created_at: 1700000000,
            modified_at: 1700000000,
            device_origin: device_id,
        }
    }

    // --- Entry basics ---

    #[test]
    fn test_append_single_entry_verify() {
        let mut dag = make_dag();
        let entry = dag.append(Action::Merge);
        assert!(entry.verify_hash().is_ok());
        assert!(entry.verify_signature().is_ok());
    }

    #[test]
    fn test_append_multiple_entries_parents_correct() {
        let mut dag = make_dag();
        let e1 = dag.append(Action::Merge);
        let e2 = dag.append(Action::Merge);
        // e2 should have e1 as parent.
        assert!(e2.parents.contains(&e1.hash));
        assert_eq!(dag.tips().len(), 1);
        assert!(dag.tips().contains(&e2.hash));
    }

    #[test]
    fn test_receive_remote_entry() {
        let (id_a, sk_a) = make_keypair();
        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = make_dag();

        // Self-approve device A so its entries are authorized.
        let approve = dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_b.receive_entry(approve).unwrap();

        let entry = dag_a.append(Action::Merge);
        let received = dag_b.receive_entry(entry.clone()).unwrap();
        assert_eq!(received.hash, entry.hash);
        assert_eq!(dag_b.len(), 2);
    }

    #[test]
    fn test_reject_bad_hash() {
        let mut dag_a = make_dag();
        let mut dag_b = make_dag();

        let mut entry = dag_a.append(Action::Merge);
        entry.hash[0] ^= 0xff; // corrupt hash
        let result = dag_b.receive_entry(entry);
        assert!(matches!(result, Err(DagError::InvalidHash)));
    }

    #[test]
    fn test_reject_bad_signature() {
        let mut dag_a = make_dag();
        let mut dag_b = make_dag();

        let mut entry = dag_a.append(Action::Merge);
        entry.signature_s[0] ^= 0xff; // corrupt signature
        let result = dag_b.receive_entry(entry);
        assert!(matches!(result, Err(DagError::InvalidSignature)));
    }

    #[test]
    fn test_reject_missing_parents() {
        let (id_a, sk_a) = make_keypair();
        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = make_dag();

        // Approve device A so its entries are authorized.
        let approve = dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_b.receive_entry(approve).unwrap();

        let _e1 = dag_a.append(Action::Merge);
        let e2 = dag_a.append(Action::Merge);
        // dag_b doesn't have e1, so receiving e2 should fail.
        let result = dag_b.receive_entry(e2);
        assert!(matches!(result, Err(DagError::MissingParents(_))));
    }

    // --- Concurrent append & merge ---

    #[test]
    fn test_two_devices_concurrent_merge() {
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();

        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = Dag::new(id_b, sk_b);

        // Device A creates the network (self-approve) and approves B.
        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_a.append(Action::DeviceApproved {
            device_id: id_b,
            role: DeviceRole::Full,
        });

        // Sync A's history to B so B is known and approved.
        dag_b.apply_sync_entries(dag_a.all_entries()).unwrap();

        // Both append independently.
        let ea = dag_a.append(Action::Merge);
        let eb = dag_b.append(Action::Merge);

        // dag_a receives eb → now has 2+ tips.
        dag_a.receive_entry(eb).unwrap();
        assert!(dag_a.tips().len() >= 2);

        // dag_b receives ea.
        dag_b.receive_entry(ea).unwrap();
        assert!(dag_b.tips().len() >= 2);

        // Merge on dag_a.
        let merge = dag_a.maybe_merge().unwrap();
        assert_eq!(dag_a.tips().len(), 1);
        assert!(dag_a.tips().contains(&merge.hash));

        // dag_b receives the merge.
        dag_b.receive_entry(merge).unwrap();
        assert_eq!(dag_b.tips().len(), 1);
    }

    #[test]
    fn test_no_merge_single_tip() {
        let mut dag = make_dag();
        dag.append(Action::Merge);
        assert!(dag.maybe_merge().is_none());
    }

    // --- Delta computation ---

    #[test]
    fn test_delta_computation() {
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();

        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = Dag::new(id_b, sk_b);

        // Self-approve device A.
        let approve = dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_b.receive_entry(approve).unwrap();

        // dag_a has 3 more entries.
        let e1 = dag_a.append(Action::Merge);
        let _e2 = dag_a.append(Action::Merge);
        let _e3 = dag_a.append(Action::Merge);

        // dag_b has only the first entry after approval.
        dag_b.receive_entry(e1).unwrap();

        // Delta from dag_a's perspective, given dag_b's tips.
        let delta = dag_a.compute_delta(dag_b.tips());
        assert_eq!(delta.len(), 2); // e2 and e3
    }

    // --- Topological sort ---

    #[test]
    fn test_topological_sort_ordering() {
        let mut dag = make_dag();
        let e1 = dag.append(Action::Merge);
        let e2 = dag.append(Action::Merge);
        let e3 = dag.append(Action::Merge);

        let entries = vec![e3.clone(), e1.clone(), e2.clone()]; // scrambled
        let sorted = topological_sort(entries).unwrap();
        assert_eq!(sorted[0].hash, e1.hash);
        assert_eq!(sorted[1].hash, e2.hash);
        assert_eq!(sorted[2].hash, e3.hash);
    }

    // --- Materialized state: devices ---

    #[test]
    fn test_state_approve_device() {
        let mut dag = make_dag();
        let (new_id, _) = make_keypair();

        dag.append(Action::DeviceApproved {
            device_id: new_id,
            role: DeviceRole::Source,
        });

        let devices = &dag.state().devices;
        assert!(devices.contains_key(&new_id));
        let info = &devices[&new_id];
        assert!(info.approved);
        assert_eq!(info.role, DeviceRole::Source);
    }

    #[test]
    fn test_state_revoke_device() {
        let mut dag = make_dag();
        let (new_id, _) = make_keypair();

        dag.append(Action::DeviceApproved {
            device_id: new_id,
            role: DeviceRole::Backup,
        });
        dag.append(Action::DeviceRevoked { device_id: new_id });

        let info = &dag.state().devices[&new_id];
        assert!(!info.approved);
    }

    #[test]
    fn test_state_device_name_changed() {
        let mut dag = make_dag();
        let (new_id, _) = make_keypair();

        dag.append(Action::DeviceJoinRequest {
            device_id: new_id,
            name: "Old Name".to_string(),
        });
        dag.append(Action::DeviceApproved {
            device_id: new_id,
            role: DeviceRole::Full,
        });
        dag.append(Action::DeviceNameChanged {
            device_id: new_id,
            name: "New Name".to_string(),
        });

        assert_eq!(dag.state().devices[&new_id].name, "New Name");
    }

    // --- Materialized state: files ---

    #[test]
    fn test_state_add_file() {
        let mut dag = make_dag();
        let meta = sample_file_metadata(dag.device_id());
        let key = (meta.folder_id, meta.path.clone());

        dag.append(Action::FileAdded {
            metadata: meta.clone(),
        });

        assert!(dag.state().files.contains_key(&key));
    }

    #[test]
    fn test_state_delete_file() {
        let mut dag = make_dag();
        let meta = sample_file_metadata(dag.device_id());
        let key = (meta.folder_id, meta.path.clone());

        dag.append(Action::FileAdded {
            metadata: meta.clone(),
        });
        dag.append(Action::FileDeleted {
            folder_id: meta.folder_id,
            path: meta.path.clone(),
        });

        assert!(!dag.state().files.contains_key(&key));
    }

    // --- Materialized state: access ---

    #[test]
    fn test_state_grant_access() {
        let mut dag = make_dag();
        let (to_id, _) = make_keypair();

        let grant = AccessGrant {
            to: to_id,
            from: dag.device_id(),
            scope: AccessScope::AllFiles,
            expires_at: 9999999999,
            signature_r: [0xab; 32],
            signature_s: [0xcd; 32],
        };

        dag.append(Action::AccessGranted {
            grant: grant.clone(),
        });

        assert_eq!(dag.state().grants.len(), 1);
        assert_eq!(dag.state().grants[0].to, to_id);
    }

    #[test]
    fn test_state_revoke_access() {
        let mut dag = make_dag();
        let (to_id, _) = make_keypair();

        let grant = AccessGrant {
            to: to_id,
            from: dag.device_id(),
            scope: AccessScope::AllFiles,
            expires_at: 9999999999,
            signature_r: [0xab; 32],
            signature_s: [0xcd; 32],
        };

        dag.append(Action::AccessGranted { grant });
        dag.append(Action::AccessRevoked { to: to_id });

        assert!(dag.state().grants.is_empty());
    }

    // --- Serialization roundtrip ---

    #[test]
    fn test_entry_serialization_roundtrip_merge() {
        let mut dag = make_dag();
        let entry = dag.append(Action::Merge);
        let bytes = entry.to_bytes();
        let decoded = DagEntry::from_bytes(&bytes).unwrap();
        assert_eq!(entry.hash, decoded.hash);
        assert_eq!(entry.hlc, decoded.hlc);
        assert!(decoded.verify_hash().is_ok());
        assert!(decoded.verify_signature().is_ok());
    }

    #[test]
    fn test_entry_serialization_roundtrip_file_added() {
        let mut dag = make_dag();
        let meta = sample_file_metadata(dag.device_id());
        let entry = dag.append(Action::FileAdded { metadata: meta });
        let bytes = entry.to_bytes();
        let decoded = DagEntry::from_bytes(&bytes).unwrap();
        assert_eq!(entry, decoded);
    }

    #[test]
    fn test_entry_serialization_roundtrip_all_variants() {
        let mut dag = make_dag();
        let (other_id, _) = make_keypair();

        let actions = vec![
            Action::DeviceJoinRequest {
                device_id: other_id,
                name: "Phone".to_string(),
            },
            Action::DeviceApproved {
                device_id: other_id,
                role: DeviceRole::Source,
            },
            Action::DeviceRevoked {
                device_id: other_id,
            },
            Action::DeviceNameChanged {
                device_id: other_id,
                name: "New".to_string(),
            },
            Action::FileAdded {
                metadata: sample_file_metadata(dag.device_id()),
            },
            Action::FileModified {
                folder_id: FolderId::from_bytes([0xaa; 32]),
                path: "photo.jpg".to_string(),
                old_hash: BlobHash::from_data(b"old"),
                new_hash: BlobHash::from_data(b"new"),
                metadata: sample_file_metadata(dag.device_id()),
            },
            Action::FileDeleted {
                folder_id: FolderId::from_bytes([0xaa; 32]),
                path: "photo.jpg".to_string(),
            },
            Action::AccessGranted {
                grant: AccessGrant {
                    to: other_id,
                    from: dag.device_id(),
                    scope: AccessScope::SingleFile(BlobHash::from_data(b"f")),
                    expires_at: 42,
                    signature_r: [1; 32],
                    signature_s: [2; 32],
                },
            },
            Action::AccessRevoked { to: other_id },
            Action::FolderCreated {
                folder: SharedFolder {
                    folder_id: FolderId::from_bytes([0xbb; 32]),
                    name: "Photos".to_string(),
                    created_by: dag.device_id(),
                    created_at: 100,
                },
            },
            Action::FolderRemoved {
                folder_id: FolderId::from_bytes([0xbb; 32]),
            },
            Action::FolderSubscribed {
                folder_id: FolderId::from_bytes([0xbb; 32]),
                device_id: dag.device_id(),
                mode: SyncMode::ReadWrite,
            },
            Action::FolderUnsubscribed {
                folder_id: FolderId::from_bytes([0xbb; 32]),
                device_id: dag.device_id(),
            },
            Action::Merge,
            Action::Snapshot {
                state_hash: [0xfe; 32],
            },
        ];

        for action in actions {
            let entry = dag.append(action);
            let bytes = entry.to_bytes();
            let decoded = DagEntry::from_bytes(&bytes).unwrap();
            assert_eq!(entry, decoded);
        }
    }

    // --- Load entries on startup ---

    #[test]
    fn test_load_entries_reconstructs_state() {
        let mut dag = make_dag();
        let device_id = dag.device_id();
        let (new_id, _) = make_keypair();

        // Build some history.
        let e1 = dag.append(Action::DeviceApproved {
            device_id: new_id,
            role: DeviceRole::Backup,
        });
        let meta = sample_file_metadata(device_id);
        let e2 = dag.append(Action::FileAdded {
            metadata: meta.clone(),
        });

        // Create a fresh DAG and load entries.
        let (id2, sk2) = make_keypair();
        let mut fresh = Dag::new(id2, sk2);
        fresh.load_entry(e1).unwrap();
        fresh.load_entry(e2).unwrap();

        assert!(fresh.state().devices.contains_key(&new_id));
        let key = (meta.folder_id, meta.path.clone());
        assert!(fresh.state().files.contains_key(&key));
        assert_eq!(fresh.len(), 2);
    }

    // --- Sync roundtrip ---

    #[test]
    fn test_sync_roundtrip_two_dags_converge() {
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();

        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = Dag::new(id_b, sk_b);

        // Device A creates the network and approves B.
        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_a.append(Action::DeviceApproved {
            device_id: id_b,
            role: DeviceRole::Source,
        });

        // Sync A's history to B so B is known and approved.
        let folder_id = FolderId::from_bytes([0xaa; 32]);
        dag_b.apply_sync_entries(dag_a.all_entries()).unwrap();
        dag_a.append(Action::FileAdded {
            metadata: FileMetadata {
                blob_hash: BlobHash::from_data(b"file1"),
                folder_id,
                path: "a.txt".to_string(),
                size: 10,
                mime_type: None,
                created_at: 0,
                modified_at: 0,
                device_origin: id_a,
            },
        });

        // dag_b adds a file (B is self-approved, so this is allowed locally).
        dag_b.append(Action::FileAdded {
            metadata: FileMetadata {
                blob_hash: BlobHash::from_data(b"file2"),
                folder_id,
                path: "b.txt".to_string(),
                size: 20,
                mime_type: None,
                created_at: 0,
                modified_at: 0,
                device_origin: id_b,
            },
        });

        // Sync: send dag_a's entries to dag_b.
        let delta_a = dag_a.compute_delta(dag_b.tips());
        dag_b.apply_sync_entries(delta_a).unwrap();

        // Sync: send dag_b's entries to dag_a.
        let delta_b = dag_b.compute_delta(dag_a.tips());
        dag_a.apply_sync_entries(delta_b).unwrap();

        // Both should have all files.
        assert!(
            dag_a
                .state()
                .files
                .contains_key(&(folder_id, "a.txt".to_string()))
        );
        assert!(
            dag_a
                .state()
                .files
                .contains_key(&(folder_id, "b.txt".to_string()))
        );
        assert!(
            dag_b
                .state()
                .files
                .contains_key(&(folder_id, "a.txt".to_string()))
        );
        assert!(
            dag_b
                .state()
                .files
                .contains_key(&(folder_id, "b.txt".to_string()))
        );

        // Merge on both to converge tips.
        dag_a.maybe_merge();
        dag_b.maybe_merge();
    }

    // --- LWW conflict resolution ---

    #[test]
    fn test_lww_higher_hlc_wins() {
        // When two devices name the same device differently, the later HLC wins.
        let mut dag = make_dag();
        let (target, _) = make_keypair();

        dag.append(Action::DeviceApproved {
            device_id: target,
            role: DeviceRole::Full,
        });
        dag.append(Action::DeviceNameChanged {
            device_id: target,
            name: "First".to_string(),
        });
        dag.append(Action::DeviceNameChanged {
            device_id: target,
            name: "Second".to_string(),
        });

        // Last write wins — "Second" should be the name.
        assert_eq!(dag.state().devices[&target].name, "Second");
    }

    #[test]
    fn test_lww_tiebreak_by_device_id() {
        // When HLC is equal (simulated), higher DeviceId wins.
        // We simulate this by constructing entries manually with the same HLC.
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();
        let (target, _) = make_keypair();

        // Determine which device ID is "higher".
        let (high_id, high_sk, low_id, low_sk) = if id_a > id_b {
            (id_a, sk_a, id_b, sk_b)
        } else {
            (id_b, sk_b, id_a, sk_a)
        };

        let hlc = 1000u64;

        // Both create a DeviceNameChanged with the same HLC.
        let entry_low = DagEntry::new_signed(
            hlc,
            low_id,
            Action::DeviceNameChanged {
                device_id: target,
                name: "ByLow".to_string(),
            },
            vec![],
            &low_sk,
        );
        let entry_high = DagEntry::new_signed(
            hlc,
            high_id,
            Action::DeviceNameChanged {
                device_id: target,
                name: "ByHigh".to_string(),
            },
            vec![],
            &high_sk,
        );

        // Load into a fresh dag — the state applies entries in order loaded,
        // but LWW with tiebreak means higher DeviceId should win.
        let (viewer_id, viewer_sk) = make_keypair();
        let mut dag = Dag::new(viewer_id, viewer_sk);

        // First approve the target so it exists.
        dag.append(Action::DeviceApproved {
            device_id: target,
            role: DeviceRole::Full,
        });

        // Load both entries (low first, then high).
        dag.load_entry(entry_low).unwrap();
        dag.load_entry(entry_high).unwrap();

        assert_eq!(dag.state().devices[&target].name, "ByHigh");
    }

    // --- Snapshot ---

    #[test]
    fn test_snapshot_entry() {
        let mut dag = make_dag();
        let entry = dag.append(Action::Snapshot {
            state_hash: [0xab; 32],
        });
        assert!(entry.verify_hash().is_ok());
        let bytes = entry.to_bytes();
        let decoded = DagEntry::from_bytes(&bytes).unwrap();
        assert_eq!(entry, decoded);
    }

    // --- Edge cases ---

    #[test]
    fn test_empty_dag() {
        let dag = make_dag();
        assert!(dag.is_empty());
        assert_eq!(dag.tips().len(), 0);
    }

    #[test]
    fn test_receive_duplicate_entry() {
        let (id_a, sk_a) = make_keypair();
        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = make_dag();

        let approve = dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_b.receive_entry(approve).unwrap();

        let entry = dag_a.append(Action::Merge);
        dag_b.receive_entry(entry.clone()).unwrap();
        // Receiving again should succeed (idempotent).
        dag_b.receive_entry(entry).unwrap();
        assert_eq!(dag_b.len(), 2);
    }

    #[test]
    fn test_apply_sync_entries_batch() {
        let (id_a, sk_a) = make_keypair();
        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = make_dag();

        // Self-approve, then append some entries.
        let approve = dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        let e1 = dag_a.append(Action::Merge);
        let e2 = dag_a.append(Action::Merge);
        let e3 = dag_a.append(Action::Merge);

        // Apply all at once (in scrambled order).
        let new = dag_b
            .apply_sync_entries(vec![e3.clone(), approve, e1.clone(), e2.clone()])
            .unwrap();
        assert_eq!(new.len(), 4);
        assert_eq!(dag_b.len(), 4);
    }

    #[test]
    fn test_delta_empty_when_synced() {
        let (id_a, sk_a) = make_keypair();
        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = make_dag();

        let approve = dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_b.receive_entry(approve).unwrap();

        let e1 = dag_a.append(Action::Merge);
        dag_b.receive_entry(e1).unwrap();

        // Both have the same entries — delta should be empty.
        let delta = dag_a.compute_delta(dag_b.tips());
        assert!(delta.is_empty());
    }

    #[test]
    fn test_device_join_request_in_state() {
        let mut dag = make_dag();
        let (new_id, _) = make_keypair();

        dag.append(Action::DeviceJoinRequest {
            device_id: new_id,
            name: "Phone".to_string(),
        });

        // Join request creates a pending (not approved) device.
        let info = &dag.state().devices[&new_id];
        assert!(!info.approved);
        assert_eq!(info.name, "Phone");
    }

    // --- Authorization ---

    #[test]
    fn test_auth_join_request_always_allowed() {
        // A DeviceJoinRequest should be accepted from any device, even unknown ones.
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();

        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = Dag::new(id_b, sk_b);

        // dag_a self-approves (genesis).
        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });

        // Sync dag_a's entries to dag_b so dag_b has id_a approved.
        let entries = dag_a.all_entries();
        dag_b.apply_sync_entries(entries).unwrap();

        // Now an unknown device (id_c) sends a join request via dag_a.
        let (id_c, sk_c) = make_keypair();
        let mut dag_c = Dag::new(id_c, sk_c);
        let join_entry = dag_c.append(Action::DeviceJoinRequest {
            device_id: id_c,
            name: "Phone".to_string(),
        });

        // dag_b should accept this join request even though id_c is unknown.
        let result = dag_b.receive_entry(join_entry);
        assert!(result.is_ok());
    }

    #[test]
    fn test_auth_genesis_self_approval_allowed() {
        // The very first DeviceApproved (self-approval) should succeed when
        // no devices are approved yet.
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();

        let mut dag_a = Dag::new(id_a, sk_a);
        let mut dag_b = Dag::new(id_b, sk_b);

        let self_approve = dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });

        // dag_b has no approved devices — this genesis self-approval should work.
        let result = dag_b.receive_entry(self_approve);
        assert!(result.is_ok());
    }

    #[test]
    fn test_auth_revoked_device_entry_rejected() {
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();

        let mut dag_a = Dag::new(id_a, sk_a);

        // Device A self-approves and approves device B.
        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_a.append(Action::DeviceApproved {
            device_id: id_b,
            role: DeviceRole::Source,
        });

        // Then revokes device B.
        dag_a.append(Action::DeviceRevoked { device_id: id_b });

        // Sync all of A's entries to a fresh dag.
        let (id_viewer, sk_viewer) = make_keypair();
        let mut dag_viewer = Dag::new(id_viewer, sk_viewer);
        dag_viewer.apply_sync_entries(dag_a.all_entries()).unwrap();

        // Device B (now revoked) tries to add a file.
        let mut dag_b = Dag::new(id_b, sk_b);
        let bad_entry = dag_b.append(Action::FileAdded {
            metadata: sample_file_metadata(id_b),
        });

        // The viewer should reject this — device B is revoked.
        let result = dag_viewer.receive_entry(bad_entry);
        assert!(
            matches!(result, Err(DagError::Unauthorized { .. })),
            "expected Unauthorized, got {result:?}"
        );
    }

    #[test]
    fn test_auth_unapproved_device_action_rejected() {
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();

        let mut dag_a = Dag::new(id_a, sk_a);
        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });

        // Sync to viewer.
        let (id_viewer, sk_viewer) = make_keypair();
        let mut dag_viewer = Dag::new(id_viewer, sk_viewer);
        dag_viewer.apply_sync_entries(dag_a.all_entries()).unwrap();

        // Device B is NOT approved but tries to add a file.
        let mut dag_b = Dag::new(id_b, sk_b);
        let bad_entry = dag_b.append(Action::FileAdded {
            metadata: sample_file_metadata(id_b),
        });

        let result = dag_viewer.receive_entry(bad_entry);
        assert!(
            matches!(result, Err(DagError::Unauthorized { .. })),
            "expected Unauthorized, got {result:?}"
        );
    }

    #[test]
    fn test_auth_approved_device_action_accepted() {
        let (id_a, sk_a) = make_keypair();
        let (id_b, sk_b) = make_keypair();

        let mut dag_a = Dag::new(id_a, sk_a);
        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_a.append(Action::DeviceApproved {
            device_id: id_b,
            role: DeviceRole::Source,
        });

        // Sync to viewer.
        let (id_viewer, sk_viewer) = make_keypair();
        let mut dag_viewer = Dag::new(id_viewer, sk_viewer);
        dag_viewer.apply_sync_entries(dag_a.all_entries()).unwrap();

        // Device B is approved and adds a file — should succeed.
        let mut dag_b = Dag::new(id_b, sk_b);
        let entry = dag_b.append(Action::FileAdded {
            metadata: sample_file_metadata(id_b),
        });

        let result = dag_viewer.receive_entry(entry);
        assert!(result.is_ok());
    }

    // --- Snapshot / Compaction ---

    #[test]
    fn test_create_snapshot_captures_state() {
        let mut dag = make_dag();
        dag.append(Action::DeviceApproved {
            device_id: dag.device_id(),
            role: DeviceRole::Full,
        });
        dag.append(Action::FileAdded {
            metadata: sample_file_metadata(dag.device_id()),
        });

        let (snap_entry, state_bytes) = dag.create_snapshot();

        // Verify the entry is a Snapshot action with the correct hash.
        match &snap_entry.action {
            Action::Snapshot { state_hash } => {
                let expected = *blake3::hash(&state_bytes).as_bytes();
                assert_eq!(*state_hash, expected);
            }
            _ => panic!("expected Snapshot action"),
        }

        // Verify the state bytes deserialize correctly.
        let restored = MaterializedState::from_bytes(&state_bytes).unwrap();
        assert_eq!(restored.devices.len(), dag.state().devices.len());
        assert_eq!(restored.files.len(), dag.state().files.len());
    }

    #[test]
    fn test_snapshot_roundtrip_state_serialization() {
        let mut dag = make_dag();
        let device_id = dag.device_id();

        dag.append(Action::DeviceApproved {
            device_id,
            role: DeviceRole::Full,
        });

        let (other_id, _) = make_keypair();
        dag.append(Action::DeviceJoinRequest {
            device_id: other_id,
            name: "Phone".to_string(),
        });
        dag.append(Action::DeviceApproved {
            device_id: other_id,
            role: DeviceRole::Source,
        });
        dag.append(Action::FileAdded {
            metadata: sample_file_metadata(device_id),
        });
        dag.append(Action::AccessGranted {
            grant: AccessGrant {
                to: other_id,
                from: device_id,
                scope: AccessScope::AllFiles,
                expires_at: 9999,
                signature_r: [0u8; 32],
                signature_s: [0u8; 32],
            },
        });

        let state = dag.state();
        let bytes = state.to_bytes();
        let restored = MaterializedState::from_bytes(&bytes).unwrap();

        assert_eq!(restored.devices.len(), state.devices.len());
        assert_eq!(restored.files.len(), state.files.len());
        assert_eq!(restored.grants.len(), state.grants.len());

        // Verify specific values survived the roundtrip.
        assert!(restored.devices[&device_id].approved);
        assert!(restored.devices[&other_id].approved);
        assert_eq!(restored.devices[&other_id].name, "Phone");
        assert_eq!(restored.grants[0].to, other_id);
    }

    #[test]
    fn test_load_snapshot_fast_forward() {
        // Build a DAG with some state, snapshot it, then load the snapshot
        // into a new DAG and verify the state matches.
        let (id_a, sk_a) = make_keypair();
        let mut dag_a = Dag::new(id_a, sk_a.clone());

        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_a.append(Action::FileAdded {
            metadata: sample_file_metadata(id_a),
        });
        dag_a.append(Action::FileAdded {
            metadata: FileMetadata {
                blob_hash: BlobHash::from_data(b"second file"),
                folder_id: FolderId::from_bytes([0xaa; 32]),
                path: "doc.pdf".to_string(),
                size: 2048,
                mime_type: Some("application/pdf".to_string()),
                created_at: 100,
                modified_at: 100,
                device_origin: id_a,
            },
        });

        let (snap_entry, state_bytes) = dag_a.create_snapshot();

        // New DAG loads just the snapshot — no replaying all entries.
        let mut dag_b = Dag::new(id_a, sk_a);
        dag_b.load_snapshot(snap_entry, &state_bytes).unwrap();

        // State should match the original.
        assert_eq!(dag_b.state().devices.len(), dag_a.state().devices.len());
        assert_eq!(dag_b.state().files.len(), dag_a.state().files.len());
        assert_eq!(dag_b.state().files.len(), 2);
        assert!(dag_b.state().devices[&id_a].approved);
    }

    #[test]
    fn test_load_snapshot_tampered_state_rejected() {
        let (id_a, sk_a) = make_keypair();
        let mut dag_a = Dag::new(id_a, sk_a.clone());

        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        let (snap_entry, mut state_bytes) = dag_a.create_snapshot();

        // Tamper with the state bytes.
        if let Some(byte) = state_bytes.last_mut() {
            *byte ^= 0xff;
        }

        let mut dag_b = Dag::new(id_a, sk_a);
        let result = dag_b.load_snapshot(snap_entry, &state_bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_snapshot_then_append_post_snapshot_entries() {
        let (id_a, sk_a) = make_keypair();
        let mut dag_a = Dag::new(id_a, sk_a.clone());

        dag_a.append(Action::DeviceApproved {
            device_id: id_a,
            role: DeviceRole::Full,
        });
        dag_a.append(Action::FileAdded {
            metadata: sample_file_metadata(id_a),
        });

        let (snap_entry, state_bytes) = dag_a.create_snapshot();

        // Append more entries after the snapshot.
        let post_snap_entry = dag_a.append(Action::FileAdded {
            metadata: FileMetadata {
                blob_hash: BlobHash::from_data(b"post-snapshot file"),
                folder_id: FolderId::from_bytes([0xaa; 32]),
                path: "new.txt".to_string(),
                size: 512,
                mime_type: Some("text/plain".to_string()),
                created_at: 200,
                modified_at: 200,
                device_origin: id_a,
            },
        });

        // New DAG: load snapshot, then load the post-snapshot entry.
        let mut dag_b = Dag::new(id_a, sk_a);
        dag_b.load_snapshot(snap_entry, &state_bytes).unwrap();
        dag_b.load_entry(post_snap_entry).unwrap();

        // Should have both the snapshot state AND the post-snapshot file.
        assert_eq!(dag_b.state().files.len(), 2);
    }

    #[test]
    fn test_should_snapshot_interval() {
        let mut dag = make_dag();

        // Empty DAG: should not snapshot.
        assert!(!dag.should_snapshot(5));

        dag.append(Action::DeviceApproved {
            device_id: dag.device_id(),
            role: DeviceRole::Full,
        });
        dag.append(Action::Merge);
        dag.append(Action::Merge);

        // 3 entries, interval 5: not yet.
        assert!(!dag.should_snapshot(5));

        dag.append(Action::Merge);
        dag.append(Action::Merge);

        // 5 entries, interval 5: should snapshot.
        assert!(dag.should_snapshot(5));

        // After snapshot, counter resets.
        dag.create_snapshot();
        // Now we have 6 entries (5 + snapshot), but 0 since last snapshot.
        assert!(!dag.should_snapshot(5));

        // Interval 0 always returns false.
        assert!(!dag.should_snapshot(0));
    }

    #[test]
    fn test_state_hash_deterministic() {
        let mut dag = make_dag();
        dag.append(Action::DeviceApproved {
            device_id: dag.device_id(),
            role: DeviceRole::Full,
        });
        dag.append(Action::FileAdded {
            metadata: sample_file_metadata(dag.device_id()),
        });

        let hash1 = dag.state().state_hash();
        let hash2 = dag.state().state_hash();
        assert_eq!(hash1, hash2);
    }
}
