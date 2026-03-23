//! Network layer for Murmur (iroh QUIC + gossip).
//!
//! Provides [`MurmurTransport`] for point-to-point QUIC messaging and blob
//! transfer, and [`GossipHandle`] for epidemic broadcast of DAG entries.

mod error;
mod message;
mod transport;
mod wire;

pub use error::NetError;
pub use message::MurmurMessage;
pub use transport::{GossipHandle, MurmurTransport};
pub use wire::{
    CHUNK_SIZE, CHUNK_THRESHOLD, COMPRESS_THRESHOLD, ChunkBuffer, compress_wire, decompress_wire,
};

/// Re-export low-level message helpers for tests and advanced usage.
pub use transport::{read_message_from_recv, recv_message, send_message, write_message_to_send};

/// Derive the gossip `TopicId` from a network ID.
pub fn topic_from_network_id(network_id: &murmur_types::NetworkId) -> iroh_gossip::TopicId {
    iroh_gossip::TopicId::from_bytes(*blake3::hash(network_id.as_bytes()).as_bytes())
}

/// Derive the QUIC ALPN protocol bytes from a network ID.
pub fn network_alpn(network_id: &murmur_types::NetworkId) -> Vec<u8> {
    let hex_prefix: String = network_id.to_string().chars().take(16).collect();
    format!("murmur/0/{hex_prefix}").into_bytes()
}

