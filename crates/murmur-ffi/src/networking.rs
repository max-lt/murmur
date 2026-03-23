//! Gossip-based DAG sync for the FFI layer.
//!
//! Adapted from `murmurd/networking.rs` to use [`FfiPlatformCallbacks`] for
//! blob storage instead of Fjall-backed `Storage`.  This enables the Android
//! (and future iOS) app to sync DAG entries and blobs with peers over iroh
//! gossip — the same wire format as `murmurd`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_lite::StreamExt;
use murmur_dag::DagEntry;
use murmur_engine::MurmurEngine;
use murmur_net::{CHUNK_SIZE, CHUNK_THRESHOLD, ChunkBuffer, compress_wire, decompress_wire};
use murmur_types::{Action, BlobHash, DeviceId, GossipMessage, GossipPayload};
use rand::TryRng;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{FfiError, SharedCallbacks};

// ---------------------------------------------------------------------------
// NetworkState
// ---------------------------------------------------------------------------

/// Active networking resources.  Created by [`start_networking`], stored in
/// `MurmurHandle`.
pub(crate) struct NetworkState {
    /// Send serialized DAG entry bytes here to broadcast via gossip.
    pub broadcast_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Number of currently connected gossip peers.
    pub connected_peers: Arc<AtomicU64>,
    /// The iroh endpoint (kept alive for the duration of the session).
    endpoint: iroh::Endpoint,
    /// Spawned task handles — aborted on stop.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl NetworkState {
    /// Abort all background tasks and close the iroh endpoint.
    pub(crate) async fn close(self) {
        for handle in &self.tasks {
            handle.abort();
        }
        self.endpoint.close().await;
        info!("FFI networking stopped");
    }
}

// ---------------------------------------------------------------------------
// start_networking
// ---------------------------------------------------------------------------

