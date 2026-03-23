//! Gossip-based DAG sync for murmurd.
//!
//! Manages the iroh endpoint, gossip protocol subscription, and background
//! tasks for broadcasting/receiving DAG entries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_lite::StreamExt;
use murmur_dag::DagEntry;
use murmur_net::{CHUNK_SIZE, CHUNK_THRESHOLD, ChunkBuffer, compress_wire, decompress_wire};
use murmur_types::{Action, BlobHash, DeviceId, GossipMessage, GossipPayload};
use rand::TryRng;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::ThrottleConfig;
use crate::metrics;
use crate::storage::Storage;

/// Handle returned from [`start_networking`], used to broadcast entries and
/// query connected peer count.
pub struct NetworkHandle {
    /// Send serialized DAG entry bytes here to broadcast via gossip.
    pub broadcast_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Number of currently connected gossip peers.
    pub connected_peers: Arc<AtomicU64>,
    /// Upload rate limiter.
    #[allow(dead_code)] // Will be wired into blob transfer paths.
    pub upload_throttle: Arc<TokenBucket>,
    /// The iroh endpoint (kept alive for the duration of the daemon).
    endpoint: iroh::Endpoint,
}

impl NetworkHandle {
    /// Gracefully close the iroh endpoint, notifying peers.
    pub async fn close(self) {
        self.endpoint.close().await;
        info!("networking stopped");
    }
}