/// Maximum message size for length-prefixed wire messages (16 MiB).
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Maximum blob size accepted in a single transfer (256 MiB).
pub const MAX_BLOB_SIZE: usize = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::address_lookup::memory::MemoryLookup;
    use murmur_types::*;

    // --- Message serialization roundtrips ---

    #[test]
    fn test_message_roundtrip_dag_entry_broadcast() {
        let msg = MurmurMessage::DagEntryBroadcast {
            entry_bytes: vec![1, 2, 3, 4],
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_dag_sync_request() {
        let msg = MurmurMessage::DagSyncRequest {
            tips: vec![[0xab; 32], [0xcd; 32]],
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_dag_sync_response() {
        let msg = MurmurMessage::DagSyncResponse {
            entries: vec![vec![10, 20], vec![30, 40]],
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_blob_push() {
        let msg = MurmurMessage::BlobPush {
            blob_hash: BlobHash::from_data(b"hello"),
            data: vec![0xff; 100],
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_blob_push_ack() {
        let msg = MurmurMessage::BlobPushAck {
            blob_hash: BlobHash::from_data(b"hello"),
            ok: true,
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_blob_request() {
        let msg = MurmurMessage::BlobRequest {
            blob_hash: BlobHash::from_data(b"file"),
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_blob_response() {
        let msg = MurmurMessage::BlobResponse {
            blob_hash: BlobHash::from_data(b"file"),
            data: Some(vec![1, 2, 3]),
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_access_request() {
        let msg = MurmurMessage::AccessRequest {
            from: DeviceId::from_data(b"tablet"),
            scope: AccessScope::AllFiles,
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_access_response() {
        let grant = AccessGrant {
            to: DeviceId::from_data(b"tablet"),
            from: DeviceId::from_data(b"phone"),
            scope: AccessScope::AllFiles,
            expires_at: 9999,
            signature_r: [0xab; 32],
            signature_s: [0xcd; 32],
        };
        let msg = MurmurMessage::AccessResponse { grant: Some(grant) };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_ping_pong() {
        for msg in [
            MurmurMessage::Ping { timestamp: 42 },
            MurmurMessage::Pong { timestamp: 42 },
        ] {
            let bytes = msg.to_bytes();
            let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn test_message_from_invalid_bytes() {
        let result = MurmurMessage::from_bytes(&[0xff, 0xff, 0xff]);
        assert!(result.is_err());
    }

    // --- ALPN and topic derivation ---

    #[test]
    fn test_network_alpn_format() {
        let nid = NetworkId::from_data(b"test-network");
        let alpn = network_alpn(&nid);
        let alpn_str = std::str::from_utf8(&alpn).unwrap();
        assert!(alpn_str.starts_with("murmur/0/"));
        assert_eq!(alpn_str.len(), 25); // "murmur/0/" + 16 hex chars
    }

    #[test]
    fn test_network_alpn_deterministic() {
        let nid = NetworkId::from_data(b"test");
        assert_eq!(network_alpn(&nid), network_alpn(&nid));
    }

    #[test]
    fn test_different_networks_different_alpn() {
        let n1 = NetworkId::from_data(b"net-a");
        let n2 = NetworkId::from_data(b"net-b");
        assert_ne!(network_alpn(&n1), network_alpn(&n2));
    }

    #[test]
    fn test_topic_from_network_id_deterministic() {
        let nid = NetworkId::from_data(b"test");
        assert_eq!(topic_from_network_id(&nid), topic_from_network_id(&nid));
    }

    #[test]
    fn test_different_networks_different_topic() {
        let n1 = NetworkId::from_data(b"net-a");
        let n2 = NetworkId::from_data(b"net-b");
        assert_ne!(topic_from_network_id(&n1), topic_from_network_id(&n2));
    }

    #[test]
    fn test_message_roundtrip_blob_response_none() {
        let msg = MurmurMessage::BlobResponse {
            blob_hash: BlobHash::from_data(b"missing"),
            data: None,
        };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_access_response_rejected() {
        let msg = MurmurMessage::AccessResponse { grant: None };
        let bytes = msg.to_bytes();
        let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_roundtrip_access_scope_variants() {
        let scopes = [
            AccessScope::AllFiles,
            AccessScope::FilesByPrefix("photos/".to_string()),
            AccessScope::SingleFile(BlobHash::from_data(b"x")),
        ];
        for scope in scopes {
            let msg = MurmurMessage::AccessRequest {
                from: DeviceId::from_data(b"d"),
                scope,
            };
            let bytes = msg.to_bytes();
            let decoded = MurmurMessage::from_bytes(&bytes).unwrap();
            assert_eq!(msg, decoded);
        }
    }

    // --- Transport integration tests ---

    /// Create a test pair of endpoints that can talk to each other via MemoryLookup.
    async fn create_test_pair() -> (MurmurTransport, MurmurTransport) {
        let nid = NetworkId::from_data(b"test-network");
        let alpn = network_alpn(&nid);
        let disco = MemoryLookup::new();

        let ep1 = iroh::Endpoint::empty_builder()
            .address_lookup(disco.clone())
            .alpns(vec![alpn.clone()])
            .bind()
            .await
            .unwrap();
        let ep2 = iroh::Endpoint::empty_builder()
            .address_lookup(disco.clone())
            .alpns(vec![alpn.clone()])
            .bind()
            .await
            .unwrap();

        disco.add_endpoint_info(ep1.addr());
        disco.add_endpoint_info(ep2.addr());

        let t1 = MurmurTransport::new(ep1, alpn.clone());
        let t2 = MurmurTransport::new(ep2, alpn);
        (t1, t2)
    }

    #[tokio::test]
    #[ignore = "requires netlink (not available in sandboxed CI)"]
    async fn test_two_endpoints_ping_pong() {
        let (t1, t2) = create_test_pair().await;

        let remote_id = t2.endpoint_id();
        let alpn = t1.alpn().to_vec();

        // t1 sends ping to t2.
        let conn = t1.endpoint().connect(remote_id, &alpn).await.unwrap();
        send_message(&conn, &MurmurMessage::Ping { timestamp: 123 })
            .await
            .unwrap();

        // t2 accepts and reads.
        let incoming = t2.endpoint().accept().await.unwrap();
        let conn2 = incoming.accept().unwrap().await.unwrap();
        let msg = recv_message(&conn2).await.unwrap();
        assert_eq!(msg, MurmurMessage::Ping { timestamp: 123 });
    }

    #[tokio::test]
    #[ignore = "requires netlink (not available in sandboxed CI)"]
    async fn test_blob_push_verify_blake3() {
        let (t1, t2) = create_test_pair().await;

        let data = b"hello blob world".to_vec();
        let hash = BlobHash::from_data(&data);

        let remote_id = t2.endpoint_id();
        let alpn = t1.alpn().to_vec();

        // t1 pushes blob.
        let conn = t1.endpoint().connect(remote_id, &alpn).await.unwrap();
        send_message(
            &conn,
            &MurmurMessage::BlobPush {
                blob_hash: hash,
                data: data.clone(),
            },
        )
        .await
        .unwrap();

        // t2 receives and verifies.
        let incoming = t2.endpoint().accept().await.unwrap();
        let conn2 = incoming.accept().unwrap().await.unwrap();
        let msg = recv_message(&conn2).await.unwrap();

        match msg {
            MurmurMessage::BlobPush {
                blob_hash,
                data: received,
            } => {
                assert_eq!(blob_hash, hash);
                let actual_hash = BlobHash::from_data(&received);
                assert_eq!(actual_hash, blob_hash);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_blob_push_corruption_detected() {
        let data = b"hello blob world".to_vec();
        let hash = BlobHash::from_data(&data);

        let mut corrupted = data.clone();
        corrupted[0] ^= 0xff;
        let actual = BlobHash::from_data(&corrupted);
        assert_ne!(actual, hash);
    }

    #[tokio::test]
    #[ignore = "requires netlink (not available in sandboxed CI)"]
    async fn test_request_response_bi_stream() {
        let (t1, t2) = create_test_pair().await;

        let remote_id = t2.endpoint_id();
        let alpn = t1.alpn().to_vec();

        // Spawn responder.
        let responder = tokio::spawn(async move {
            let incoming = t2.endpoint().accept().await.unwrap();
            let conn = incoming.accept().unwrap().await.unwrap();
            let (send, recv) = conn.accept_bi().await.unwrap();
            let request = read_message_from_recv(recv).await.unwrap();
            match request {
                MurmurMessage::DagSyncRequest { tips } => {
                    write_message_to_send(
                        send,
                        &MurmurMessage::DagSyncResponse {
                            entries: tips.iter().map(|t| t.to_vec()).collect(),
                        },
                    )
                    .await
                    .unwrap();
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        // t1 sends request, reads response.
        let conn = t1.endpoint().connect(remote_id, &alpn).await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        write_message_to_send(
            send,
            &MurmurMessage::DagSyncRequest {
                tips: vec![[0x42; 32]],
            },
        )
        .await
        .unwrap();
        let response = read_message_from_recv(recv).await.unwrap();
        match response {
            MurmurMessage::DagSyncResponse { entries } => {
                assert_eq!(entries.len(), 1);
            }
            other => panic!("unexpected: {other:?}"),
        }

        responder.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires netlink (not available in sandboxed CI)"]
    async fn test_gossip_broadcast_receive() {
        let network_id = NetworkId::from_data(b"test-gossip-net");
        let topic = topic_from_network_id(&network_id);
        let disco = MemoryLookup::new();

        let ep1 = iroh::Endpoint::empty_builder()
            .address_lookup(disco.clone())
            .alpns(vec![iroh_gossip::ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let ep2 = iroh::Endpoint::empty_builder()
            .address_lookup(disco.clone())
            .alpns(vec![iroh_gossip::ALPN.to_vec()])
            .bind()
            .await
            .unwrap();

        disco.add_endpoint_info(ep1.addr());
        disco.add_endpoint_info(ep2.addr());

        let gossip1 = iroh_gossip::Gossip::builder().spawn(ep1.clone());
        let gossip2 = iroh_gossip::Gossip::builder().spawn(ep2.clone());

        // Accept gossip connections in background.
        let g1_clone = gossip1.clone();
        let ep1_clone = ep1.clone();
        let accept1 = tokio::spawn(async move {
            loop {
                let Some(incoming) = ep1_clone.accept().await else {
                    break;
                };
                if let Ok(conn) = incoming.accept() {
                    if let Ok(conn) = conn.await {
                        let _ = g1_clone.handle_connection(conn).await;
                    }
                }
            }
        });

        let g2_clone = gossip2.clone();
        let ep2_clone = ep2.clone();
        let accept2 = tokio::spawn(async move {
            loop {
                let Some(incoming) = ep2_clone.accept().await else {
                    break;
                };
                if let Ok(conn) = incoming.accept() {
                    if let Ok(conn) = conn.await {
                        let _ = g2_clone.handle_connection(conn).await;
                    }
                }
            }
        });

        // Subscribe.
        let (sender1, _receiver1) = gossip1
            .subscribe(topic, vec![ep2.id()])
            .await
            .unwrap()
            .split();
        let (_sender2, mut receiver2) = gossip2.subscribe(topic, vec![]).await.unwrap().split();

        // Wait for receiver2 to be joined.
        tokio::time::timeout(std::time::Duration::from_secs(10), receiver2.joined())
            .await
            .unwrap()
            .unwrap();

        // Broadcast a message.
        let payload = b"hello gossip";
        sender1
            .broadcast(bytes::Bytes::from_static(payload))
            .await
            .unwrap();

        // Receive it.
        use futures_lite::StreamExt;
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), receiver2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        match event {
            iroh_gossip::api::Event::Received(msg) => {
                assert_eq!(&msg.content[..], payload);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Cleanup.
        accept1.abort();
        accept2.abort();
        ep1.close().await;
        ep2.close().await;
    }
}