/// Start the networking layer: iroh endpoint, gossip subscription, and
/// background receive/broadcast tasks.
///
/// Adapted from `murmurd/networking.rs::start_networking` — uses
/// `FfiPlatformCallbacks` for blob I/O instead of `Storage`.
pub(crate) async fn start_networking(
    engine: Arc<Mutex<MurmurEngine>>,
    ffi_callbacks: SharedCallbacks,
    device_id: DeviceId,
    creator_iroh_key_bytes: [u8; 32],
    is_creator: bool,
    topic: iroh_gossip::TopicId,
) -> Result<NetworkState, FfiError> {
    // Derive the creator's endpoint ID (all peers can compute this).
    let creator_secret = iroh::SecretKey::from_bytes(&creator_iroh_key_bytes);
    let creator_endpoint_id = creator_secret.public();

    // This device's iroh secret key: creator uses the deterministic key,
    // other devices generate a random key.
    let my_secret = if is_creator {
        creator_secret
    } else {
        let mut bytes = [0u8; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .expect("OS RNG should not fail");
        iroh::SecretKey::from_bytes(&bytes)
    };

    // Create iroh endpoint with relay enabled (for NAT traversal).
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(my_secret)
        .alpns(vec![iroh_gossip::ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| FfiError::OperationFailed {
            message: format!("bind iroh endpoint: {e}"),
        })?;

    info!(endpoint_id = %endpoint.id(), "iroh endpoint started (FFI)");

    let mut tasks = Vec::new();

    // Create gossip protocol.
    let gossip = iroh_gossip::Gossip::builder()
        .max_message_size(2 * 1024 * 1024)
        .spawn(endpoint.clone());

    // Accept incoming connections and route to gossip.
    let gossip_for_accept = gossip.clone();
    let ep_for_accept = endpoint.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            let Some(incoming) = ep_for_accept.accept().await else {
                break;
            };
            let g = gossip_for_accept.clone();
            tokio::spawn(async move {
                if let Ok(connecting) = incoming.accept()
                    && let Ok(conn) = connecting.await
                {
                    let _ = g.handle_connection(conn).await;
                }
            });
        }
    }));

    // Subscribe to gossip topic.
    let bootstrap = if is_creator {
        vec![]
    } else {
        vec![creator_endpoint_id]
    };

    let topic_handle =
        gossip
            .subscribe(topic, bootstrap)
            .await
            .map_err(|e| FfiError::OperationFailed {
                message: format!("subscribe to gossip topic: {e}"),
            })?;
    let (sender, mut receiver) = topic_handle.split();

    info!(?topic, "subscribed to gossip topic (FFI)");

    // Channel for outgoing entries to broadcast.
    let (broadcast_tx, mut broadcast_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Broadcast task.
    let sender_for_broadcast = sender.clone();
    let device_id_for_broadcast = device_id;
    tasks.push(tokio::spawn(async move {
        while let Some(entry_bytes) = broadcast_rx.recv().await {
            let gossip_msg = GossipMessage {
                nonce: rand::random(),
                sender: device_id_for_broadcast,
                payload: GossipPayload::DagEntry { entry_bytes },
            };
            let wire_bytes =
                postcard::to_allocvec(&gossip_msg).expect("GossipMessage serialization");
            let compressed = compress_wire(&wire_bytes);
            if let Err(e) = sender_for_broadcast
                .broadcast(bytes::Bytes::from(compressed))
                .await
            {
                warn!(error = %e, "gossip broadcast failed (FFI)");
            }
        }
    }));

    // Connected peer tracking.
    let connected_peers = Arc::new(AtomicU64::new(0));
    let peers_for_recv = connected_peers.clone();

    // Receive task.
    let engine_for_recv = engine.clone();
    let callbacks_for_recv = ffi_callbacks.clone();
    let sender_for_recv = sender.clone();
    let device_id_for_recv = device_id;
    tasks.push(tokio::spawn(async move {
        let engine = engine_for_recv;
        let mut chunk_buffers: HashMap<BlobHash, ChunkBuffer> = HashMap::new();
        while let Some(event) = receiver.next().await {
            match event {
                Ok(iroh_gossip::api::Event::Received(msg)) => {
                    let wire_bytes = match decompress_wire(&msg.content) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(error = %e, "failed to decompress gossip message (FFI)");
                            continue;
                        }
                    };
                    handle_gossip_message(
                        &wire_bytes,
                        &engine,
                        &callbacks_for_recv,
                        &sender_for_recv,
                        device_id_for_recv,
                        &mut chunk_buffers,
                    )
                    .await;
                }
                Ok(iroh_gossip::api::Event::NeighborUp(id)) => {
                    let count = peers_for_recv.fetch_add(1, Ordering::Relaxed) + 1;
                    info!(%id, count, "gossip peer connected (FFI)");

                    // Send DagSyncRequest with our tips.
                    let tips: Vec<[u8; 32]> = {
                        let eng = engine.lock().unwrap();
                        eng.tips().iter().copied().collect()
                    };
                    let sync_msg = GossipMessage {
                        nonce: rand::random(),
                        sender: device_id_for_recv,
                        payload: GossipPayload::DagSyncRequest { tips },
                    };
                    if let Err(e) = broadcast_gossip(&sender_for_recv, &sync_msg).await {
                        warn!(error = %e, "sync request broadcast failed (FFI)");
                    }
                    debug!("sent DagSyncRequest to peers (FFI)");
                }
                Ok(iroh_gossip::api::Event::NeighborDown(id)) => {
                    let count = peers_for_recv.fetch_sub(1, Ordering::Relaxed) - 1;
                    info!(%id, count, "gossip peer disconnected (FFI)");
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "gossip receive error (FFI)"),
            }
        }
    }));

    // Send initial DagSyncRequest so existing peers can send us their delta.
    let initial_tips: Vec<[u8; 32]> = {
        let eng = engine.lock().unwrap();
        eng.tips().iter().copied().collect()
    };
    let initial_sync_msg = GossipMessage {
        nonce: rand::random(),
        sender: device_id,
        payload: GossipPayload::DagSyncRequest { tips: initial_tips },
    };
    if let Err(e) = broadcast_gossip(&sender, &initial_sync_msg).await {
        debug!(error = %e, "initial sync request failed — no peers yet (FFI)");
    }

    Ok(NetworkState {
        broadcast_tx,
        connected_peers,
        endpoint,
        tasks,
    })
}

// ---------------------------------------------------------------------------
// Gossip helpers
// ---------------------------------------------------------------------------

/// Serialize, compress, and broadcast a GossipMessage.
async fn broadcast_gossip(
    gossip_sender: &iroh_gossip::api::GossipSender,
    msg: &GossipMessage,
) -> Result<(), FfiError> {
    let wire = postcard::to_allocvec(msg).expect("GossipMessage serialization");
    let compressed = compress_wire(&wire);
    gossip_sender
        .broadcast(bytes::Bytes::from(compressed))
        .await
        .map_err(|e| FfiError::OperationFailed {
            message: format!("gossip broadcast: {e}"),
        })
}