/// Start the networking layer: iroh endpoint, gossip subscription, and
/// background receive/broadcast tasks.
///
/// - `device_id`: this device's ID (included in gossip messages for sender verification).
/// - `creator_iroh_key_bytes`: 32-byte secret key for the network creator's
///   iroh endpoint. All peers derive the same bytes from the mnemonic.
/// - `is_creator`: whether this device is the network creator.
/// - `topic`: gossip topic derived from the network ID.
pub async fn start_networking(
    engine: Arc<Mutex<murmur_engine::MurmurEngine>>,
    storage: Arc<Storage>,
    device_id: DeviceId,
    creator_iroh_key_bytes: [u8; 32],
    is_creator: bool,
    topic: iroh_gossip::TopicId,
    throttle: ThrottleConfig,
) -> Result<NetworkHandle> {
    // Derive the creator's endpoint ID (all peers can compute this).
    let creator_secret = iroh::SecretKey::from_bytes(&creator_iroh_key_bytes);
    let creator_endpoint_id = creator_secret.public();

    // This device's iroh secret key: creator uses the deterministic key,
    // other devices generate a random key from 32 random bytes.
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
        .context("bind iroh endpoint")?;

    info!(endpoint_id = %endpoint.id(), "iroh endpoint started");

    // Create gossip protocol.
    // Set max gossip message size large enough for 1 MB blob chunks + overhead.
    let gossip = iroh_gossip::Gossip::builder()
        .max_message_size(2 * 1024 * 1024)
        .spawn(endpoint.clone());

    // Accept incoming connections and route to gossip.
    let gossip_for_accept = gossip.clone();
    let ep_for_accept = endpoint.clone();
    tokio::spawn(async move {
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
    });

    // Subscribe to gossip topic. Non-creator devices bootstrap with the
    // creator's endpoint ID.
    let bootstrap = if is_creator {
        vec![]
    } else {
        vec![creator_endpoint_id]
    };

    let topic_handle = gossip
        .subscribe(topic, bootstrap)
        .await
        .context("subscribe to gossip topic")?;
    let (sender, mut receiver) = topic_handle.split();

    info!(?topic, "subscribed to gossip topic");

    // Channel for outgoing entries to broadcast.
    let (broadcast_tx, mut broadcast_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Broadcast task: reads entry bytes from channel, wraps in GossipMessage,
    // compresses, and sends via gossip.
    let sender_for_broadcast = sender.clone();
    let device_id_for_broadcast = device_id;
    tokio::spawn(async move {
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
                warn!(error = %e, "gossip broadcast failed");
            }
        }
    });

    // Connected peer tracking.
    let connected_peers = Arc::new(AtomicU64::new(0));
    let peers_for_recv = connected_peers.clone();

    // Receive task: processes incoming gossip events.
    let engine_for_recv = engine.clone();
    let storage_for_recv = storage.clone();
    let sender_for_recv = sender.clone();
    let device_id_for_recv = device_id;
    tokio::spawn(async move {
        let engine = engine_for_recv;
        let mut chunk_buffers: HashMap<BlobHash, ChunkBuffer> = HashMap::new();
        while let Some(event) = receiver.next().await {
            match event {
                Ok(iroh_gossip::api::Event::Received(msg)) => {
                    let wire_bytes = match decompress_wire(&msg.content) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(error = %e, "failed to decompress gossip message");
                            continue;
                        }
                    };
                    handle_gossip_message(
                        &wire_bytes,
                        &engine,
                        &storage_for_recv,
                        &sender_for_recv,
                        device_id_for_recv,
                        &mut chunk_buffers,
                    )
                    .await;
                }
                Ok(iroh_gossip::api::Event::NeighborUp(id)) => {
                    let count = peers_for_recv.fetch_add(1, Ordering::Relaxed) + 1;
                    metrics::set_connected_peers(count);
                    info!(%id, count, "gossip peer connected");

                    // Send a DagSyncRequest with our tips so peers can compute delta.
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
                        warn!(error = %e, "sync request broadcast failed");
                    }
                    debug!("sent DagSyncRequest to peers");
                }
                Ok(iroh_gossip::api::Event::NeighborDown(id)) => {
                    let count = peers_for_recv.fetch_sub(1, Ordering::Relaxed) - 1;
                    metrics::set_connected_peers(count);
                    info!(%id, count, "gossip peer disconnected");
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "gossip receive error"),
            }
        }
    });

    // On startup, send a DagSyncRequest so existing peers can send us their delta.
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
        debug!(error = %e, "initial sync request failed (no peers yet)");
    }

    // Background push-queue retry task: periodically checks the push queue
    // and broadcasts pending blobs with exponential backoff.
    let storage_for_push = storage.clone();
    let sender_for_push = sender.clone();
    let peers_for_push = connected_peers.clone();
    tokio::spawn(async move {
        const CHECK_INTERVAL_SECS: u64 = 10;
        const BASE_DELAY_SECS: u64 = 5;
        const MAX_DELAY_SECS: u64 = 30 * 60; // 30 minutes

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(CHECK_INTERVAL_SECS)).await;

            // Only attempt if we have connected peers.
            if peers_for_push.load(Ordering::Relaxed) == 0 {
                continue;
            }

            let items = match storage_for_push.push_queue_items() {
                Ok(items) => items,
                Err(e) => {
                    warn!(error = %e, "failed to read push queue");
                    continue;
                }
            };

            if items.is_empty() {
                continue;
            }

            debug!(pending = items.len(), "push queue check");

            for (blob_hash, retry_count) in items {
                // Exponential backoff: skip this item if not enough time has passed.
                // We approximate by only attempting items whose retry_count maps
                // to a delay that has elapsed since the last check interval.
                let delay_secs =
                    (BASE_DELAY_SECS * 2u64.saturating_pow(retry_count)).min(MAX_DELAY_SECS);

                // Only attempt if enough check intervals have passed for this delay.
                // This is approximate — we check every CHECK_INTERVAL_SECS.
                if delay_secs > CHECK_INTERVAL_SECS
                    && retry_count > 0
                    && (retry_count as u64 * CHECK_INTERVAL_SECS) < delay_secs
                {
                    continue;
                }

                let data = match storage_for_push.load_blob(blob_hash) {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        // Blob no longer on disk — remove from queue.
                        let _ = storage_for_push.push_queue_remove(blob_hash);
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, %blob_hash, "push queue: failed to load blob");
                        continue;
                    }
                };

                if data.len() <= CHUNK_THRESHOLD {
                    // Small blob: send as a single response.
                    let msg = GossipMessage {
                        nonce: rand::random(),
                        sender: device_id,
                        payload: GossipPayload::BlobResponse { blob_hash, data },
                    };
                    match broadcast_gossip(&sender_for_push, &msg).await {
                        Ok(()) => {
                            info!(%blob_hash, "push queue: blob broadcast successful, removing");
                            let _ = storage_for_push.push_queue_remove(blob_hash);
                        }
                        Err(e) => {
                            debug!(error = %e, %blob_hash, retry_count, "push queue: broadcast failed, will retry");
                            let _ = storage_for_push.push_queue_increment_retry(blob_hash);
                        }
                    }
                } else {
                    // Large blob: send in chunks, same as BlobRequest handler.
                    let total_chunks = data.len().div_ceil(CHUNK_SIZE) as u32;
                    info!(%blob_hash, size = data.len(), total_chunks, "push queue: sending blob in chunks");
                    let mut all_ok = true;
                    for (i, chunk) in data.chunks(CHUNK_SIZE).enumerate() {
                        let msg = GossipMessage {
                            nonce: rand::random(),
                            sender: device_id,
                            payload: GossipPayload::BlobChunk {
                                blob_hash,
                                chunk_index: i as u32,
                                total_chunks,
                                data: chunk.to_vec(),
                            },
                        };
                        if let Err(e) = broadcast_gossip(&sender_for_push, &msg).await {
                            warn!(error = %e, %blob_hash, chunk = i, "push queue: chunk broadcast failed");
                            all_ok = false;
                            break;
                        }
                    }
                    if all_ok {
                        info!(%blob_hash, "push queue: chunked blob broadcast successful, removing");
                        let _ = storage_for_push.push_queue_remove(blob_hash);
                    } else {
                        let _ = storage_for_push.push_queue_increment_retry(blob_hash);
                    }
                }
            }
        }
    });

    let upload_throttle = Arc::new(TokenBucket::new(throttle.max_upload_bytes_per_sec));

    if throttle.max_upload_bytes_per_sec > 0 {
        info!(
            rate = throttle.max_upload_bytes_per_sec,
            "upload bandwidth throttling enabled"
        );
    }

    Ok(NetworkHandle {
        broadcast_tx,
        connected_peers,
        upload_throttle,
        endpoint,
    })
}