/// Process a single gossip message (adapted from murmurd for FFI callbacks).
async fn handle_gossip_message(
    content: &[u8],
    engine: &Arc<Mutex<MurmurEngine>>,
    ffi_callbacks: &SharedCallbacks,
    gossip_sender: &iroh_gossip::api::GossipSender,
    our_device_id: DeviceId,
    chunk_buffers: &mut HashMap<BlobHash, ChunkBuffer>,
) {
    let gossip_msg: GossipMessage = match postcard::from_bytes(content) {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "failed to decode gossip message (FFI)");
            return;
        }
    };

    // Sender verification: reject non-DAG messages from unknown devices.
    {
        let eng = engine.lock().unwrap();
        let sender_known = eng.state().devices.contains_key(&gossip_msg.sender);
        let is_dag_payload = matches!(
            gossip_msg.payload,
            GossipPayload::DagEntry { .. }
                | GossipPayload::DagSyncRequest { .. }
                | GossipPayload::DagSyncResponse { .. }
        );
        if !sender_known && !is_dag_payload && !eng.state().devices.is_empty() {
            debug!(sender = %gossip_msg.sender, "dropping gossip from unknown device (FFI)");
            return;
        }
    }

    match gossip_msg.payload {
        GossipPayload::DagEntry { entry_bytes } => {
            match DagEntry::from_bytes(&entry_bytes) {
                Ok(entry) => {
                    let hash_short: String = entry
                        .hash
                        .iter()
                        .take(4)
                        .map(|b| format!("{b:02x}"))
                        .collect();

                    // Check if this is a FileAdded entry — request blob if missing.
                    let blob_to_request = if let Action::FileAdded { ref metadata } = entry.action {
                        let blob_hash = metadata.blob_hash;
                        let hash_bytes = blob_hash.as_bytes().to_vec();
                        match ffi_callbacks.on_blob_needed(hash_bytes) {
                            Some(_) => None, // Already have it.
                            None => Some(blob_hash),
                        }
                    } else {
                        None
                    };

                    {
                        let mut eng = engine.lock().unwrap();
                        match eng.receive_entry(entry) {
                            Ok(_) => {
                                info!(hash = %hash_short, "received dag entry via gossip (FFI)");
                            }
                            Err(e) => {
                                debug!(error = %e, hash = %hash_short, "gossip entry skipped (FFI)");
                            }
                        }
                    }

                    // Request the blob if we don't have it.
                    if let Some(blob_hash) = blob_to_request {
                        let req = GossipMessage {
                            nonce: rand::random(),
                            sender: our_device_id,
                            payload: GossipPayload::BlobRequest { blob_hash },
                        };
                        if let Err(e) = broadcast_gossip(gossip_sender, &req).await {
                            warn!(error = %e, %blob_hash, "blob request broadcast failed (FFI)");
                        } else {
                            debug!(%blob_hash, "requested blob from peers (FFI)");
                        }
                    }
                }
                Err(e) => warn!(error = %e, "invalid dag entry bytes from gossip (FFI)"),
            }
        }
        GossipPayload::MembershipEvent { device_id, online } => {
            debug!(%device_id, online, "membership event (FFI)");
        }
        GossipPayload::DagSyncRequest { tips } => {
            let remote_tips: std::collections::HashSet<[u8; 32]> = tips.into_iter().collect();
            let delta_entries: Vec<Vec<u8>> = {
                let eng = engine.lock().unwrap();
                eng.compute_delta(&remote_tips)
                    .into_iter()
                    .map(|e| e.to_bytes())
                    .collect()
            };

            if delta_entries.is_empty() {
                debug!(sender = %gossip_msg.sender, "sync request: peer is up to date (FFI)");
                return;
            }

            info!(
                sender = %gossip_msg.sender,
                entries = delta_entries.len(),
                "responding to sync request with delta (FFI)"
            );

            let response = GossipMessage {
                nonce: rand::random(),
                sender: our_device_id,
                payload: GossipPayload::DagSyncResponse {
                    entries: delta_entries,
                },
            };
            if let Err(e) = broadcast_gossip(gossip_sender, &response).await {
                warn!(error = %e, "sync response broadcast failed (FFI)");
            }
        }
        GossipPayload::DagSyncResponse { entries } => {
            info!(count = entries.len(), "received sync response (FFI)");
            let mut eng = engine.lock().unwrap();
            for entry_bytes in entries {
                match DagEntry::from_bytes(&entry_bytes) {
                    Ok(entry) => {
                        if let Err(e) = eng.receive_entry(entry) {
                            debug!(error = %e, "sync response entry skipped (FFI)");
                        }
                    }
                    Err(e) => warn!(error = %e, "invalid entry in sync response (FFI)"),
                }
            }
        }
        GossipPayload::BlobRequest { blob_hash } => {
            let hash_bytes = blob_hash.as_bytes().to_vec();
            match ffi_callbacks.on_blob_needed(hash_bytes) {
                Some(data) => {
                    if data.len() <= CHUNK_THRESHOLD {
                        info!(%blob_hash, size = data.len(), "serving blob to peer (FFI)");
                        let response = GossipMessage {
                            nonce: rand::random(),
                            sender: our_device_id,
                            payload: GossipPayload::BlobResponse { blob_hash, data },
                        };
                        if let Err(e) = broadcast_gossip(gossip_sender, &response).await {
                            warn!(error = %e, %blob_hash, "blob response broadcast failed (FFI)");
                        }
                    } else {
                        let total_chunks = data.len().div_ceil(CHUNK_SIZE) as u32;
                        info!(
                            %blob_hash,
                            size = data.len(),
                            total_chunks,
                            "serving blob in chunks (FFI)"
                        );
                        for (i, chunk) in data.chunks(CHUNK_SIZE).enumerate() {
                            let msg = GossipMessage {
                                nonce: rand::random(),
                                sender: our_device_id,
                                payload: GossipPayload::BlobChunk {
                                    blob_hash,
                                    chunk_index: i as u32,
                                    total_chunks,
                                    data: chunk.to_vec(),
                                },
                            };
                            if let Err(e) = broadcast_gossip(gossip_sender, &msg).await {
                                warn!(
                                    error = %e,
                                    %blob_hash,
                                    chunk = i,
                                    "chunk broadcast failed (FFI)"
                                );
                                break;
                            }
                        }
                    }
                }
                None => {
                    debug!(%blob_hash, "peer requested blob we don't have (FFI)");
                }
            }
        }
        GossipPayload::BlobResponse { blob_hash, data } => {
            if data.is_empty() {
                debug!(%blob_hash, "received empty blob response (FFI)");
                return;
            }

            // Verify integrity before storing.
            let actual_hash = BlobHash::from_data(&data);
            if actual_hash != blob_hash {
                warn!(
                    expected = %blob_hash,
                    actual = %actual_hash,
                    "blob integrity check failed, rejecting (FFI)"
                );
                return;
            }

            // Only store if we don't already have it.
            let hash_bytes = blob_hash.as_bytes().to_vec();
            if ffi_callbacks.on_blob_needed(hash_bytes.clone()).is_some() {
                debug!(%blob_hash, "already have this blob, skipping (FFI)");
            } else {
                ffi_callbacks.on_blob_received(hash_bytes, data.clone());
                info!(%blob_hash, size = data.len(), "blob received and stored (FFI)");
            }
        }
        GossipPayload::BlobChunk {
            blob_hash,
            chunk_index,
            total_chunks,
            data,
        } => {
            debug!(
                %blob_hash,
                chunk = chunk_index,
                total = total_chunks,
                size = data.len(),
                "received blob chunk (FFI)"
            );

            let buffer = chunk_buffers
                .entry(blob_hash)
                .or_insert_with(|| ChunkBuffer::new(total_chunks));

            buffer.insert(chunk_index, data);

            if buffer.is_complete() {
                let buffer = chunk_buffers.remove(&blob_hash).unwrap();
                let full_data = buffer.reassemble();

                // Verify full blob hash.
                let actual_hash = BlobHash::from_data(&full_data);
                if actual_hash != blob_hash {
                    warn!(
                        expected = %blob_hash,
                        actual = %actual_hash,
                        "chunked blob integrity check failed (FFI)"
                    );
                    return;
                }

                let hash_bytes = blob_hash.as_bytes().to_vec();
                ffi_callbacks.on_blob_received(hash_bytes, full_data.clone());
                info!(
                    %blob_hash,
                    size = full_data.len(),
                    chunks = total_chunks,
                    "chunked blob reassembled and stored (FFI)"
                );
            }
        }
    }
}