/// Simple token-bucket rate limiter.
#[allow(dead_code)] // Will be wired into blob transfer paths.
pub struct TokenBucket {
    /// Maximum tokens (bytes) per second.
    rate: u64,
    /// Current available tokens.
    tokens: std::sync::atomic::AtomicI64,
    /// Last refill instant.
    last_refill: tokio::sync::Mutex<tokio::time::Instant>,
}

impl TokenBucket {
    /// Create a new token bucket. Rate of 0 means unlimited.
    pub fn new(rate: u64) -> Self {
        Self {
            rate,
            tokens: std::sync::atomic::AtomicI64::new(rate as i64),
            last_refill: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }

    /// Wait until enough tokens are available for `bytes` bytes.
    /// No-op if rate is 0 (unlimited).
    #[allow(dead_code)] // Will be wired into blob transfer paths.
    pub async fn acquire(&self, bytes: usize) {
        if self.rate == 0 {
            return;
        }

        loop {
            // Refill tokens based on elapsed time.
            {
                let mut last = self.last_refill.lock().await;
                let now = tokio::time::Instant::now();
                let elapsed = now.duration_since(*last);
                let refill = (elapsed.as_secs_f64() * self.rate as f64) as i64;
                if refill > 0 {
                    let current = self.tokens.load(Ordering::Relaxed);
                    let new = (current + refill).min(self.rate as i64);
                    self.tokens.store(new, Ordering::Relaxed);
                    *last = now;
                }
            }

            let current = self.tokens.load(Ordering::Relaxed);
            if current >= bytes as i64 {
                self.tokens.fetch_sub(bytes as i64, Ordering::Relaxed);
                return;
            }

            // Wait a short interval before retrying.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// Serialize, compress, and broadcast a GossipMessage.
async fn broadcast_gossip(
    gossip_sender: &iroh_gossip::api::GossipSender,
    msg: &GossipMessage,
) -> std::result::Result<(), anyhow::Error> {
    let wire = postcard::to_allocvec(msg).expect("GossipMessage serialization");
    let compressed = compress_wire(&wire);
    gossip_sender
        .broadcast(bytes::Bytes::from(compressed))
        .await
        .context("gossip broadcast")
}

/// Process a single gossip message.
///
/// Deserializes the [`GossipMessage`] envelope, verifies the sender is a
/// known device (approved or pending), then processes the payload.
async fn handle_gossip_message(
    content: &[u8],
    engine: &Arc<Mutex<murmur_engine::MurmurEngine>>,
    storage: &Arc<Storage>,
    gossip_sender: &iroh_gossip::api::GossipSender,
    our_device_id: DeviceId,
    chunk_buffers: &mut HashMap<BlobHash, ChunkBuffer>,
) {
    let gossip_msg: GossipMessage = match postcard::from_bytes(content) {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "failed to decode gossip message");
            return;
        }
    };

    // Sender verification: reject non-DAG messages from completely unknown devices.
    // DagEntry payloads are always allowed through because they may contain
    // DeviceJoinRequest entries from new devices. The DAG authorization layer
    // provides the definitive check for those. Other payload types (BlobRequest,
    // BlobResponse, etc.) are blocked from unknown senders.
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
            debug!(sender = %gossip_msg.sender, "dropping gossip from unknown device");
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

                    // Check if this is a FileAdded entry — we may need to request the blob.
                    let blob_to_request = if let Action::FileAdded { ref metadata } = entry.action {
                        let blob_hash = metadata.blob_hash;
                        match storage.load_blob(blob_hash) {
                            Ok(Some(_)) => None, // Already have it.
                            _ => Some(blob_hash),
                        }
                    } else {
                        None
                    };

                    {
                        let mut eng = engine.lock().unwrap();
                        match eng.receive_entry(entry) {
                            Ok(_) => {
                                metrics::record_dag_entry();
                                info!(hash = %hash_short, "received dag entry via gossip");
                            }
                            Err(e) => {
                                debug!(error = %e, hash = %hash_short, "gossip entry skipped");
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
                            warn!(error = %e, %blob_hash, "blob request broadcast failed");
                        } else {
                            debug!(%blob_hash, "requested blob from peers");
                        }
                    }
                }
                Err(e) => warn!(error = %e, "invalid dag entry bytes from gossip"),
            }
        }
        GossipPayload::MembershipEvent { device_id, online } => {
            debug!(%device_id, online, "membership event");
        }
        GossipPayload::DagSyncRequest { tips } => {
            // Peer is requesting entries they're missing. Compute delta and respond.
            let remote_tips: std::collections::HashSet<[u8; 32]> = tips.into_iter().collect();
            let delta_entries: Vec<Vec<u8>> = {
                let eng = engine.lock().unwrap();
                eng.compute_delta(&remote_tips)
                    .into_iter()
                    .map(|e| e.to_bytes())
                    .collect()
            };

            if delta_entries.is_empty() {
                debug!(sender = %gossip_msg.sender, "sync request: peer is up to date");
                return;
            }

            info!(
                sender = %gossip_msg.sender,
                entries = delta_entries.len(),
                "responding to sync request with delta"
            );

            let response = GossipMessage {
                nonce: rand::random(),
                sender: our_device_id,
                payload: GossipPayload::DagSyncResponse {
                    entries: delta_entries,
                },
            };
            if let Err(e) = broadcast_gossip(gossip_sender, &response).await {
                warn!(error = %e, "sync response broadcast failed");
            }
        }
        GossipPayload::DagSyncResponse { entries } => {
            // Received a batch of entries from a peer's delta computation.
            info!(count = entries.len(), "received sync response");
            let sync_start = std::time::Instant::now();
            let mut eng = engine.lock().unwrap();
            for entry_bytes in entries {
                match DagEntry::from_bytes(&entry_bytes) {
                    Ok(entry) => {
                        if let Err(e) = eng.receive_entry(entry) {
                            debug!(error = %e, "sync response entry skipped");
                        }
                    }
                    Err(e) => warn!(error = %e, "invalid entry in sync response"),
                }
            }
            metrics::observe_sync_duration(sync_start.elapsed().as_secs_f64());
        }
        GossipPayload::BlobRequest { blob_hash } => {
            // A peer is requesting a blob. If we have it, send it.
            match storage.load_blob(blob_hash) {
                Ok(Some(data)) => {
                    if data.len() <= CHUNK_THRESHOLD {
                        // Small blob: send as a single response.
                        metrics::record_blob_transfer_bytes("upload", data.len() as u64);
                        info!(%blob_hash, size = data.len(), "serving blob to peer");
                        let response = GossipMessage {
                            nonce: rand::random(),
                            sender: our_device_id,
                            payload: GossipPayload::BlobResponse { blob_hash, data },
                        };
                        if let Err(e) = broadcast_gossip(gossip_sender, &response).await {
                            warn!(error = %e, %blob_hash, "blob response broadcast failed");
                        }
                    } else {
                        // Large blob: send in chunks.
                        let total_chunks = data.len().div_ceil(CHUNK_SIZE) as u32;
                        info!(
                            %blob_hash,
                            size = data.len(),
                            total_chunks,
                            "serving blob in chunks"
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
                                    "chunk broadcast failed"
                                );
                                break;
                            }
                        }
                    }
                }
                Ok(None) => {
                    debug!(%blob_hash, "peer requested blob we don't have");
                }
                Err(e) => {
                    warn!(error = %e, %blob_hash, "error loading blob for peer request");
                }
            }
        }
        GossipPayload::BlobResponse { blob_hash, data } => {
            if data.is_empty() {
                debug!(%blob_hash, "received empty blob response");
                return;
            }

            // Verify integrity before storing.
            let actual_hash = BlobHash::from_data(&data);
            if actual_hash != blob_hash {
                warn!(
                    expected = %blob_hash,
                    actual = %actual_hash,
                    "blob integrity check failed, rejecting"
                );
                return;
            }

            // Only store if we don't already have it.
            match storage.load_blob(blob_hash) {
                Ok(Some(_)) => {
                    debug!(%blob_hash, "already have this blob, skipping");
                }
                _ => {
                    if let Err(e) = storage.store_blob(blob_hash, &data) {
                        warn!(error = %e, %blob_hash, "failed to store received blob");
                    } else {
                        metrics::record_blob_transfer_bytes("download", data.len() as u64);
                        info!(%blob_hash, size = data.len(), "blob received and stored");
                    }
                }
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
                "received blob chunk"
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
                        "chunked blob integrity check failed"
                    );
                    return;
                }

                if let Err(e) = storage.store_blob(blob_hash, &full_data) {
                    warn!(error = %e, %blob_hash, "failed to store reassembled blob");
                } else {
                    metrics::record_blob_transfer_bytes("download", full_data.len() as u64);
                    info!(
                        %blob_hash,
                        size = full_data.len(),
                        chunks = total_chunks,
                        "chunked blob reassembled and stored"
                    );
                }
            }
        }
    }
}
