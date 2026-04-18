//! `murmurd` — Murmur headless backup daemon.
//!
//! Runs as a pure daemon with no subcommands. Management is done via
//! `murmur-cli` which connects over a Unix domain socket.

mod config;
mod crypto;
#[cfg(feature = "metrics")]
mod http;
mod ignore;
mod mdns;
mod metrics;
mod networking;
mod storage;
mod sync;
mod transfers;
mod watcher;

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use murmur_ipc::{
    CliRequest, CliResponse, ConflictInfoIpc, ConflictVersionIpc, DeviceInfoIpc, DevicePresenceIpc,
    EngineEventIpc, FileInfoIpc, FileVersionIpc, FolderConfigIpc, FolderInfoIpc, FolderStorageIpc,
    FolderSubscriberIpc, NetworkFolderInfoIpc, PeerInfoIpc, TransferInfoIpc,
};
use murmur_types::{BlobHash, DeviceId, FolderId, SyncMode};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zeroize::Zeroize;

use config::Config;
use storage::{FjallPlatform, Storage};
use sync::SyncedFolder;
use transfers::TransferTracker;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Murmur headless backup daemon.
///
/// On first run, automatically creates a new network and prints the mnemonic.
/// To join an existing network instead, use `murmur-cli join <mnemonic>` first.
#[derive(Parser)]
#[command(name = "murmurd", about = "Murmur headless backup daemon")]
struct Cli {
    /// Base directory for all murmurd data.
    #[arg(long, default_value_os_t = Config::default_base_dir())]
    data_dir: PathBuf,

    /// Device name (only used on first run).
    #[arg(long, default_value = "murmurd")]
    name: String,

    /// Increase log verbosity (debug level).
    #[arg(long, short)]
    verbose: bool,

    /// Enable HTTP health/metrics endpoint on this port (requires `metrics` feature).
    #[arg(long)]
    http_port: Option<u16>,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .init();

    run_daemon(&cli.data_dir, &cli.name, cli.http_port)
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// Run the daemon: load state, listen on socket, handle signals.
///
/// On first run (no config.toml), automatically creates a new network,
/// generates a mnemonic, writes config, and prints the mnemonic.
fn run_daemon(base_dir: &Path, default_name: &str, http_port: Option<u16>) -> Result<()> {
    // Check for a LeaveNetwork marker from a previous run.
    // If present, wipe the entire data directory and start fresh.
    // Preserve any mnemonic the desktop app pre-wrote for a Join flow.
    let leave_marker = base_dir.join(".leave");
    if leave_marker.exists() {
        info!("LeaveNetwork marker found — wiping data directory");
        let mnemonic_path = Config::mnemonic_path(base_dir);
        let preserved_mnemonic = std::fs::read(&mnemonic_path).ok();
        if let Err(e) = std::fs::remove_dir_all(base_dir) {
            warn!(error = %e, "failed to wipe data dir for LeaveNetwork");
        }
        // Recreate the base dir so auto_init can proceed.
        std::fs::create_dir_all(base_dir).context("recreate base dir after wipe")?;
        // Restore the mnemonic if the desktop app wrote one for a Join flow.
        if let Some(bytes) = preserved_mnemonic {
            std::fs::write(&mnemonic_path, bytes)
                .context("restore pre-written mnemonic after leave wipe")?;
        }
    }

    let config_path = Config::config_path(base_dir);

    // Auto-initialize on first run (or after a wipe).
    let first_run = !config_path.exists();
    if first_run {
        auto_init(base_dir, default_name)?;
    }

    let config = Config::load(&config_path).context("load config")?;

    // Load mnemonic. Zeroize the raw string after parsing.
    let mut mnemonic_str =
        std::fs::read_to_string(Config::mnemonic_path(base_dir)).context("read mnemonic")?;
    let mnemonic = murmur_seed::parse_mnemonic(mnemonic_str.trim())?;
    mnemonic_str.zeroize();
    let identity = murmur_seed::NetworkIdentity::from_mnemonic(&mnemonic, "");

    // Determine device key. Zeroize raw key bytes after use.
    let device_key_path = Config::device_key_path(base_dir);
    let (device_id, signing_key, my_iroh_key) = if device_key_path.exists() {
        let mut raw_bytes = std::fs::read(&device_key_path).context("read device key")?;
        let bytes: [u8; 32] = raw_bytes
            .clone()
            .try_into()
            .map_err(|_| anyhow::anyhow!("device key file must be 32 bytes"))?;
        raw_bytes.zeroize();
        // Derive a stable iroh secret from the signing key bytes so the iroh
        // endpoint ID is identical across restarts. Without this, joiners
        // generate a fresh random iroh key each startup, which breaks peer
        // reconnection whenever one side of the network restarts.
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"murmur/device-iroh-key-v1");
        hasher.update(&bytes);
        let iroh_key = *hasher.finalize().as_bytes();
        let kp = murmur_seed::DeviceKeyPair::from_bytes(bytes);
        (kp.device_id(), kp.signing_key().clone(), iroh_key)
    } else {
        // Creator: mnemonic-derived iroh key, computed below next to network_id.
        (
            identity.first_device_id(),
            identity.first_device_signing_key().clone(),
            identity.creator_iroh_key_bytes(),
        )
    };

    // Check for another running instance BEFORE opening storage.
    let sock_path = murmur_ipc::socket_path(base_dir);
    let pid_path = base_dir.join("murmurd.pid");
    cleanup_stale_instance(&sock_path, &pid_path)?;

    // Write our PID so future launches can detect us.
    std::fs::write(&pid_path, std::process::id().to_string()).context("write PID file")?;

    // Open storage. DAG entries are in a flat file (dag.bin) — resilient
    // to process crashes. Fjall is only used for the transient push queue.
    let mut storage_inner = Storage::open(&config.storage.data_dir, &config.storage.blob_dir)
        .context("open storage")?;
    let blob_enc_key = identity.blob_encryption_key();
    storage_inner.set_blob_encryption_key(&blob_enc_key);
    let storage = Arc::new(storage_inner);

    // Clean up stale streaming temp files from interrupted transfers.
    if let Err(e) = storage.clean_stream_tmp() {
        warn!(error = %e, "failed to clean streaming temp files");
    }

    let platform = Arc::new(FjallPlatform::new(storage.clone()));
    let platform_ref = platform.clone();

    // Create engine.
    //
    // Three cases:
    // 1. Restart (has persisted DAG entries) → load from storage, no new entries.
    // 2. First run as creator (no device.key) → create_network (auto-approves).
    // 3. First run as joiner (has device.key) → join_network (sends join request).
    let persisted_entries = storage.load_all_dag_entries()?;
    let is_joiner = device_key_path.exists();

    // Keep a copy of the signing key alive for the DaemonCtx — the engine
    // variants below consume the original by value.
    let ctx_signing_key = signing_key.clone();

    let engine = if !persisted_entries.is_empty() {
        // Restart: recreate engine from persisted state.
        let dag = murmur_dag::Dag::new(device_id, signing_key);
        let mut engine = murmur_engine::MurmurEngine::from_dag(dag, platform);
        for entry_bytes in &persisted_entries {
            if let Err(e) = engine.load_entry_bytes(entry_bytes) {
                warn!(error = %e, "skip loading dag entry");
            }
        }
        engine.rebuild_conflicts();
        info!(
            entries = persisted_entries.len(),
            conflicts = engine.list_conflicts().len(),
            "loaded persisted DAG entries"
        );
        engine
    } else if is_joiner {
        // First run as a joining device.
        info!("joining existing network — awaiting approval");
        murmur_engine::MurmurEngine::join_network(
            device_id,
            signing_key,
            config.device.name.clone(),
            platform,
        )
    } else {
        // First run as network creator.
        murmur_engine::MurmurEngine::create_network(
            device_id,
            signing_key,
            config.device.name.clone(),
            platform,
        )
    };

    // Verify blob integrity on startup.
    let corrupted = storage.verify_all_blobs()?;
    if !corrupted.is_empty() {
        warn!(
            count = corrupted.len(),
            "corrupted blobs detected — these may need to be re-synced"
        );
    }

    // Set initial metrics from loaded state.
    metrics::set_connected_peers(0);
    if let Ok(items) = storage.push_queue_items() {
        metrics::set_blobs_pending_sync(items.len() as u64);
    }

    info!(%device_id, name = %config.device.name, "daemon started");

    let start_time = Instant::now();

    // Wrap engine in Arc<Mutex> for shared access.
    let engine = Arc::new(Mutex::new(engine));

    // Set up socket listener (stale socket already cleaned above).
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("bind socket at {}", sock_path.display()))?;
    // Capture the raw fd so we can unblock the accept loop on shutdown.
    // The fd remains valid while the listener lives inside the spawn_blocking task.
    use std::os::unix::io::AsRawFd;
    let accept_fd = listener.as_raw_fd();
    info!(path = %sock_path.display(), "listening on socket");

    // Determine if this device is the network creator (first device).
    let is_creator = !Config::device_key_path(base_dir).exists();

    // Use tokio for networking, signal handling, and socket accept loop.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // Start gossip networking.
        let network_id = identity.network_id();
        let topic = murmur_net::topic_from_network_id(&network_id);
        let creator_iroh_key = identity.creator_iroh_key_bytes();

        // Create device_presence before networking so both the gossip receive
        // task and DaemonCtx share the same map.
        let device_presence: Arc<Mutex<std::collections::HashMap<DeviceId, (bool, u64)>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

        let (net_handle, blob_arrived_rx) = networking::start_networking(
            engine.clone(),
            storage.clone(),
            device_id,
            creator_iroh_key,
            my_iroh_key,
            is_creator,
            topic,
            config.network.throttle.clone(),
            device_presence.clone(),
        )
        .await
        .context("start networking")?;

        info!("gossip networking started");

        // Optionally start HTTP health/metrics server.
        #[cfg(feature = "metrics")]
        let _http_task = if let Some(port) = http_port {
            let http_state = http::server::HttpState {
                engine: engine.clone(),
                storage: storage.clone(),
                connected_peers: net_handle.connected_peers.clone(),
                start_time,
            };
            Some(tokio::spawn(http::server::start_http_server(
                http_state, port,
            )))
        } else {
            None
        };
        #[cfg(not(feature = "metrics"))]
        if http_port.is_some() {
            warn!("--http-port requires the `metrics` feature; ignoring");
        }

        // Optionally start mDNS LAN peer discovery.
        let _mdns_handle = if config.network.mdns {
            match mdns::start_mdns(&network_id, 0, net_handle.connected_peers.clone()) {
                Ok(handle) => {
                    info!("mDNS peer discovery enabled");
                    Some(handle)
                }
                Err(e) => {
                    warn!(error = %e, "mDNS startup failed, continuing without it");
                    None
                }
            }
        } else {
            None
        };

        // Set up event broadcast channel for IPC event streaming.
        let (ipc_event_tx, _) = tokio::sync::broadcast::channel::<murmur_engine::EngineEvent>(256);
        let ipc_event_tx_for_forward = ipc_event_tx.clone();

        // M31: live transfer tracker (EWMA speed / ETA). Shared between the
        // event-forwarding task (writer) and `TransferStatus` IPC (reader).
        let transfer_tracker = Arc::new(TransferTracker::new());
        let transfer_tracker_for_events = transfer_tracker.clone();

        // Set up filesystem watching and sync for configured folders.
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        platform_ref.set_event_tx(event_tx);

        let (mut folder_watcher, mut watcher_rx) = watcher::FolderWatcher::new();
        let mut synced_folders = std::collections::HashMap::new();

        for fc in &config.folders {
            let folder_id_bytes = match hex::decode(&fc.folder_id) {
                Ok(b) if b.len() == 32 => {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&b);
                    arr
                }
                _ => {
                    warn!(folder_id = %fc.folder_id, "invalid folder_id hex in config, skipping");
                    continue;
                }
            };
            let folder_id = FolderId::from_bytes(folder_id_bytes);
            let mode = SyncMode::from_str_loose(&fc.mode).unwrap_or(SyncMode::Full);

            // Create local directory if it doesn't exist.
            if let Err(e) = std::fs::create_dir_all(&fc.local_path) {
                warn!(
                    path = %fc.local_path.display(),
                    error = %e,
                    "failed to create folder directory, skipping"
                );
                continue;
            }

            let synced = SyncedFolder {
                folder_id,
                local_path: fc.local_path.clone(),
                mode,
            };

            // Run initial scan.
            {
                let filter = ignore::IgnoreFilter::new(&fc.local_path);
                let mut eng = engine.lock().unwrap();
                if let Err(e) = sync::initial_scan(
                    &synced,
                    &mut eng,
                    &storage,
                    folder_watcher.echo_suppressor(),
                    &filter,
                ) {
                    error!(
                        folder_id = %fc.folder_id,
                        error = %e,
                        "initial scan failed"
                    );
                }
            }

            // Start filesystem watcher.
            if let Err(e) = folder_watcher.watch_folder(folder_id, &fc.local_path) {
                error!(
                    folder_id = %fc.folder_id,
                    error = %e,
                    "failed to start filesystem watcher"
                );
            }

            synced_folders.insert(folder_id, synced);
        }

        // Clone the echo suppressor before wrapping the watcher in a mutex,
        // so the reverse sync task can use it without locking the watcher.
        let echo_suppressor = folder_watcher.echo_suppressor().clone();

        // Spawn debounce poll task (drains ready events every 100ms).
        let watcher_pending = Arc::new(Mutex::new(folder_watcher));
        let watcher_for_poll = watcher_pending.clone();
        let debounce_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                interval.tick().await;
                watcher_for_poll.lock().unwrap().drain_ready();
            }
        });

        // Spawn forward sync task (filesystem events → engine).
        let engine_for_forward = engine.clone();
        let synced_folders_arc = Arc::new(synced_folders);
        let folders_for_forward = synced_folders_arc.clone();
        let forward_sync_task = tokio::spawn(async move {
            while let Some(event) = watcher_rx.recv().await {
                let folders = folders_for_forward.clone();
                let eng = engine_for_forward.clone();
                // Use spawn_blocking to avoid holding the mutex across await.
                tokio::task::spawn_blocking(move || {
                    let mut eng = eng.lock().unwrap();
                    if let Err(e) = sync::handle_forward_sync_event(&event, &folders, &mut eng) {
                        error!(
                            path = %event.relative_path,
                            error = %e,
                            "forward sync error"
                        );
                    }
                });
            }
        });

        // Spawn reverse sync task (engine events → filesystem + IPC event broadcast).
        let storage_for_reverse = storage.clone();
        let folders_for_reverse = synced_folders_arc.clone();
        let echo_for_reverse = echo_suppressor.clone();
        let reverse_sync_task = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                // Forward to IPC event subscribers (best-effort, ignore if no subscribers).
                let _ = ipc_event_tx_for_forward.send(event.clone());

                // M31: feed blob-transfer progress samples into the tracker so
                // `TransferStatus` returns smoothed speed / ETA.
                if let murmur_engine::EngineEvent::BlobTransferProgress {
                    blob_hash,
                    bytes_transferred,
                    total_bytes,
                } = &event
                {
                    transfer_tracker_for_events.record(
                        *blob_hash,
                        *bytes_transferred,
                        *total_bytes,
                    );
                    if *total_bytes > 0 && bytes_transferred >= total_bytes {
                        transfer_tracker_for_events.remove(blob_hash);
                    }
                }
                if let murmur_engine::EngineEvent::BlobReceived { blob_hash } = &event {
                    transfer_tracker_for_events.remove(blob_hash);
                }

                match sync::handle_reverse_sync_event(
                    &event,
                    &folders_for_reverse,
                    &storage_for_reverse,
                    &echo_for_reverse,
                ) {
                    Ok(true) => debug!("reverse sync handled event"),
                    Ok(false) => {} // Not a file event, ignore.
                    Err(e) => error!(error = %e, "reverse sync error"),
                }
            }
        });

        // Spawn blob-arrived task: when the gossip layer stores a new blob from
        // a peer, write any locally-subscribed files that reference that blob.
        // This handles the case where reverse-sync ran before the blob arrived
        // (which is always true for a newly joined device catching up via
        // DagSyncResponse).
        let mut blob_arrived_rx = blob_arrived_rx;
        let engine_for_blob = engine.clone();
        let storage_for_blob = storage.clone();
        let folders_for_blob = synced_folders_arc.clone();
        let echo_for_blob = echo_suppressor.clone();
        let blob_arrived_task = tokio::spawn(async move {
            while let Some(blob_hash) = blob_arrived_rx.recv().await {
                // Collect (folder_id, path) pairs for files that reference this blob
                // and are in a locally-synced folder.
                let pending: Vec<(murmur_types::FolderId, String)> = {
                    let eng = engine_for_blob.lock().unwrap();
                    folders_for_blob
                        .iter()
                        .flat_map(|(fid, _)| {
                            let fid = *fid;
                            eng.folder_files(fid)
                                .into_iter()
                                .filter(move |m| m.blob_hash == blob_hash)
                                .map(move |m| (fid, m.path.clone()))
                        })
                        .collect()
                };

                for (folder_id, path) in pending {
                    if let Some(folder) = folders_for_blob.get(&folder_id) {
                        if !folder.mode.can_receive() {
                            debug!(
                                path = %path,
                                "blob_arrived: skipping reverse sync for send-only folder"
                            );
                            continue;
                        }
                        if let Err(e) = sync::reverse_sync_file(
                            folder,
                            &path,
                            blob_hash,
                            &storage_for_blob,
                            &echo_for_blob,
                        ) {
                            warn!(
                                error = %e,
                                %blob_hash,
                                path = %path,
                                "blob_arrived: failed to write file"
                            );
                        } else {
                            info!(
                                %blob_hash,
                                path = %path,
                                "blob_arrived: wrote file to disk"
                            );
                        }
                    }
                }
            }
        });

        // Spawn a task to accept socket connections.
        let ctx = Arc::new(DaemonCtx {
            engine: engine.clone(),
            storage: storage.clone(),
            device_name: config.device.name.clone(),
            network_id_hex: identity.network_id().to_string(),
            mnemonic: mnemonic.to_string(),
            start_time,
            broadcast_tx: net_handle.broadcast_tx.clone(),
            connected_peers: net_handle.connected_peers.clone(),
            event_broadcast: ipc_event_tx,
            synced_folders: synced_folders_arc.clone(),
            base_dir: base_dir.to_path_buf(),
            endpoint: Some(net_handle.endpoint().clone()),
            config: Arc::new(Mutex::new(config.clone())),
            sync_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            paused_folders: Arc::new(Mutex::new(std::collections::HashSet::new())),
            device_presence: device_presence.clone(),
            used_invite_nonces: Arc::new(Mutex::new(std::collections::HashSet::new())),
            signing_key: Arc::new(ctx_signing_key.clone()),
            device_id,
            transfer_tracker: transfer_tracker.clone(),
        });

        // Spawn conflict-expiry tick (M29). Periodically scans folder
        // configs and auto-resolves conflicts older than their expiry window.
        let ctx_for_expiry = ctx.clone();
        let expiry_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                EXPIRE_CONFLICTS_INTERVAL_SECS,
            ));
            // First tick fires immediately — skip it so we don't resolve on
            // startup before the daemon has a chance to stabilise.
            interval.tick().await;
            loop {
                interval.tick().await;
                let ctx = ctx_for_expiry.clone();
                // Lock-heavy work runs on the blocking pool so the runtime
                // stays responsive under large folder counts.
                let _ = tokio::task::spawn_blocking(move || {
                    expire_conflicts(&ctx);
                })
                .await;
            }
        });

        let accept_handle = tokio::task::spawn_blocking(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let ctx = ctx.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = handle_connection(stream, &ctx) {
                                warn!(error = %e, "socket connection error");
                            }
                        });
                    }
                    Err(e) => {
                        // Listener was likely closed by shutdown.
                        info!(error = %e, "socket listener stopped");
                        break;
                    }
                }
            }
        });

        // Wait for shutdown signal (SIGINT or SIGTERM).
        let shutdown = async {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .context("register SIGTERM handler")?;
                tokio::select! {
                    _ = ctrl_c => info!("received SIGINT"),
                    _ = sigterm.recv() => info!("received SIGTERM"),
                }
            }
            #[cfg(not(unix))]
            {
                ctrl_c.await.context("listen for ctrl-c")?;
                info!("received SIGINT");
            }
            Ok::<(), anyhow::Error>(())
        };

        shutdown.await?;
        info!("shutting down…");

        // Hard exit deadline: if graceful shutdown stalls (blocked mDNS,
        // slow iroh endpoint close, etc.), force-exit after 5 seconds.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(5));
            warn!("graceful shutdown timed out — forcing exit");
            std::process::exit(0);
        });

        // Unblock the IPC accept loop: shutdown the listener socket so the
        // blocking accept() call in spawn_blocking returns an error and exits.
        // SAFETY: accept_fd is valid while the accept_handle task is alive.
        unsafe {
            libc::shutdown(accept_fd, libc::SHUT_RDWR);
        }
        accept_handle.abort();

        // Abort daemon background tasks (debounce poll, sync tasks, blob writes).
        debounce_task.abort();
        forward_sync_task.abort();
        reverse_sync_task.abort();
        blob_arrived_task.abort();
        expiry_task.abort();

        // Abort HTTP server if running.
        #[cfg(feature = "metrics")]
        if let Some(task) = _http_task {
            task.abort();
        }

        // Stop mDNS if running.
        if let Some(handle) = _mdns_handle {
            handle.shutdown();
        }

        // Close the iroh endpoint and abort networking tasks.
        net_handle.close().await;

        Ok::<(), anyhow::Error>(())
    })?;

    // Explicitly shutdown the runtime with a timeout. This is needed because
    // the IPC accept loop uses spawn_blocking, and Runtime::drop would wait
    // indefinitely for blocking tasks to finish.
    rt.shutdown_timeout(std::time::Duration::from_secs(2));

    // Cleanup: drop storage (DAG is already durable via sync_data per write),
    // then remove socket and PID.
    drop(storage);
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pid_path);
    info!("daemon stopped");

    Ok(())
}

// ---------------------------------------------------------------------------
// Socket connection handler
// ---------------------------------------------------------------------------

/// Shared context passed to CLI connection handlers.
struct DaemonCtx {
    engine: Arc<Mutex<murmur_engine::MurmurEngine>>,
    storage: Arc<Storage>,
    device_name: String,
    network_id_hex: String,
    mnemonic: String,
    start_time: Instant,
    broadcast_tx: mpsc::UnboundedSender<Vec<u8>>,
    connected_peers: Arc<std::sync::atomic::AtomicU64>,
    /// Broadcast channel for IPC event streaming.
    event_broadcast: tokio::sync::broadcast::Sender<murmur_engine::EngineEvent>,
    /// Synced folder map (folder_id → local path + mode).
    synced_folders: Arc<std::collections::HashMap<FolderId, SyncedFolder>>,
    /// Base directory for config persistence.
    #[allow(dead_code)]
    base_dir: PathBuf,
    /// Current config reference.
    config: Arc<Mutex<Config>>,
    /// Whether global sync is paused (M20a).
    sync_paused: Arc<std::sync::atomic::AtomicBool>,
    /// Set of paused folder IDs (M24a).
    paused_folders: Arc<Mutex<std::collections::HashSet<FolderId>>>,
    /// Per-device presence (M23a): device_id → (online, last_seen_unix).
    device_presence: Arc<Mutex<std::collections::HashMap<DeviceId, (bool, u64)>>>,
    /// Iroh endpoint for connectivity checks (None only in tests).
    endpoint: Option<iroh::Endpoint>,
    /// Pairing-invite nonces that have already been redeemed on this daemon.
    /// Guards against local replay of the same URL; cross-daemon replay
    /// cannot be detected offline by design — the invite URL is
    /// self-contained.
    used_invite_nonces: Arc<Mutex<std::collections::HashSet<[u8; 32]>>>,
    /// This daemon's signing key — signs pairing invites.
    signing_key: Arc<ed25519_dalek::SigningKey>,
    /// This daemon's device ID — identifies the invite issuer.
    device_id: DeviceId,
    /// Live in-flight transfer stats with smoothed speed / ETA (M31).
    transfer_tracker: Arc<TransferTracker>,
}

/// Handle a single CLI connection.
fn handle_connection(mut stream: std::os::unix::net::UnixStream, ctx: &DaemonCtx) -> Result<()> {
    let request: CliRequest = murmur_ipc::recv_message(&mut stream)?;
    info!(?request, "received CLI request");

    // Event streaming is a special case: keep the connection open.
    if matches!(request, CliRequest::SubscribeEvents) {
        handle_event_stream(stream, ctx);
        return Ok(());
    }

    let response = process_request(request, ctx);

    murmur_ipc::send_message(&mut stream, &response)?;
    Ok(())
}

/// Handle a long-lived event stream connection.
///
/// Subscribes to the engine event broadcast channel and sends each event
/// as a `CliResponse::Event` message over the connection until the client
/// disconnects or an error occurs.
fn handle_event_stream(mut stream: std::os::unix::net::UnixStream, ctx: &DaemonCtx) {
    let mut rx = ctx.event_broadcast.subscribe();
    info!("client subscribed to event stream");

    loop {
        match rx.blocking_recv() {
            Ok(event) => {
                let ipc_event = engine_event_to_ipc(&event, ctx);
                let response = CliResponse::Event { event: ipc_event };
                if murmur_ipc::send_message(&mut stream, &response).is_err() {
                    debug!("event stream client disconnected");
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "event stream subscriber lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                debug!("event broadcast channel closed");
                break;
            }
        }
    }
}

/// Process a CLI request and produce a response.
fn process_request(request: CliRequest, ctx: &DaemonCtx) -> CliResponse {
    match request {
        CliRequest::Status => {
            let eng = ctx.engine.lock().unwrap();
            let gossip_peers = ctx.connected_peers.load(Ordering::Relaxed);
            CliResponse::Status {
                device_id: eng.device_id().to_string(),
                device_name: ctx.device_name.clone(),
                network_id: ctx.network_id_hex.clone(),
                peer_count: gossip_peers,
                dag_entries: eng.all_entries().len() as u64,
                uptime_secs: ctx.start_time.elapsed().as_secs(),
            }
        }
        CliRequest::ListDevices => {
            let eng = ctx.engine.lock().unwrap();
            let devices = eng
                .list_devices()
                .into_iter()
                .filter(|d| d.approved)
                .map(device_to_ipc)
                .collect();
            CliResponse::Devices { devices }
        }
        CliRequest::ListPending => {
            let eng = ctx.engine.lock().unwrap();
            let devices = eng
                .pending_requests()
                .into_iter()
                .map(device_to_ipc)
                .collect();
            CliResponse::Pending { devices }
        }
        CliRequest::ApproveDevice { device_id_hex } => {
            let device_id = match parse_device_id(&device_id_hex) {
                Ok(id) => id,
                Err(e) => {
                    return CliResponse::Error {
                        message: format!("{e:#}"),
                    };
                }
            };
            let mut eng = ctx.engine.lock().unwrap();
            match eng.approve_device(device_id) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after approve");
                    }
                    CliResponse::Ok {
                        message: format!("Device {device_id} approved."),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }
        CliRequest::RevokeDevice { device_id_hex } => {
            let device_id = match parse_device_id(&device_id_hex) {
                Ok(id) => id,
                Err(e) => {
                    return CliResponse::Error {
                        message: format!("{e:#}"),
                    };
                }
            };
            let mut eng = ctx.engine.lock().unwrap();
            match eng.revoke_device(device_id) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after revoke");
                    }
                    CliResponse::Ok {
                        message: format!("Device {device_id} revoked."),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }
        CliRequest::ShowMnemonic => CliResponse::Mnemonic {
            mnemonic: ctx.mnemonic.clone(),
        },
        CliRequest::ListFiles => {
            let eng = ctx.engine.lock().unwrap();
            let files = eng
                .state()
                .files
                .iter()
                .map(|((folder_id, _path), meta)| FileInfoIpc {
                    blob_hash: meta.blob_hash.to_string(),
                    folder_id: folder_id.to_string(),
                    path: meta.path.clone(),
                    size: meta.size,
                    mime_type: meta.mime_type.clone(),
                    device_origin: meta.device_origin.to_string(),
                })
                .collect();
            CliResponse::Files { files }
        }
        CliRequest::TransferStatus => {
            let items = match ctx.storage.push_queue_items() {
                Ok(items) => items,
                Err(e) => {
                    return CliResponse::Error {
                        message: format!("read push queue: {e}"),
                    };
                }
            };
            // Merge pending push-queue entries (we know total size but not
            // progress) with the live tracker (smoothed progress samples from
            // BlobTransferProgress events).
            let transfers = items
                .into_iter()
                .map(|(blob_hash, _retry_count)| {
                    let total_bytes = match ctx.storage.load_blob(blob_hash) {
                        Ok(Some(data)) => data.len() as u64,
                        _ => 0,
                    };
                    let stats = ctx.transfer_tracker.get(&blob_hash);
                    let (
                        bytes_transferred,
                        started_at_unix,
                        last_progress_unix,
                        bytes_per_sec_smoothed,
                        eta_seconds,
                        total,
                    ) = match stats {
                        Some(s) => (
                            s.bytes_transferred,
                            s.started_at_unix,
                            s.last_progress_unix,
                            s.bytes_per_sec_smoothed.round() as u64,
                            s.eta_seconds(),
                            if s.total_bytes > 0 {
                                s.total_bytes
                            } else {
                                total_bytes
                            },
                        ),
                        None => (0, 0, 0, 0, None, total_bytes),
                    };
                    TransferInfoIpc {
                        blob_hash: blob_hash.to_string(),
                        bytes_transferred,
                        total_bytes: total,
                        started_at_unix,
                        last_progress_unix,
                        bytes_per_sec_smoothed,
                        eta_seconds,
                    }
                })
                .collect();
            CliResponse::TransferStatus { transfers }
        }
        CliRequest::AddFile { path } => process_add_file(path, ctx),

        // -- Folder management (M17) --
        CliRequest::CreateFolder {
            name,
            local_path,
            ignore_patterns,
        } => {
            let mut eng = ctx.engine.lock().unwrap();
            match eng.create_folder(&name) {
                Ok((folder, entries)) => {
                    for entry in &entries {
                        let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    }

                    // If a local path was provided, register it in config and
                    // scan existing files on disk into the DAG.
                    if let Some(ref lp) = local_path {
                        let local = PathBuf::from(lp);
                        // Save to config.
                        {
                            let mut cfg = ctx.config.lock().unwrap();
                            cfg.folders.push(config::FolderConfig {
                                folder_id: folder.folder_id.to_string(),
                                name: folder.name.clone(),
                                local_path: local.clone(),
                                mode: SyncMode::Full.as_str().to_string(),
                                auto_resolve: "none".to_string(),
                                conflict_expiry_days: None,
                                color_hex: None,
                                icon: None,
                            });
                            persist_config(ctx, &cfg);
                        }
                        // Write folder-template ignore patterns if provided.
                        if let Some(ref patterns) = ignore_patterns {
                            if let Err(e) = std::fs::create_dir_all(&local) {
                                warn!(
                                    path = %local.display(),
                                    error = %e,
                                    "failed to create folder dir for .murmurignore"
                                );
                            } else if let Err(e) =
                                std::fs::write(local.join(".murmurignore"), patterns)
                            {
                                warn!(
                                    path = %local.display(),
                                    error = %e,
                                    "failed to write .murmurignore from template"
                                );
                            }
                        }
                        // Scan existing files on disk.
                        if local.is_dir() {
                            let device_id = eng.device_id();
                            let folder_id = folder.folder_id;
                            let mut added = 0u64;
                            if let Ok(walker) = std::fs::read_dir(&local) {
                                for entry in walker.flatten() {
                                    let path = entry.path();
                                    if !path.is_file() {
                                        continue;
                                    }
                                    let filename = match path.file_name() {
                                        Some(n) => n.to_string_lossy().to_string(),
                                        None => continue,
                                    };
                                    let data = match std::fs::read(&path) {
                                        Ok(d) => d,
                                        Err(_) => continue,
                                    };
                                    let blob_hash = murmur_types::BlobHash::from_data(&data);
                                    let size = data.len() as u64;
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs();
                                    let meta = murmur_types::FileMetadata {
                                        blob_hash,
                                        folder_id,
                                        path: filename,
                                        size,
                                        mime_type: guess_mime(&path.to_string_lossy()),
                                        created_at: now,
                                        modified_at: now,
                                        device_origin: device_id,
                                    };
                                    if eng.add_file(meta, data).is_ok() {
                                        added += 1;
                                    }
                                }
                            }
                            info!(%added, path = %local.display(), "scanned existing files");
                        }
                    }

                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after create_folder");
                    }
                    CliResponse::Ok {
                        message: format!("Folder created: {} ({})", folder.name, folder.folder_id),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }
        CliRequest::RemoveFolder { folder_id_hex } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let mut eng = ctx.engine.lock().unwrap();
            match eng.remove_folder(folder_id) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after remove_folder");
                    }
                    // Remove the folder mapping from local config.
                    {
                        let mut cfg = ctx.config.lock().unwrap();
                        let before = cfg.folders.len();
                        cfg.folders.retain(|f| f.folder_id != folder_id_hex);
                        if cfg.folders.len() != before {
                            persist_config(ctx, &cfg);
                        }
                    }
                    CliResponse::Ok {
                        message: format!("Folder {folder_id} removed."),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }
        CliRequest::ListFolders => {
            let eng = ctx.engine.lock().unwrap();
            let device_id = eng.device_id();
            let cfg = ctx.config.lock().unwrap();
            let paused_set = ctx.paused_folders.lock().unwrap();
            let global_paused = ctx.sync_paused.load(std::sync::atomic::Ordering::Relaxed);
            let folders = eng
                .list_folders()
                .into_iter()
                .map(|f| {
                    let fid_str = f.folder_id.to_string();
                    let file_count = eng.folder_files(f.folder_id).len() as u64;
                    let sub = eng
                        .folder_subscriptions(f.folder_id)
                        .into_iter()
                        .find(|s| s.device_id == device_id);
                    let fc = cfg.folders.iter().find(|fc| fc.folder_id == fid_str);
                    // Prefer the config name (user may have renamed locally).
                    let name = fc
                        .and_then(|fc| {
                            if fc.name.is_empty() {
                                None
                            } else {
                                Some(fc.name.clone())
                            }
                        })
                        .unwrap_or(f.name);
                    let local_path = fc.map(|fc| fc.local_path.to_string_lossy().to_string());
                    let sync_status = if sub.is_none() {
                        "Not subscribed".to_string()
                    } else if global_paused || paused_set.contains(&f.folder_id) {
                        "Paused".to_string()
                    } else {
                        let conflicts = eng.list_conflicts_in_folder(f.folder_id).len();
                        if conflicts > 0 {
                            format!(
                                "{conflicts} conflict{}",
                                if conflicts == 1 { "" } else { "s" }
                            )
                        } else {
                            "Up to date".to_string()
                        }
                    };
                    let (color_hex, icon) = fc
                        .map(|fc| (fc.color_hex.clone(), fc.icon.clone()))
                        .unwrap_or((None, None));
                    FolderInfoIpc {
                        folder_id: fid_str,
                        name,
                        created_by: f.created_by.to_string(),
                        file_count,
                        subscribed: sub.is_some(),
                        mode: sub.map(|s| s.mode.as_str().to_string()),
                        local_path,
                        sync_status,
                        color_hex,
                        icon,
                    }
                })
                .collect();
            CliResponse::Folders { folders }
        }
        CliRequest::SubscribeFolder {
            folder_id_hex,
            name,
            local_path,
            mode,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let sync_mode = SyncMode::from_str_loose(&mode).unwrap_or(SyncMode::Full);
            let mut eng = ctx.engine.lock().unwrap();
            // Resolve the display name: explicit name > folder's original name > folder_id.
            let folder_name = name
                .or_else(|| {
                    eng.list_folders()
                        .into_iter()
                        .find(|f| f.folder_id == folder_id)
                        .map(|f| f.name)
                })
                .unwrap_or_else(|| folder_id_hex.clone());
            match eng.subscribe_folder(folder_id, sync_mode) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after subscribe_folder");
                    }
                    // Persist the folder mapping in config.
                    {
                        let mut cfg = ctx.config.lock().unwrap();
                        // Avoid duplicates — update if already present.
                        if let Some(fc) = cfg
                            .folders
                            .iter_mut()
                            .find(|f| f.folder_id == folder_id_hex)
                        {
                            fc.name = folder_name.clone();
                            fc.local_path = PathBuf::from(&local_path);
                            fc.mode = mode.clone();
                        } else {
                            cfg.folders.push(config::FolderConfig {
                                folder_id: folder_id_hex.clone(),
                                name: folder_name.clone(),
                                local_path: PathBuf::from(&local_path),
                                mode: mode.clone(),
                                auto_resolve: "none".to_string(),
                                conflict_expiry_days: None,
                                color_hex: None,
                                icon: None,
                            });
                        }
                        persist_config(ctx, &cfg);
                    }
                    CliResponse::Ok {
                        message: format!(
                            "Subscribed to folder {folder_name} ({folder_id}) as {sync_mode}."
                        ),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }
        CliRequest::UnsubscribeFolder {
            folder_id_hex,
            keep_local: _,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let mut eng = ctx.engine.lock().unwrap();
            match eng.unsubscribe_folder(folder_id) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after unsubscribe_folder");
                    }
                    // Remove the folder mapping from local config.
                    {
                        let mut cfg = ctx.config.lock().unwrap();
                        let before = cfg.folders.len();
                        cfg.folders.retain(|f| f.folder_id != folder_id_hex);
                        if cfg.folders.len() != before {
                            persist_config(ctx, &cfg);
                        }
                    }
                    CliResponse::Ok {
                        message: format!("Unsubscribed from folder {folder_id}."),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }
        CliRequest::FolderFiles { folder_id_hex } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let eng = ctx.engine.lock().unwrap();
            let files = eng
                .folder_files(folder_id)
                .into_iter()
                .map(|meta| FileInfoIpc {
                    blob_hash: meta.blob_hash.to_string(),
                    folder_id: meta.folder_id.to_string(),
                    path: meta.path.clone(),
                    size: meta.size,
                    mime_type: meta.mime_type.clone(),
                    device_origin: meta.device_origin.to_string(),
                })
                .collect();
            CliResponse::Files { files }
        }
        CliRequest::FolderStatus { folder_id_hex } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let eng = ctx.engine.lock().unwrap();
            let folder = eng
                .list_folders()
                .into_iter()
                .find(|f| f.folder_id == folder_id);
            match folder {
                Some(f) => {
                    let file_count = eng.folder_files(folder_id).len() as u64;
                    let conflict_count = eng.list_conflicts_in_folder(folder_id).len() as u64;
                    let sync_status = if conflict_count > 0 {
                        "conflicts".to_string()
                    } else if file_count > 0 {
                        "synced".to_string()
                    } else {
                        "empty".to_string()
                    };
                    CliResponse::FolderStatus {
                        folder_id: folder_id.to_string(),
                        name: f.name,
                        file_count,
                        conflict_count,
                        sync_status,
                    }
                }
                None => CliResponse::Error {
                    message: format!("folder not found: {folder_id_hex}"),
                },
            }
        }
        CliRequest::ListConflicts { folder_id_hex } => {
            let folder_filter = match folder_id_hex {
                Some(hex) => match parse_folder_id(&hex) {
                    Ok(id) => Some(id),
                    Err(e) => return CliResponse::Error { message: e },
                },
                None => None,
            };
            let eng = ctx.engine.lock().unwrap();
            let conflicts = build_conflict_list(&eng, folder_filter);
            CliResponse::Conflicts { conflicts }
        }
        CliRequest::ResolveConflict {
            folder_id_hex,
            path,
            chosen_hash_hex,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let chosen_hash = match parse_blob_hash(&chosen_hash_hex) {
                Ok(h) => h,
                Err(e) => return CliResponse::Error { message: e },
            };
            let mut eng = ctx.engine.lock().unwrap();
            match eng.resolve_conflict(folder_id, &path, chosen_hash) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after resolve_conflict");
                    }
                    CliResponse::Ok {
                        message: format!("Conflict resolved for {path}."),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }
        CliRequest::FileHistory {
            folder_id_hex,
            path,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let eng = ctx.engine.lock().unwrap();
            let versions = eng
                .file_history(folder_id, &path)
                .into_iter()
                .map(|(blob_hash, hlc)| {
                    let (device_id_str, device_name, size) =
                        find_version_info(&eng, folder_id, &path, blob_hash, hlc);
                    FileVersionIpc {
                        blob_hash: blob_hash.to_string(),
                        device_id: device_id_str,
                        device_name,
                        modified_at: hlc,
                        size,
                    }
                })
                .collect();
            CliResponse::FileVersions { versions }
        }
        CliRequest::SetFolderMode {
            folder_id_hex,
            mode,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let sync_mode = match SyncMode::from_str_loose(&mode) {
                Some(m) => m,
                None => {
                    return CliResponse::Error {
                        message: format!(
                            "unknown mode: {mode:?} (use full, send-only, or receive-only)"
                        ),
                    };
                }
            };
            // Unsubscribe then re-subscribe with new mode.
            let mut eng = ctx.engine.lock().unwrap();
            if let Err(e) = eng.unsubscribe_folder(folder_id) {
                return CliResponse::Error {
                    message: format!("unsubscribe: {e}"),
                };
            }
            match eng.subscribe_folder(folder_id, sync_mode) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after set_folder_mode");
                    }
                    CliResponse::Ok {
                        message: format!("Folder {folder_id} mode set to {sync_mode}."),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }
        CliRequest::BlobPreview {
            blob_hash_hex,
            max_bytes,
        } => {
            let blob_hash = match parse_blob_hash(&blob_hash_hex) {
                Ok(h) => h,
                Err(e) => return CliResponse::Error { message: e },
            };
            match ctx.storage.load_blob(blob_hash) {
                Ok(Some(data)) => {
                    let limit = max_bytes as usize;
                    let truncated = if data.len() > limit {
                        data[..limit].to_vec()
                    } else {
                        data
                    };
                    CliResponse::BlobData { data: truncated }
                }
                Ok(None) => CliResponse::Error {
                    message: format!("blob not found: {blob_hash_hex}"),
                },
                Err(e) => CliResponse::Error {
                    message: format!("load blob: {e}"),
                },
            }
        }
        CliRequest::RestoreFileVersion {
            folder_id_hex,
            path,
            blob_hash_hex,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let blob_hash = match parse_blob_hash(&blob_hash_hex) {
                Ok(h) => h,
                Err(e) => return CliResponse::Error { message: e },
            };
            // Look up the local path for this folder.
            let synced = match ctx.synced_folders.get(&folder_id) {
                Some(s) => s,
                None => {
                    return CliResponse::Error {
                        message: format!("folder not subscribed locally: {folder_id_hex}"),
                    };
                }
            };
            // Load the blob data.
            let data = match ctx.storage.load_blob(blob_hash) {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return CliResponse::Error {
                        message: format!("blob not found: {blob_hash_hex}"),
                    };
                }
                Err(e) => {
                    return CliResponse::Error {
                        message: format!("load blob: {e}"),
                    };
                }
            };
            // Write the blob to the file's local path. The filesystem watcher
            // will pick up the change and emit a new FileModified DAG entry.
            let file_path = synced.local_path.join(&path);
            if let Some(parent) = file_path.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return CliResponse::Error {
                    message: format!("create parent dirs: {e}"),
                };
            }
            match std::fs::write(&file_path, &data) {
                Ok(()) => CliResponse::Ok {
                    message: format!("Restored {path} to version {blob_hash_hex}."),
                },
                Err(e) => CliResponse::Error {
                    message: format!("write file: {e}"),
                },
            }
        }
        // SubscribeEvents is handled in handle_connection before we get here.
        CliRequest::SubscribeEvents => CliResponse::Error {
            message: "event streaming handled elsewhere".to_string(),
        },

        // -- M19a: Zero-Config Onboarding --
        CliRequest::GetConfig => {
            let cfg = ctx.config.lock().unwrap();
            let folders = cfg
                .folders
                .iter()
                .map(|f| FolderConfigIpc {
                    folder_id: f.folder_id.clone(),
                    name: f.name.clone(),
                    local_path: f.local_path.to_string_lossy().to_string(),
                    mode: f.mode.clone(),
                    auto_resolve: f.auto_resolve.clone(),
                    conflict_expiry_days: f.conflict_expiry_days,
                    color_hex: f.color_hex.clone(),
                    icon: f.icon.clone(),
                })
                .collect();
            CliResponse::Config {
                device_name: cfg.device.name.clone(),
                network_id: ctx.network_id_hex.clone(),
                folders,
                auto_approve: cfg.network.auto_approve,
                mdns: cfg.network.mdns,
                upload_throttle: cfg.network.throttle.max_upload_bytes_per_sec,
                download_throttle: cfg.network.throttle.max_download_bytes_per_sec,
                sync_paused: ctx.sync_paused.load(Ordering::Relaxed),
            }
        }
        CliRequest::InitDefaultFolder { local_path: _ } => {
            let mut eng = ctx.engine.lock().unwrap();
            // Check if a folder named "Murmur" already exists (idempotent).
            let existing = eng.list_folders().into_iter().find(|f| f.name == "Murmur");
            if let Some(folder) = existing {
                CliResponse::Ok {
                    message: format!(
                        "Default folder already exists: {} ({})",
                        folder.name, folder.folder_id
                    ),
                }
            } else {
                match eng.create_folder("Murmur") {
                    Ok((folder, entries)) => {
                        for entry in &entries {
                            let _ = ctx.broadcast_tx.send(entry.to_bytes());
                        }
                        if let Err(e) = ctx.storage.flush() {
                            error!(error = %e, "flush after init_default_folder");
                        }
                        CliResponse::Ok {
                            message: format!(
                                "Default folder created: {} ({})",
                                folder.name, folder.folder_id
                            ),
                        }
                    }
                    Err(e) => CliResponse::Error {
                        message: format!("{e}"),
                    },
                }
            }
        }

        // -- M20a: System Tray --
        CliRequest::PauseSync => {
            ctx.sync_paused.store(true, Ordering::Relaxed);
            CliResponse::Ok {
                message: "Sync paused globally.".to_string(),
            }
        }
        CliRequest::ResumeSync => {
            ctx.sync_paused.store(false, Ordering::Relaxed);
            CliResponse::Ok {
                message: "Sync resumed globally.".to_string(),
            }
        }

        // -- M21a: Folder Discovery --
        CliRequest::ListNetworkFolders => {
            let eng = ctx.engine.lock().unwrap();
            let device_id = eng.device_id();
            let folders = eng
                .list_folders()
                .into_iter()
                .filter_map(|f| {
                    let file_count = eng.folder_files(f.folder_id).len() as u64;
                    let subs = eng.folder_subscriptions(f.folder_id);
                    let subscriber_count = subs.len() as u64;
                    let my_sub = subs.iter().find(|s| s.device_id == device_id);
                    // Only include folders we're subscribed to, or that have
                    // at least one other subscriber (i.e. actually on the network).
                    if my_sub.is_none() && subscriber_count == 0 {
                        return None;
                    }
                    Some(NetworkFolderInfoIpc {
                        folder_id: f.folder_id.to_string(),
                        name: f.name,
                        created_by: f.created_by.to_string(),
                        file_count,
                        subscriber_count,
                        subscribed: my_sub.is_some(),
                        mode: my_sub.map(|s| s.mode.as_str().to_string()),
                    })
                })
                .collect();
            CliResponse::NetworkFolders { folders }
        }
        CliRequest::FolderSubscribers { folder_id_hex } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let eng = ctx.engine.lock().unwrap();
            let state = eng.state();
            let subscribers = eng
                .folder_subscriptions(folder_id)
                .into_iter()
                .map(|sub| {
                    let device_name = state
                        .devices
                        .get(&sub.device_id)
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    FolderSubscriberIpc {
                        device_id: sub.device_id.to_string(),
                        device_name,
                        mode: sub.mode.as_str().to_string(),
                    }
                })
                .collect();
            CliResponse::FolderSubscriberList { subscribers }
        }

        // -- M22a: Rich Conflict Resolution --
        CliRequest::BulkResolveConflicts {
            folder_id_hex,
            strategy,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let mut eng = ctx.engine.lock().unwrap();
            let conflicts = eng.list_conflicts_in_folder(folder_id);
            let device_id = eng.device_id();
            let mut resolved = 0u64;
            for conflict in &conflicts {
                let chosen = match strategy.as_str() {
                    "keep_newest" => conflict
                        .versions
                        .iter()
                        .max_by_key(|v| v.hlc)
                        .map(|v| v.blob_hash),
                    "keep_local" => conflict
                        .versions
                        .iter()
                        .find(|v| v.device_id == device_id)
                        .map(|v| v.blob_hash),
                    "keep_remote" => conflict
                        .versions
                        .iter()
                        .find(|v| v.device_id != device_id)
                        .map(|v| v.blob_hash),
                    _ => None,
                };
                if let Some(chosen_hash) = chosen
                    && eng
                        .resolve_conflict(folder_id, &conflict.path, chosen_hash)
                        .is_ok()
                {
                    resolved += 1;
                }
            }
            if let Err(e) = ctx.storage.flush() {
                error!(error = %e, "flush after bulk_resolve");
            }
            CliResponse::Ok {
                message: format!("Resolved {resolved} conflicts with strategy '{strategy}'."),
            }
        }
        CliRequest::SetFolderAutoResolve {
            folder_id_hex,
            strategy,
        } => {
            // Validate strategy up-front so typos surface as errors, not a
            // silent no-op on the next expiry tick.
            if !matches!(strategy.as_str(), "none" | "newest" | "mine") {
                return CliResponse::Error {
                    message: format!(
                        "invalid auto-resolve strategy '{strategy}' (expected: none, newest, mine)"
                    ),
                };
            }
            let mut cfg = ctx.config.lock().unwrap();
            match cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
            {
                Some(fc) => {
                    fc.auto_resolve = strategy.clone();
                    persist_config(ctx, &cfg);
                    CliResponse::Ok {
                        message: format!("Auto-resolve set to '{strategy}'."),
                    }
                }
                None => CliResponse::Error {
                    message: format!("folder not in local config: {folder_id_hex}"),
                },
            }
        }
        CliRequest::DismissConflict {
            folder_id_hex,
            path,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            // Remove the conflict from the active list without creating a DAG
            // entry. Both files remain on disk.
            let mut eng = ctx.engine.lock().unwrap();
            let state = eng.state_mut();
            state
                .conflicts
                .retain(|c| !(c.folder_id == folder_id && c.path == path));
            CliResponse::Ok {
                message: format!("Conflict dismissed for {path}."),
            }
        }

        // -- M29a: Conflict Resolution Improvements --
        CliRequest::ConflictDiff {
            folder_id_hex,
            path,
        } => handle_conflict_diff(ctx, &folder_id_hex, &path),
        CliRequest::SetConflictExpiry {
            folder_id_hex,
            days,
        } => {
            let mut cfg = ctx.config.lock().unwrap();
            match cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
            {
                Some(fc) => {
                    // `0` is the UX-friendly way to disable expiry from the CLI.
                    fc.conflict_expiry_days = if days == 0 { None } else { Some(days) };
                    persist_config(ctx, &cfg);
                    CliResponse::Ok {
                        message: if days == 0 {
                            "Conflict expiry disabled.".to_string()
                        } else {
                            format!("Conflict expiry set to {days} days.")
                        },
                    }
                }
                None => CliResponse::Error {
                    message: format!("folder not in local config: {folder_id_hex}"),
                },
            }
        }

        // -- M31: Sync Progress & Desktop UX Polish --
        CliRequest::SetFolderColor {
            folder_id_hex,
            color_hex,
        } => {
            let mut cfg = ctx.config.lock().unwrap();
            match cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
            {
                Some(fc) => {
                    fc.color_hex = color_hex;
                    persist_config(ctx, &cfg);
                    CliResponse::Ok {
                        message: "Folder color updated.".to_string(),
                    }
                }
                None => CliResponse::Error {
                    message: format!("folder not in local config: {folder_id_hex}"),
                },
            }
        }
        CliRequest::SetFolderIcon {
            folder_id_hex,
            icon,
        } => {
            let mut cfg = ctx.config.lock().unwrap();
            match cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
            {
                Some(fc) => {
                    fc.icon = icon;
                    persist_config(ctx, &cfg);
                    CliResponse::Ok {
                        message: "Folder icon updated.".to_string(),
                    }
                }
                None => CliResponse::Error {
                    message: format!("folder not in local config: {folder_id_hex}"),
                },
            }
        }
        CliRequest::GetNotificationSettings => {
            let cfg = ctx.config.lock().unwrap();
            CliResponse::NotificationSettings {
                settings: murmur_ipc::NotificationSettingsIpc {
                    conflict: cfg.notifications.conflict,
                    transfer_completed: cfg.notifications.transfer_completed,
                    device_joined: cfg.notifications.device_joined,
                    error: cfg.notifications.error,
                },
            }
        }
        CliRequest::SetNotificationSettings { settings } => {
            let mut cfg = ctx.config.lock().unwrap();
            cfg.notifications = config::NotificationSettings {
                conflict: settings.conflict,
                transfer_completed: settings.transfer_completed,
                device_joined: settings.device_joined,
                error: settings.error,
            };
            persist_config(ctx, &cfg);
            CliResponse::Ok {
                message: "Notification settings updated.".to_string(),
            }
        }

        // -- M23a: Device Management --
        CliRequest::GetDevicePresence => {
            // A device is considered "online" if we received a gossip message
            // from it within the last 60 seconds.
            const ONLINE_THRESHOLD_SECS: u64 = 60;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let eng = ctx.engine.lock().unwrap();
            let presence = ctx.device_presence.lock().unwrap();
            let devices = eng
                .list_devices()
                .into_iter()
                .filter(|d| d.approved)
                .map(|d| {
                    let (_, last_seen) = presence.get(&d.device_id).copied().unwrap_or((false, 0));
                    let is_local = d.device_id == eng.device_id();
                    let online = is_local
                        || (last_seen > 0 && now.saturating_sub(last_seen) < ONLINE_THRESHOLD_SECS);
                    DevicePresenceIpc {
                        device_id: d.device_id.to_string(),
                        device_name: d.name,
                        online,
                        last_seen_unix: if is_local { now } else { last_seen },
                    }
                })
                .collect();
            CliResponse::DevicePresence { devices }
        }
        CliRequest::SetDeviceName { name } => {
            let mut eng = ctx.engine.lock().unwrap();
            let device_id = eng.device_id();
            let entry = eng
                .dag_mut()
                .append(murmur_types::Action::DeviceNameChanged {
                    device_id,
                    name: name.clone(),
                });
            let _ = ctx.broadcast_tx.send(entry.to_bytes());
            if let Err(e) = ctx.storage.flush() {
                error!(error = %e, "flush after set_device_name");
            }
            CliResponse::Ok {
                message: format!("Device name set to '{name}'."),
            }
        }

        // -- M24a: Sync Progress --
        CliRequest::PauseFolderSync { folder_id_hex } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            ctx.paused_folders.lock().unwrap().insert(folder_id);
            CliResponse::Ok {
                message: format!("Sync paused for folder {folder_id}."),
            }
        }
        CliRequest::ResumeFolderSync { folder_id_hex } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            ctx.paused_folders.lock().unwrap().remove(&folder_id);
            CliResponse::Ok {
                message: format!("Sync resumed for folder {folder_id}."),
            }
        }

        // -- M25a: File Browser --
        CliRequest::DeleteFile {
            folder_id_hex,
            path,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let mut eng = ctx.engine.lock().unwrap();
            match eng.delete_file(folder_id, &path) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after delete_file");
                    }
                    CliResponse::Ok {
                        message: format!("File deleted: {path}"),
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("{e}"),
                },
            }
        }

        // -- M26a: Settings & Configuration --
        CliRequest::SetAutoApprove { enabled } => {
            {
                let mut cfg = ctx.config.lock().unwrap();
                cfg.network.auto_approve = enabled;
                persist_config(ctx, &cfg);
            }
            CliResponse::Ok {
                message: format!("Auto-approve set to {enabled}."),
            }
        }
        CliRequest::SetMdns { enabled } => {
            {
                let mut cfg = ctx.config.lock().unwrap();
                cfg.network.mdns = enabled;
                persist_config(ctx, &cfg);
            }
            CliResponse::Ok {
                message: format!("mDNS set to {enabled}."),
            }
        }
        CliRequest::ReclaimOrphanedBlobs => {
            let eng = ctx.engine.lock().unwrap();
            // Collect all blob hashes referenced in the DAG state.
            let referenced: std::collections::HashSet<BlobHash> =
                eng.state().files.values().map(|m| m.blob_hash).collect();
            drop(eng);

            match ctx.storage.list_all_blob_hashes() {
                Ok(on_disk) => {
                    let mut bytes_freed = 0u64;
                    let mut blobs_removed = 0u64;
                    for hash in on_disk {
                        if !referenced.contains(&hash) {
                            match ctx.storage.remove_blob(hash) {
                                Ok(size) => {
                                    bytes_freed += size;
                                    blobs_removed += 1;
                                }
                                Err(e) => {
                                    warn!(error = %e, %hash, "failed to remove orphaned blob");
                                }
                            }
                        }
                    }
                    info!(blobs_removed, bytes_freed, "reclaimed orphaned blobs");
                    CliResponse::ReclaimedBytes {
                        bytes_freed,
                        blobs_removed,
                    }
                }
                Err(e) => CliResponse::Error {
                    message: format!("list blobs: {e}"),
                },
            }
        }
        CliRequest::SetFolderLocalPath {
            folder_id_hex,
            new_local_path,
        } => {
            let mut cfg = ctx.config.lock().unwrap();
            if let Some(fc) = cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
            {
                fc.local_path = PathBuf::from(&new_local_path);
                persist_config(ctx, &cfg);
                CliResponse::Ok {
                    message: format!("Folder path updated to {new_local_path}."),
                }
            } else {
                CliResponse::Error {
                    message: format!("folder not in config: {folder_id_hex}"),
                }
            }
        }
        CliRequest::SetFolderName {
            folder_id_hex,
            name,
        } => {
            let mut cfg = ctx.config.lock().unwrap();
            if let Some(fc) = cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
            {
                fc.name = name.clone();
                persist_config(ctx, &cfg);
                CliResponse::Ok {
                    message: format!("Folder renamed to {name}."),
                }
            } else {
                CliResponse::Error {
                    message: format!("folder not in config: {folder_id_hex}"),
                }
            }
        }
        CliRequest::GetIgnorePatterns { folder_id_hex } => {
            // Find the folder's local path, then read .murmurignore.
            let cfg = ctx.config.lock().unwrap();
            match cfg.folders.iter().find(|f| f.folder_id == folder_id_hex) {
                Some(fc) => {
                    let ignore_path = fc.local_path.join(".murmurignore");
                    let patterns = std::fs::read_to_string(&ignore_path).unwrap_or_default();
                    CliResponse::IgnorePatterns { patterns }
                }
                None => {
                    // Folder not in config — try synced_folders.
                    CliResponse::IgnorePatterns {
                        patterns: String::new(),
                    }
                }
            }
        }
        CliRequest::SetIgnorePatterns {
            folder_id_hex,
            patterns,
        } => {
            let cfg = ctx.config.lock().unwrap();
            match cfg.folders.iter().find(|f| f.folder_id == folder_id_hex) {
                Some(fc) => {
                    let ignore_path = fc.local_path.join(".murmurignore");
                    match std::fs::write(&ignore_path, &patterns) {
                        Ok(()) => CliResponse::Ok {
                            message: "Ignore patterns saved.".to_string(),
                        },
                        Err(e) => CliResponse::Error {
                            message: format!("write .murmurignore: {e}"),
                        },
                    }
                }
                None => CliResponse::Error {
                    message: format!("folder not in config: {folder_id_hex}"),
                },
            }
        }
        CliRequest::SetThrottle {
            upload_bytes_per_sec,
            download_bytes_per_sec,
        } => {
            let mut cfg = ctx.config.lock().unwrap();
            cfg.network.throttle.max_upload_bytes_per_sec = upload_bytes_per_sec;
            cfg.network.throttle.max_download_bytes_per_sec = download_bytes_per_sec;
            persist_config(ctx, &cfg);
            CliResponse::Ok {
                message: format!(
                    "Throttle set: up={upload_bytes_per_sec} B/s, down={download_bytes_per_sec} B/s."
                ),
            }
        }

        CliRequest::LeaveNetwork => {
            // Write a marker file so next startup knows to wipe.
            // We can't wipe the DB dir while Fjall has it locked.
            let marker = ctx.base_dir.join(".leave");
            let _ = std::fs::write(&marker, b"");
            info!(base = %ctx.base_dir.display(), "LeaveNetwork: marked for wipe");
            // Exit the daemon process. The desktop app expects the connection
            // to drop. On next startup the marker triggers a full wipe.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                std::process::exit(0);
            });
            CliResponse::Ok {
                message: "Network left. Daemon shutting down.".to_string(),
            }
        }

        // -- M27a: Diagnostics & Network Health --
        CliRequest::ListPeers => {
            const ONLINE_THRESHOLD_SECS: u64 = 60;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let eng = ctx.engine.lock().unwrap();
            let presence = ctx.device_presence.lock().unwrap();
            let device_id = eng.device_id();
            let peers = eng
                .list_devices()
                .into_iter()
                .filter(|d| d.approved && d.device_id != device_id)
                .map(|d| {
                    let (_, last_seen) = presence.get(&d.device_id).copied().unwrap_or((false, 0));
                    let online =
                        last_seen > 0 && now.saturating_sub(last_seen) < ONLINE_THRESHOLD_SECS;
                    let conn_type = if online { "direct" } else { "relay" };
                    PeerInfoIpc {
                        device_id: d.device_id.to_string(),
                        device_name: d.name,
                        connection_type: conn_type.to_string(),
                        last_seen_unix: last_seen,
                    }
                })
                .collect();
            CliResponse::Peers { peers }
        }
        CliRequest::StorageStats => {
            let eng = ctx.engine.lock().unwrap();
            let state = eng.state();

            // Per-folder stats.
            let folders: Vec<FolderStorageIpc> = state
                .folders
                .values()
                .map(|f| {
                    let files: Vec<_> = state
                        .files
                        .iter()
                        .filter(|((fid, _), _)| *fid == f.folder_id)
                        .collect();
                    let file_count = files.len() as u64;
                    let total_bytes: u64 = files.iter().map(|(_, m)| m.size).sum();
                    FolderStorageIpc {
                        folder_id: f.folder_id.to_string(),
                        name: f.name.clone(),
                        file_count,
                        total_bytes,
                    }
                })
                .collect();

            let dag_entry_count = ctx.storage.dag_entry_count().unwrap_or(0);

            // Blob stats.
            let (total_blob_count, total_blob_bytes) =
                ctx.storage.blob_dir_stats().unwrap_or((0, 0));

            // Orphaned blobs = on disk but not in DAG state.
            let referenced: std::collections::HashSet<BlobHash> =
                state.files.values().map(|m| m.blob_hash).collect();
            let on_disk = ctx.storage.list_all_blob_hashes().unwrap_or_default();
            let orphaned: Vec<_> = on_disk.iter().filter(|h| !referenced.contains(h)).collect();
            let orphaned_blob_count = orphaned.len() as u64;
            let orphaned_blob_bytes: u64 = orphaned
                .iter()
                .filter_map(|h| {
                    ctx.storage
                        .load_blob(**h)
                        .ok()
                        .flatten()
                        .map(|d| d.len() as u64)
                })
                .sum();

            CliResponse::StorageStatsResponse {
                folders,
                total_blob_count,
                total_blob_bytes,
                orphaned_blob_count,
                orphaned_blob_bytes,
                dag_entry_count,
            }
        }
        CliRequest::RunConnectivityCheck => {
            if let Some(ref endpoint) = ctx.endpoint {
                // Check if the endpoint has a relay URL in its address
                // (meaning it successfully connected to a relay server).
                let addr = endpoint.addr();
                let relay_urls: Vec<_> = addr.relay_urls().collect();
                let relay_reachable = !relay_urls.is_empty();
                let latency_ms = if relay_reachable { Some(0) } else { None };
                CliResponse::ConnectivityResult {
                    relay_reachable,
                    latency_ms,
                }
            } else {
                CliResponse::ConnectivityResult {
                    relay_reachable: false,
                    latency_ms: None,
                }
            }
        }
        // -- Onboarding: pairing invites --
        CliRequest::IssuePairingInvite => {
            let token = murmur_seed::PairingToken::issue_default(
                &ctx.mnemonic,
                ctx.device_id,
                &ctx.signing_key,
            );
            let url = token.to_url();
            let expires_at_unix = token.expires_at_unix;
            info!(
                issuer = %ctx.device_id,
                expires_at_unix,
                "issued pairing invite"
            );
            CliResponse::PairingInvite {
                url,
                expires_at_unix,
            }
        }
        CliRequest::RedeemPairingInvite { url } => {
            let token = match murmur_seed::PairingToken::from_url(&url) {
                Ok(t) => t,
                Err(e) => {
                    return CliResponse::Error {
                        message: format!("invalid invite URL: {e}"),
                    };
                }
            };
            // Enforce single-use on this daemon (cross-daemon replay cannot
            // be detected offline — documented trade-off in docs/plan.md).
            {
                let mut used = ctx.used_invite_nonces.lock().unwrap();
                if !used.insert(token.nonce) {
                    return CliResponse::Error {
                        message: "invite already redeemed on this daemon".to_string(),
                    };
                }
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            match token.redeem(now) {
                Ok(mnemonic) => CliResponse::RedeemedMnemonic {
                    mnemonic,
                    issued_by: token.issued_by.to_string(),
                },
                Err(e) => {
                    // Release the nonce so a corrupt URL doesn't lock out a
                    // later correct one (edge case, but cheap to handle).
                    ctx.used_invite_nonces.lock().unwrap().remove(&token.nonce);
                    CliResponse::Error {
                        message: format!("redeem failed: {e}"),
                    }
                }
            }
        }
        CliRequest::ExportDiagnostics { output_path } => {
            const ONLINE_THRESHOLD_SECS: u64 = 60;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let eng = ctx.engine.lock().unwrap();
            let presence = ctx.device_presence.lock().unwrap();
            let device_id = eng.device_id();

            let peers: Vec<serde_json::Value> = eng
                .list_devices()
                .into_iter()
                .filter(|d| d.approved && d.device_id != device_id)
                .map(|d| {
                    let (_, last_seen) = presence.get(&d.device_id).copied().unwrap_or((false, 0));
                    let online =
                        last_seen > 0 && now.saturating_sub(last_seen) < ONLINE_THRESHOLD_SECS;
                    serde_json::json!({
                        "device_id": d.device_id.to_string(),
                        "name": d.name,
                        "online": online,
                        "last_seen_unix": last_seen,
                    })
                })
                .collect();

            let dag_count = ctx.storage.dag_entry_count().unwrap_or(0);
            let (blob_count, blob_bytes) = ctx.storage.blob_dir_stats().unwrap_or((0, 0));
            let uptime = ctx.start_time.elapsed().as_secs();

            let diagnostics = serde_json::json!({
                "device_id": device_id.to_string(),
                "device_name": ctx.device_name,
                "network_id": ctx.network_id_hex,
                "uptime_secs": uptime,
                "dag_entry_count": dag_count,
                "blob_count": blob_count,
                "blob_bytes": blob_bytes,
                "peers": peers,
            });

            let output = std::path::Path::new(&output_path);
            if let Some(parent) = output.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return CliResponse::Error {
                    message: format!("create output dir: {e}"),
                };
            }
            match std::fs::write(
                output,
                serde_json::to_string_pretty(&diagnostics).unwrap_or_default(),
            ) {
                Ok(()) => CliResponse::Ok {
                    message: format!("Diagnostics exported to {output_path}"),
                },
                Err(e) => CliResponse::Error {
                    message: format!("write diagnostics: {e}"),
                },
            }
        }
    }
}

/// Process `AddFile` request (extracted for readability).
fn process_add_file(path: String, ctx: &DaemonCtx) -> CliResponse {
    let file_path = std::path::Path::new(&path);
    if !file_path.exists() {
        return CliResponse::Error {
            message: format!("file not found: {path}"),
        };
    }
    let data = match std::fs::read(file_path) {
        Ok(d) => d,
        Err(e) => {
            return CliResponse::Error {
                message: format!("read file: {e}"),
            };
        }
    };
    let blob_hash = murmur_types::BlobHash::from_data(&data);
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let size = data.len() as u64;
    let mime_type = guess_mime(&filename);

    let mut eng = ctx.engine.lock().unwrap();

    // Get or create the "default" folder for the file. Prefer a folder
    // literally named "default"; if none exists, create one. Fall back to
    // the first folder only as a legacy safety net for networks whose
    // default folder was renamed.
    let folder_id = {
        let folders = eng.list_folders();
        if let Some(f) = folders.iter().find(|f| f.name == "default") {
            f.folder_id
        } else if folders.is_empty() {
            match eng.create_folder("default") {
                Ok((folder, entries)) => {
                    for entry in &entries {
                        let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    }
                    folder.folder_id
                }
                Err(e) => {
                    return CliResponse::Error {
                        message: format!("create default folder: {e}"),
                    };
                }
            }
        } else {
            folders[0].folder_id
        }
    };

    // Auto-subscribe to the folder if not already subscribed.
    let device_id = eng.device_id();
    let already_subscribed = eng
        .folder_subscriptions(folder_id)
        .iter()
        .any(|s| s.device_id == device_id);
    if !already_subscribed {
        match eng.subscribe_folder(folder_id, murmur_types::SyncMode::Full) {
            Ok(entry) => {
                let _ = ctx.broadcast_tx.send(entry.to_bytes());
            }
            Err(e) => {
                return CliResponse::Error {
                    message: format!("subscribe to folder: {e}"),
                };
            }
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let metadata = murmur_types::FileMetadata {
        blob_hash,
        folder_id,
        path: filename.clone(),
        size,
        mime_type: mime_type.clone(),
        created_at: now,
        modified_at: now,
        device_origin: eng.device_id(),
    };
    match eng.add_file(metadata, data) {
        Ok(entry) => {
            let _ = ctx.broadcast_tx.send(entry.to_bytes());
            if let Err(e) = ctx.storage.push_queue_add(blob_hash) {
                error!(error = %e, "enqueue blob for push");
            }
            if let Err(e) = ctx.storage.flush() {
                error!(error = %e, "flush after add_file");
            }
            CliResponse::Ok {
                message: format!("File added: {filename} ({blob_hash})"),
            }
        }
        Err(e) => CliResponse::Error {
            message: format!("{e}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Auto-init
// ---------------------------------------------------------------------------

/// Initialize a new network on first run.
fn auto_init(base_dir: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(base_dir).context("create base directory")?;

    // Use existing mnemonic (written by desktop join flow) or generate a new one.
    let mnemonic_path = Config::mnemonic_path(base_dir);
    let joining = mnemonic_path.exists();
    let mnemonic = if joining {
        info!("first run — joining existing network");
        let s = std::fs::read_to_string(&mnemonic_path).context("read existing mnemonic")?;
        murmur_seed::parse_mnemonic(s.trim())?
    } else {
        info!("first run — creating new network");
        let m = murmur_seed::generate_mnemonic(murmur_seed::WordCount::TwentyFour);
        std::fs::write(&mnemonic_path, m.to_string().as_bytes()).context("save mnemonic")?;
        m
    };
    let identity = murmur_seed::NetworkIdentity::from_mnemonic(&mnemonic, "");

    // For a joining device, generate a unique device key and write it to disk.
    // murmurd detects this file on startup to decide whether to call
    // join_network() (joiner, pending approval) vs create_network() (creator).
    let device_id = if joining {
        let kp = murmur_seed::DeviceKeyPair::generate();
        let id = kp.device_id();
        std::fs::write(Config::device_key_path(base_dir), kp.to_bytes())
            .context("save device key")?;
        id
    } else {
        identity.first_device_id()
    };

    // Write config.
    let config = Config::new(base_dir, name);
    let toml_str = toml::to_string_pretty(&config).context("serialize config")?;
    std::fs::write(Config::config_path(base_dir), toml_str).context("write config")?;

    if joining {
        println!("Joined existing Murmur network.");
    } else {
        println!("New Murmur network created.");
        println!();
        println!("IMPORTANT — Write down your mnemonic and store it safely:");
        println!();
        println!("  {mnemonic}");
    }
    println!();
    println!("Device ID: {device_id}");
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Persist the current config to disk. Logs errors but does not return them.
fn persist_config(ctx: &DaemonCtx, cfg: &Config) {
    let path = Config::config_path(&ctx.base_dir);
    if let Err(e) = cfg.save(&path) {
        error!(error = %e, "failed to persist config");
    }
}

/// Convert a `DeviceInfo` to IPC format.
fn device_to_ipc(dev: murmur_types::DeviceInfo) -> DeviceInfoIpc {
    DeviceInfoIpc {
        device_id: dev.device_id.to_string(),
        name: dev.name,
        approved: dev.approved,
    }
}

/// Parse a hex string into a [`DeviceId`].
fn parse_device_id(hex_str: &str) -> Result<DeviceId> {
    let hex_str = hex_str.trim();
    if hex_str.len() != 64 {
        anyhow::bail!(
            "device ID must be 64 hex characters (32 bytes), got {}",
            hex_str.len()
        );
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16)
            .context("invalid hex in device ID")?;
    }
    Ok(DeviceId::from_bytes(bytes))
}

/// Simple MIME type guess from file extension.
///
/// Returns `application/octet-stream` when the extension is unknown or the
/// filename has no extension, so callers always get a non-null MIME.
fn guess_mime(filename: &str) -> Option<String> {
    let lower = filename.to_lowercase();
    let ext = match lower.rsplit_once('.') {
        Some((_, e)) => e,
        None => return Some("application/octet-stream".to_string()),
    };
    let mime = match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    };
    Some(mime.to_string())
}

/// Parse a hex string into a [`FolderId`].
fn parse_folder_id(hex_str: &str) -> Result<FolderId, String> {
    let hex_str = hex_str.trim();
    if hex_str.len() != 64 {
        return Err(format!(
            "folder ID must be 64 hex characters (32 bytes), got {}",
            hex_str.len()
        ));
    }
    let bytes = hex::decode(hex_str).map_err(|e| format!("invalid hex in folder ID: {e}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "folder ID must be 32 bytes".to_string())?;
    Ok(FolderId::from_bytes(arr))
}

/// Parse a hex string into a [`BlobHash`].
fn parse_blob_hash(hex_str: &str) -> Result<BlobHash, String> {
    let hex_str = hex_str.trim();
    if hex_str.len() != 64 {
        return Err(format!(
            "blob hash must be 64 hex characters (32 bytes), got {}",
            hex_str.len()
        ));
    }
    let bytes = hex::decode(hex_str).map_err(|e| format!("invalid hex in blob hash: {e}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "blob hash must be 32 bytes".to_string())?;
    Ok(BlobHash::from_bytes(arr))
}

/// Seconds between conflict-expiry ticks. Short enough that setting
/// `conflict_expiry_days = 0` (used in tests) triggers resolution promptly,
/// long enough to be free in steady state.
const EXPIRE_CONFLICTS_INTERVAL_SECS: u64 = 60;

/// Scan every locally-configured folder for conflicts that have aged past
/// `conflict_expiry_days`. Apply the folder's `auto_resolve` strategy
/// (`newest` or `mine`), or fall back to "keep both" — a DAG-free dismissal
/// that leaves both file versions on disk untouched.
///
/// Emits [`murmur_engine::EngineEvent::ConflictAutoResolved`] per resolved
/// conflict so the desktop activity feed surfaces the action.
pub(crate) fn expire_conflicts(ctx: &DaemonCtx) {
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    // Snapshot per-folder expiry policy under the config lock only.
    let policies: Vec<(FolderId, u64, String)> = {
        let cfg = ctx.config.lock().unwrap();
        cfg.folders
            .iter()
            .filter_map(|fc| {
                let days = fc.conflict_expiry_days?;
                // Treat `Some(0)` as "expire immediately" — useful for tests
                // and also a sensible "nag me about any unresolved conflict
                // on the next tick" mode.
                let fid_bytes: [u8; 32] = hex::decode(&fc.folder_id).ok()?.try_into().ok()?;
                Some((
                    FolderId::from_bytes(fid_bytes),
                    days,
                    fc.auto_resolve.clone(),
                ))
            })
            .collect()
    };

    if policies.is_empty() {
        return;
    }

    let mut dag_entries_to_broadcast: Vec<Vec<u8>> = Vec::new();
    let mut events_to_broadcast: Vec<murmur_engine::EngineEvent> = Vec::new();
    let mut any_resolved = false;

    {
        let mut eng = ctx.engine.lock().unwrap();
        let device_id = eng.device_id();

        for (folder_id, days, strategy) in &policies {
            let expiry_ns = (*days).saturating_mul(24 * 3600 * 1_000_000_000u64);
            let conflicts = eng.list_conflicts_in_folder(*folder_id);
            for conflict in conflicts {
                if conflict.detected_at.saturating_add(expiry_ns) > now_ns {
                    continue;
                }
                let chosen = match strategy.as_str() {
                    "newest" => conflict
                        .versions
                        .iter()
                        .max_by_key(|v| v.hlc)
                        .map(|v| v.blob_hash),
                    "mine" => conflict
                        .versions
                        .iter()
                        .find(|v| v.device_id == device_id)
                        .map(|v| v.blob_hash),
                    // "none" (or any unrecognised value) → keep both.
                    _ => None,
                };

                let applied_strategy = if chosen.is_some() {
                    strategy.clone()
                } else {
                    "keep_both".to_string()
                };

                if let Some(hash) = chosen {
                    match eng.resolve_conflict(*folder_id, &conflict.path, hash) {
                        Ok(entry) => {
                            dag_entries_to_broadcast.push(entry.to_bytes());
                            events_to_broadcast.push(
                                murmur_engine::EngineEvent::ConflictAutoResolved {
                                    folder_id: *folder_id,
                                    path: conflict.path.clone(),
                                    strategy: applied_strategy,
                                },
                            );
                            any_resolved = true;
                        }
                        Err(e) => {
                            warn!(error = %e, path = %conflict.path, "expire_conflicts: resolve failed");
                        }
                    }
                } else {
                    // Keep-both fallback: dismiss from active list without a
                    // `ConflictResolved` DAG entry. Both files remain on
                    // disk and the user can still intervene later if another
                    // peer reintroduces the conflict.
                    let state = eng.state_mut();
                    state
                        .conflicts
                        .retain(|c| !(c.folder_id == *folder_id && c.path == conflict.path));
                    events_to_broadcast.push(murmur_engine::EngineEvent::ConflictAutoResolved {
                        folder_id: *folder_id,
                        path: conflict.path.clone(),
                        strategy: applied_strategy,
                    });
                    any_resolved = true;
                }
            }
        }
    }

    for bytes in dag_entries_to_broadcast {
        let _ = ctx.broadcast_tx.send(bytes);
    }
    for event in events_to_broadcast {
        let _ = ctx.event_broadcast.send(event);
    }

    if any_resolved && let Err(e) = ctx.storage.flush() {
        error!(error = %e, "flush after expire_conflicts");
    }
}

/// Upper bound on the bytes returned per side of a [`CliResponse::ConflictDiff`].
///
/// Keeps the full payload well under the 1 MiB IPC frame cap even when both
/// sides max out. Text diffs over this size are rare and truncation there is
/// acceptable — `similar` renders the truncated slice and the client can read
/// `size` to know more bytes exist on disk.
const CONFLICT_DIFF_MAX_BYTES: usize = 256 * 1024;

/// Binary-detection heuristic: treat the prefix as text if it is valid UTF-8
/// and contains no NUL bytes (a strong binary signal in otherwise-UTF-8 data).
///
/// Scans only the first [`UTF8_SNIFF_LEN`] bytes so behaviour is bounded on
/// very large blobs. Both sides must pass for the diff to be rendered as text.
fn is_utf8_text(bytes: &[u8]) -> bool {
    const UTF8_SNIFF_LEN: usize = 8192;
    let prefix = &bytes[..bytes.len().min(UTF8_SNIFF_LEN)];
    if prefix.contains(&0u8) {
        return false;
    }
    std::str::from_utf8(prefix).is_ok()
}

/// Handle a [`CliRequest::ConflictDiff`]: load both blobs, detect text vs.
/// binary, and build a [`CliResponse::ConflictDiff`].
///
/// Returns an error response if the conflict doesn't exist or doesn't have
/// exactly two versions (the diff viewer only supports pairs — multi-way
/// conflicts still show all versions in the list view).
fn handle_conflict_diff(ctx: &DaemonCtx, folder_id_hex: &str, path: &str) -> CliResponse {
    use murmur_ipc::ConflictDiffSide;

    let folder_id = match parse_folder_id(folder_id_hex) {
        Ok(id) => id,
        Err(e) => return CliResponse::Error { message: e },
    };

    // Snapshot the conflict + device names while holding the engine lock once.
    let (left_version, right_version, left_device_name, right_device_name) = {
        let eng = ctx.engine.lock().unwrap();
        let conflict = match eng
            .list_conflicts_in_folder(folder_id)
            .into_iter()
            .find(|c| c.path == path)
        {
            Some(c) => c,
            None => {
                return CliResponse::Error {
                    message: format!("no active conflict for {path}"),
                };
            }
        };
        if conflict.versions.len() < 2 {
            return CliResponse::Error {
                message: format!(
                    "conflict has {} versions — at least 2 required",
                    conflict.versions.len()
                ),
            };
        }
        let state = eng.state();
        let left = conflict.versions[0].clone();
        let right = conflict.versions[1].clone();
        let ln = state
            .devices
            .get(&left.device_id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let rn = state
            .devices
            .get(&right.device_id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "unknown".to_string());
        (left, right, ln, rn)
    };

    let load = |hash: BlobHash| -> (u64, Vec<u8>) {
        match ctx.storage.load_blob(hash) {
            Ok(Some(data)) => {
                let size = data.len() as u64;
                let truncated = if data.len() > CONFLICT_DIFF_MAX_BYTES {
                    data[..CONFLICT_DIFF_MAX_BYTES].to_vec()
                } else {
                    data
                };
                (size, truncated)
            }
            _ => (0, Vec::new()),
        }
    };

    let (left_size, left_bytes) = load(left_version.blob_hash);
    let (right_size, right_bytes) = load(right_version.blob_hash);

    // Text vs. binary is determined on the bytes we actually hold. A truncated
    // prefix is still a fine representative sample for UTF-8 detection.
    let is_text = !left_bytes.is_empty()
        && !right_bytes.is_empty()
        && is_utf8_text(&left_bytes)
        && is_utf8_text(&right_bytes);

    CliResponse::ConflictDiff {
        is_text,
        left: ConflictDiffSide {
            blob_hash: left_version.blob_hash.to_string(),
            device_name: left_device_name,
            size: left_size,
            bytes: left_bytes,
        },
        right: ConflictDiffSide {
            blob_hash: right_version.blob_hash.to_string(),
            device_name: right_device_name,
            size: right_size,
            bytes: right_bytes,
        },
    }
}

/// Build a list of conflict info for IPC, optionally filtered by folder.
fn build_conflict_list(
    eng: &murmur_engine::MurmurEngine,
    folder_filter: Option<FolderId>,
) -> Vec<ConflictInfoIpc> {
    let conflicts = match folder_filter {
        Some(fid) => eng.list_conflicts_in_folder(fid),
        None => eng.list_conflicts(),
    };
    let state = eng.state();
    conflicts
        .into_iter()
        .map(|c| {
            let folder_name = state
                .folders
                .get(&c.folder_id)
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let versions = c
                .versions
                .iter()
                .map(|v| {
                    let device_name = state
                        .devices
                        .get(&v.device_id)
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    ConflictVersionIpc {
                        blob_hash: v.blob_hash.to_string(),
                        device_id: v.device_id.to_string(),
                        device_name,
                        hlc: v.hlc,
                    }
                })
                .collect();
            ConflictInfoIpc {
                folder_id: c.folder_id.to_string(),
                folder_name,
                path: c.path,
                versions,
            }
        })
        .collect()
}

/// Look up device info for a specific file version.
///
/// Returns `(device_id_hex, device_name, file_size)` by scanning
/// DAG entries for the one that introduced this version.
fn find_version_info(
    eng: &murmur_engine::MurmurEngine,
    folder_id: FolderId,
    _path: &str,
    blob_hash: BlobHash,
    hlc: u64,
) -> (String, String, u64) {
    use murmur_types::Action;

    // Scan DAG entries to find the entry that introduced this version.
    // Match on HLC + action containing matching blob_hash.
    for entry in eng.all_entries() {
        if entry.hlc != hlc {
            continue;
        }
        let meta = match &entry.action {
            Action::FileAdded { metadata }
                if metadata.folder_id == folder_id && metadata.blob_hash == blob_hash =>
            {
                Some(metadata)
            }
            Action::FileModified { metadata, .. }
                if metadata.folder_id == folder_id && metadata.blob_hash == blob_hash =>
            {
                Some(metadata)
            }
            _ => None,
        };
        if let Some(metadata) = meta {
            let state = eng.state();
            let device_name = state
                .devices
                .get(&metadata.device_origin)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "unknown".to_string());
            return (
                metadata.device_origin.to_string(),
                device_name,
                metadata.size,
            );
        }
    }

    // Fallback: version not found in DAG (should be rare).
    ("unknown".to_string(), "unknown".to_string(), 0)
}

/// Escape a string for embedding in a JSON value.
///
/// Handles quotes, backslashes, and control characters per RFC 8259.
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                // \u00XX for other control chars.
                for unit in c.encode_utf16(&mut [0; 2]) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Convert an [`EngineEvent`] to its IPC representation.
fn engine_event_to_ipc(event: &murmur_engine::EngineEvent, _ctx: &DaemonCtx) -> EngineEventIpc {
    use murmur_engine::EngineEvent;
    match event {
        EngineEvent::DeviceJoinRequested { device_id, name } => EngineEventIpc {
            event_type: "device_join_requested".to_string(),
            data: format!(
                "{{\"device_id\":\"{device_id}\",\"name\":\"{}\"}}",
                escape_json(name)
            ),
        },
        EngineEvent::DeviceApproved { device_id } => EngineEventIpc {
            event_type: "device_approved".to_string(),
            data: format!("{{\"device_id\":\"{device_id}\"}}"),
        },
        EngineEvent::DeviceRevoked { device_id } => EngineEventIpc {
            event_type: "device_revoked".to_string(),
            data: format!("{{\"device_id\":\"{device_id}\"}}"),
        },
        EngineEvent::FolderCreated { folder_id, name } => EngineEventIpc {
            event_type: "folder_created".to_string(),
            data: format!(
                "{{\"folder_id\":\"{folder_id}\",\"name\":\"{}\"}}",
                escape_json(name)
            ),
        },
        EngineEvent::FolderSubscribed {
            folder_id,
            device_id,
            mode,
        } => EngineEventIpc {
            event_type: "folder_subscribed".to_string(),
            data: format!(
                "{{\"folder_id\":\"{folder_id}\",\"device_id\":\"{device_id}\",\"mode\":\"{mode}\"}}"
            ),
        },
        EngineEvent::FileSynced {
            blob_hash,
            folder_id,
            path,
        } => EngineEventIpc {
            event_type: "file_synced".to_string(),
            data: format!(
                "{{\"blob_hash\":\"{blob_hash}\",\"folder_id\":\"{folder_id}\",\"path\":\"{}\"}}",
                escape_json(path)
            ),
        },
        EngineEvent::FileModified {
            folder_id,
            path,
            new_hash,
        } => EngineEventIpc {
            event_type: "file_modified".to_string(),
            data: format!(
                "{{\"folder_id\":\"{folder_id}\",\"path\":\"{}\",\"new_hash\":\"{new_hash}\"}}",
                escape_json(path)
            ),
        },
        EngineEvent::BlobReceived { blob_hash } => EngineEventIpc {
            event_type: "blob_received".to_string(),
            data: format!("{{\"blob_hash\":\"{blob_hash}\"}}"),
        },
        EngineEvent::AccessRequested { from } => EngineEventIpc {
            event_type: "access_requested".to_string(),
            data: format!("{{\"from\":\"{from}\"}}"),
        },
        EngineEvent::AccessGranted { to } => EngineEventIpc {
            event_type: "access_granted".to_string(),
            data: format!("{{\"to\":\"{to}\"}}"),
        },
        EngineEvent::DagSynced { new_entries } => EngineEventIpc {
            event_type: "dag_synced".to_string(),
            data: format!("{{\"new_entries\":{new_entries}}}"),
        },
        EngineEvent::NetworkCreated { device_id } => EngineEventIpc {
            event_type: "network_created".to_string(),
            data: format!("{{\"device_id\":\"{device_id}\"}}"),
        },
        EngineEvent::NetworkJoined { device_id } => EngineEventIpc {
            event_type: "network_joined".to_string(),
            data: format!("{{\"device_id\":\"{device_id}\"}}"),
        },
        EngineEvent::BlobTransferProgress {
            blob_hash,
            bytes_transferred,
            total_bytes,
        } => EngineEventIpc {
            event_type: "blob_transfer_progress".to_string(),
            data: format!(
                "{{\"blob_hash\":\"{blob_hash}\",\"bytes_transferred\":{bytes_transferred},\"total_bytes\":{total_bytes}}}"
            ),
        },
        EngineEvent::ConflictDetected {
            folder_id,
            path,
            versions,
        } => {
            let version_count = versions.len();
            EngineEventIpc {
                event_type: "conflict_detected".to_string(),
                data: format!(
                    "{{\"folder_id\":\"{folder_id}\",\"path\":\"{}\",\"version_count\":{version_count}}}",
                    escape_json(path)
                ),
            }
        }
        EngineEvent::FileDeleted { folder_id, path } => EngineEventIpc {
            event_type: "file_deleted".to_string(),
            data: format!(
                "{{\"folder_id\":\"{folder_id}\",\"path\":\"{}\"}}",
                escape_json(path)
            ),
        },
        EngineEvent::ConflictAutoResolved {
            folder_id,
            path,
            strategy,
        } => EngineEventIpc {
            event_type: "conflict_auto_resolved".to_string(),
            data: format!(
                "{{\"folder_id\":\"{folder_id}\",\"path\":\"{}\",\"strategy\":\"{}\"}}",
                escape_json(path),
                escape_json(strategy)
            ),
        },
    }
}

/// Remove a stale socket file if no process is listening.
/// Check if another murmurd is already running.
///
/// Uses both the socket file and a PID file for robust detection:
/// 1. If the socket is connectable → another daemon is live, abort.
/// 2. If a PID file exists and the process is alive → abort.
/// 3. Otherwise clean up stale socket/PID files.
fn cleanup_stale_instance(sock_path: &Path, pid_path: &Path) -> Result<()> {
    // Check socket first — most reliable.
    if sock_path.exists() {
        if std::os::unix::net::UnixStream::connect(sock_path).is_ok() {
            anyhow::bail!(
                "another murmurd is already running (socket {} is active)",
                sock_path.display()
            );
        }
        info!(path = %sock_path.display(), "removing stale socket");
        let _ = std::fs::remove_file(sock_path);
    }

    // Check PID file — catches daemons that lost their socket (e.g. mid-shutdown).
    if pid_path.exists()
        && let Ok(contents) = std::fs::read_to_string(pid_path)
        && let Ok(pid) = contents.trim().parse::<i32>()
    {
        // kill(pid, 0) checks if the process exists without sending a signal.
        if unsafe { libc::kill(pid, 0) } == 0 {
            anyhow::bail!("another murmurd is already running (PID {pid})");
        }
        info!("removing stale PID file");
        let _ = std::fs::remove_file(pid_path);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// Create a test [`DaemonCtx`] with storage in a temp dir.
    fn test_ctx(dir: &Path) -> DaemonCtx {
        let mnemonic = murmur_seed::generate_mnemonic(murmur_seed::WordCount::Twelve);
        let identity = murmur_seed::NetworkIdentity::from_mnemonic(&mnemonic, "");
        let device_id = identity.first_device_id();
        let signing_key = identity.first_device_signing_key().clone();
        let network_id_hex = identity.network_id().to_string();

        let storage = Arc::new(Storage::open(&dir.join("db"), &dir.join("blobs")).unwrap());
        let platform = Arc::new(FjallPlatform::new(storage.clone()));

        let ctx_signing_key = signing_key.clone();
        let engine = murmur_engine::MurmurEngine::create_network(
            device_id,
            signing_key,
            "TestDaemon".to_string(),
            platform,
        );

        let (broadcast_tx, _rx) = mpsc::unbounded_channel();
        let (event_broadcast, _) =
            tokio::sync::broadcast::channel::<murmur_engine::EngineEvent>(16);

        let config = Config::new(dir, "TestDaemon");
        DaemonCtx {
            engine: Arc::new(Mutex::new(engine)),
            storage,
            device_name: "TestDaemon".to_string(),
            network_id_hex,
            mnemonic: mnemonic.to_string(),
            start_time: Instant::now(),
            broadcast_tx,
            connected_peers: Arc::new(AtomicU64::new(0)),
            event_broadcast,
            synced_folders: Arc::new(std::collections::HashMap::new()),
            base_dir: dir.to_path_buf(),
            config: Arc::new(Mutex::new(config)),
            sync_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            paused_folders: Arc::new(Mutex::new(std::collections::HashSet::new())),
            device_presence: Arc::new(Mutex::new(std::collections::HashMap::new())),
            endpoint: None,
            used_invite_nonces: Arc::new(Mutex::new(std::collections::HashSet::new())),
            signing_key: Arc::new(ctx_signing_key),
            device_id,
            transfer_tracker: Arc::new(TransferTracker::new()),
        }
    }

    #[test]
    fn test_parse_device_id_valid() {
        let hex = "ab".repeat(32);
        let id = parse_device_id(&hex).unwrap();
        assert_eq!(id, DeviceId::from_bytes([0xab; 32]));
    }

    #[test]
    fn test_parse_device_id_invalid_length() {
        assert!(parse_device_id("abcd").is_err());
    }

    #[test]
    fn test_parse_device_id_invalid_hex() {
        let hex = "zz".repeat(32);
        assert!(parse_device_id(&hex).is_err());
    }

    #[test]
    fn test_process_request_status() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(CliRequest::Status, &ctx);

        match resp {
            CliResponse::Status {
                device_name,
                peer_count,
                ..
            } => {
                assert_eq!(device_name, "TestDaemon");
                assert_eq!(peer_count, 0); // no gossip peers in test
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_list_devices() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(CliRequest::ListDevices, &ctx);

        match resp {
            CliResponse::Devices { devices } => {
                assert!(!devices.is_empty());
                assert!(devices[0].approved);
            }
            other => panic!("expected Devices, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_list_pending_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(CliRequest::ListPending, &ctx);

        match resp {
            CliResponse::Pending { devices } => assert!(devices.is_empty()),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_show_mnemonic() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let expected_mnemonic = ctx.mnemonic.clone();

        let resp = process_request(CliRequest::ShowMnemonic, &ctx);

        match resp {
            CliResponse::Mnemonic { mnemonic: m } => {
                assert_eq!(m, expected_mnemonic);
            }
            other => panic!("expected Mnemonic, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_approve_invalid_hex() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::ApproveDevice {
                device_id_hex: "not-valid-hex".to_string(),
            },
            &ctx,
        );

        match resp {
            CliResponse::Error { message } => {
                assert!(!message.is_empty());
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_list_files_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(CliRequest::ListFiles, &ctx);

        match resp {
            CliResponse::Files { files } => assert!(files.is_empty()),
            other => panic!("expected Files, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_add_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::AddFile {
                path: "/nonexistent/file.txt".to_string(),
            },
            &ctx,
        );

        match resp {
            CliResponse::Error { message } => {
                assert!(message.contains("not found"), "got: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_add_file_success() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create a test file.
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, b"hello world").unwrap();

        let resp = process_request(
            CliRequest::AddFile {
                path: file_path.to_string_lossy().to_string(),
            },
            &ctx,
        );

        match resp {
            CliResponse::Ok { message } => {
                assert!(message.contains("test.txt"), "got: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        // Verify file shows up in listing.
        let resp2 = process_request(CliRequest::ListFiles, &ctx);
        match resp2 {
            CliResponse::Files { files } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, "test.txt");
            }
            other => panic!("expected Files, got {other:?}"),
        }
    }

    #[test]
    fn test_socket_listener_accepts_and_responds() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test.sock");
        let ctx = Arc::new(test_ctx(dir.path()));

        // Create listener.
        let listener = UnixListener::bind(&sock_path).unwrap();

        // Spawn a thread to accept one connection.
        let ctx_clone = ctx.clone();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, &ctx_clone).unwrap();
        });

        // Connect as client.
        let mut client = UnixStream::connect(&sock_path).unwrap();
        murmur_ipc::send_message(&mut client, &CliRequest::Status).unwrap();
        let resp: CliResponse = murmur_ipc::recv_message(&mut client).unwrap();

        match resp {
            CliResponse::Status { device_name, .. } => {
                assert_eq!(device_name, "TestDaemon");
            }
            other => panic!("expected Status, got {other:?}"),
        }

        handle.join().unwrap();
    }

    #[test]
    fn test_socket_concurrent_connections() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("concurrent.sock");
        let ctx = Arc::new(test_ctx(dir.path()));

        let listener = UnixListener::bind(&sock_path).unwrap();

        // Accept 3 connections on separate threads.
        let ctx_for_accept = ctx.clone();
        let accept_handle = std::thread::spawn(move || {
            let mut handles = Vec::new();
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                let ctx = ctx_for_accept.clone();
                handles.push(std::thread::spawn(move || {
                    handle_connection(stream, &ctx).unwrap();
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });

        // Send 3 concurrent requests.
        let mut client_handles = Vec::new();
        for _ in 0..3 {
            let path = sock_path.clone();
            client_handles.push(std::thread::spawn(move || {
                let mut client = UnixStream::connect(&path).unwrap();
                murmur_ipc::send_message(&mut client, &CliRequest::Status).unwrap();
                let resp: CliResponse = murmur_ipc::recv_message(&mut client).unwrap();
                assert!(matches!(resp, CliResponse::Status { .. }));
            }));
        }

        for h in client_handles {
            h.join().unwrap();
        }
        accept_handle.join().unwrap();
    }

    #[test]
    fn test_cleanup_stale_instance() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("stale.sock");
        let pid_path = dir.path().join("murmurd.pid");

        // Create a stale socket file (not a real listener).
        std::fs::write(&sock_path, "stale").unwrap();
        assert!(sock_path.exists());

        // Create a stale PID file (non-existent PID).
        std::fs::write(&pid_path, "999999999").unwrap();
        assert!(pid_path.exists());

        cleanup_stale_instance(&sock_path, &pid_path).unwrap();

        // Both should be cleaned up.
        assert!(!sock_path.exists());
        assert!(!pid_path.exists());
    }

    #[test]
    fn test_auto_init_creates_config_and_mnemonic() {
        let dir = tempfile::tempdir().unwrap();
        auto_init(dir.path(), "TestNAS").unwrap();

        assert!(Config::config_path(dir.path()).exists());
        assert!(Config::mnemonic_path(dir.path()).exists());
        // First device uses seed key, no device.key file.
        assert!(!Config::device_key_path(dir.path()).exists());

        // Config should be loadable and have correct values.
        let config = Config::load(&Config::config_path(dir.path())).unwrap();
        assert_eq!(config.device.name, "TestNAS");

        // Mnemonic should be valid.
        let mnemonic_str = std::fs::read_to_string(Config::mnemonic_path(dir.path())).unwrap();
        murmur_seed::parse_mnemonic(mnemonic_str.trim()).unwrap();
    }

    /// Reproduce: Leave network → Join with new mnemonic → Settings shows wrong mnemonic.
    ///
    /// The desktop app writes the mnemonic to disk *before* spawning murmurd.
    /// If a `.leave` marker exists from a previous LeaveNetwork, `run_daemon`
    /// wipes the data dir (including the just-written mnemonic) before
    /// `auto_init` runs, so `auto_init` generates a fresh mnemonic instead of
    /// using the one the user provided.
    #[test]
    fn test_leave_then_join_preserves_provided_mnemonic() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // Simulate a previous LeaveNetwork: .leave marker exists.
        std::fs::write(base.join(".leave"), b"").unwrap();

        // Simulate the desktop app writing the user's mnemonic before
        // spawning murmurd (this is what launch_and_wait does).
        let user_mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        std::fs::write(Config::mnemonic_path(base), user_mnemonic).unwrap();

        // --- Replicate what run_daemon does at startup ---

        // 1. Check for .leave marker and wipe (preserving mnemonic).
        let leave_marker = base.join(".leave");
        if leave_marker.exists() {
            let mnemonic_path = Config::mnemonic_path(base);
            let preserved = std::fs::read(&mnemonic_path).ok();
            let _ = std::fs::remove_dir_all(base);
            std::fs::create_dir_all(base).unwrap();
            if let Some(bytes) = preserved {
                std::fs::write(&mnemonic_path, bytes).unwrap();
            }
        }

        // 2. auto_init (first_run because config.toml is gone after wipe).
        auto_init(base, "JoinDevice").unwrap();

        // The mnemonic on disk must be the one the user entered, not a random one.
        let on_disk = std::fs::read_to_string(Config::mnemonic_path(base)).unwrap();
        assert_eq!(
            on_disk.trim(),
            user_mnemonic,
            "mnemonic on disk should be the user-provided one, not a newly generated one"
        );

        // A device.key must have been generated so murmurd treats this as a
        // joiner and calls join_network() instead of create_network().
        assert!(
            Config::device_key_path(base).exists(),
            "joining device must have a device.key so is_joiner is true"
        );
    }

    /// Verify that auto_init in join mode generates a device.key file.
    ///
    /// Without device.key, run_daemon sets is_joiner = false and calls
    /// create_network() — producing a duplicate device ID and no visible state
    /// on either device.
    #[test]
    fn test_auto_init_join_creates_device_key() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // Simulate the desktop app writing the mnemonic before spawning murmurd.
        let user_mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        std::fs::write(Config::mnemonic_path(base), user_mnemonic).unwrap();

        auto_init(base, "JoiningDevice").unwrap();

        // device.key must exist so is_joiner = true.
        assert!(
            Config::device_key_path(base).exists(),
            "join auto_init must write device.key"
        );
        // device.key must be 32 bytes.
        let key_bytes = std::fs::read(Config::device_key_path(base)).unwrap();
        assert_eq!(key_bytes.len(), 32, "device.key must be 32 bytes");
        // Mnemonic is preserved unchanged.
        let on_disk = std::fs::read_to_string(Config::mnemonic_path(base)).unwrap();
        assert_eq!(on_disk.trim(), user_mnemonic);
    }

    #[test]
    fn test_auto_init_skipped_when_config_exists() {
        let dir = tempfile::tempdir().unwrap();
        auto_init(dir.path(), "First").unwrap();

        // Config exists, so run_daemon would skip auto_init.
        assert!(Config::config_path(dir.path()).exists());

        let config = Config::load(&Config::config_path(dir.path())).unwrap();
        assert_eq!(config.device.name, "First");
    }

    #[test]
    fn test_process_request_transfer_status_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(CliRequest::TransferStatus, &ctx);

        match resp {
            CliResponse::TransferStatus { transfers } => assert!(transfers.is_empty()),
            other => panic!("expected TransferStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_notification_settings_roundtrip() {
        // M31: SetNotificationSettings persists and GetNotificationSettings
        // returns the stored value.
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let settings = murmur_ipc::NotificationSettingsIpc {
            conflict: false,
            transfer_completed: true,
            device_joined: false,
            error: true,
        };
        let resp = process_request(
            CliRequest::SetNotificationSettings {
                settings: settings.clone(),
            },
            &ctx,
        );
        assert!(matches!(resp, CliResponse::Ok { .. }));

        let got = process_request(CliRequest::GetNotificationSettings, &ctx);
        match got {
            CliResponse::NotificationSettings { settings: got } => assert_eq!(got, settings),
            other => panic!("expected NotificationSettings, got {other:?}"),
        }
    }

    #[test]
    fn test_process_request_set_folder_color_and_icon() {
        // M31: SetFolderColor/SetFolderIcon update the folder's local
        // config entry and are reflected in GetConfig.
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create a folder so there's an entry in config.
        let local = dir.path().join("sync");
        std::fs::create_dir_all(&local).unwrap();
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "TestFolder".to_string(),
                local_path: Some(local.to_string_lossy().to_string()),
                ignore_patterns: None,
            },
            &ctx,
        );
        let fid = match resp {
            CliResponse::Ok { message } => {
                let s = message.find('(').unwrap();
                let e = message.find(')').unwrap();
                message[s + 1..e].to_string()
            }
            other => panic!("expected Ok, got {other:?}"),
        };

        let r1 = process_request(
            CliRequest::SetFolderColor {
                folder_id_hex: fid.clone(),
                color_hex: Some("#4f8cff".to_string()),
            },
            &ctx,
        );
        assert!(matches!(r1, CliResponse::Ok { .. }));
        let r2 = process_request(
            CliRequest::SetFolderIcon {
                folder_id_hex: fid.clone(),
                icon: Some("photos".to_string()),
            },
            &ctx,
        );
        assert!(matches!(r2, CliResponse::Ok { .. }));

        let cfg = process_request(CliRequest::GetConfig, &ctx);
        match cfg {
            CliResponse::Config { folders, .. } => {
                let fc = folders.iter().find(|f| f.folder_id == fid).unwrap();
                assert_eq!(fc.color_hex.as_deref(), Some("#4f8cff"));
                assert_eq!(fc.icon.as_deref(), Some("photos"));
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn test_transfer_status_exposes_tracker_ewma() {
        // M31: when the transfer tracker has samples, the TransferStatus
        // IPC surfaces smoothed speed + ETA to clients.
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Synthesize a blob in storage + push queue so it shows up.
        // 60 KB total, so a 30s progression at 1 KB/s leaves half remaining
        // (and therefore an ETA).
        let data = vec![0u8; 60_000];
        let blob_hash = murmur_types::BlobHash::from_data(&data);
        ctx.storage.store_blob(blob_hash, &data).unwrap();
        ctx.storage.push_queue_add(blob_hash).unwrap();

        for t in 0..=30u64 {
            ctx.transfer_tracker
                .record_at(blob_hash, 1000 * t, data.len() as u64, t);
        }

        let resp = process_request(CliRequest::TransferStatus, &ctx);
        match resp {
            CliResponse::TransferStatus { transfers } => {
                assert_eq!(transfers.len(), 1);
                let t = &transfers[0];
                assert_eq!(t.total_bytes, data.len() as u64);
                assert_eq!(t.bytes_transferred, 30_000);
                assert!(t.bytes_per_sec_smoothed > 0);
                assert!(t.eta_seconds.is_some());
            }
            other => panic!("expected TransferStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_guess_mime() {
        assert_eq!(guess_mime("photo.jpg"), Some("image/jpeg".to_string()));
        assert_eq!(guess_mime("doc.pdf"), Some("application/pdf".to_string()));
        assert_eq!(guess_mime("video.mp4"), Some("video/mp4".to_string()));
        assert_eq!(guess_mime("index.html"), Some("text/html".to_string()));
        assert_eq!(guess_mime("notes.md"), Some("text/markdown".to_string()));
        assert_eq!(guess_mime("style.css"), Some("text/css".to_string()));
        assert_eq!(guess_mime("script.js"), Some("text/javascript".to_string()));
        // Unknown extensions and extension-less files fall back to octet-stream.
        assert_eq!(
            guess_mime("unknown.xyz"),
            Some("application/octet-stream".to_string())
        );
        assert_eq!(
            guess_mime("README"),
            Some("application/octet-stream".to_string())
        );
        assert_eq!(
            guess_mime("random.bin"),
            Some("application/octet-stream".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // Milestone 17 — IPC & CLI expansion tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_ipc_create_folder() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::CreateFolder {
                name: "Photos".to_string(),
                local_path: None,
                ignore_patterns: None,
            },
            &ctx,
        );

        match resp {
            CliResponse::Ok { message } => {
                assert!(message.contains("Photos"), "got: {message}");
                assert!(message.contains("Folder created"), "got: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        // Folder should appear in list.
        let resp2 = process_request(CliRequest::ListFolders, &ctx);
        match resp2 {
            CliResponse::Folders { folders } => {
                // create_network auto-creates no folders, so only ours should be here.
                assert!(
                    folders.iter().any(|f| f.name == "Photos"),
                    "Photos not found in: {folders:?}"
                );
            }
            other => panic!("expected Folders, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_list_folders_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(CliRequest::ListFolders, &ctx);

        match resp {
            CliResponse::Folders { folders } => assert!(folders.is_empty()),
            other => panic!("expected Folders, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_subscribe_folder() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create a folder first.
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "Docs".to_string(),
                local_path: None,
                ignore_patterns: None,
            },
            &ctx,
        );
        let folder_id_hex = match resp {
            CliResponse::Ok { message } => {
                // Extract folder ID from message: "Folder created: Docs (<hex>)"
                let paren_start = message.find('(').unwrap();
                let paren_end = message.find(')').unwrap();
                message[paren_start + 1..paren_end].to_string()
            }
            other => panic!("expected Ok, got {other:?}"),
        };

        // Creator is already subscribed, but we can verify via ListFolders.
        let resp2 = process_request(CliRequest::ListFolders, &ctx);
        match resp2 {
            CliResponse::Folders { folders } => {
                let f = folders.iter().find(|f| f.name == "Docs").unwrap();
                assert!(f.subscribed, "creator should be auto-subscribed");
                assert_eq!(f.mode.as_deref(), Some("full"));
            }
            other => panic!("expected Folders, got {other:?}"),
        }

        // Unsubscribe, then re-subscribe as read-only.
        let _ = process_request(
            CliRequest::UnsubscribeFolder {
                folder_id_hex: folder_id_hex.clone(),
                keep_local: false,
            },
            &ctx,
        );

        let resp3 = process_request(
            CliRequest::SubscribeFolder {
                folder_id_hex: folder_id_hex.clone(),
                name: None,
                local_path: "/tmp/test".to_string(),
                mode: "read-only".to_string(),
            },
            &ctx,
        );
        match resp3 {
            CliResponse::Ok { message } => {
                assert!(message.contains("Subscribed"), "got: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        // Verify read-only subscription.
        let resp4 = process_request(CliRequest::ListFolders, &ctx);
        match resp4 {
            CliResponse::Folders { folders } => {
                let f = folders.iter().find(|f| f.name == "Docs").unwrap();
                assert!(f.subscribed);
                assert_eq!(f.mode.as_deref(), Some("receive-only"));
            }
            other => panic!("expected Folders, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_folder_status() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create folder.
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "Music".to_string(),
                local_path: None,
                ignore_patterns: None,
            },
            &ctx,
        );
        let folder_id_hex = match resp {
            CliResponse::Ok { message } => {
                let paren_start = message.find('(').unwrap();
                let paren_end = message.find(')').unwrap();
                message[paren_start + 1..paren_end].to_string()
            }
            other => panic!("expected Ok, got {other:?}"),
        };

        let resp2 = process_request(
            CliRequest::FolderStatus {
                folder_id_hex: folder_id_hex.clone(),
            },
            &ctx,
        );
        match resp2 {
            CliResponse::FolderStatus {
                name,
                file_count,
                conflict_count,
                sync_status,
                ..
            } => {
                assert_eq!(name, "Music");
                assert_eq!(file_count, 0);
                assert_eq!(conflict_count, 0);
                assert_eq!(sync_status, "empty");
            }
            other => panic!("expected FolderStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_folder_status_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::FolderStatus {
                folder_id_hex: "aa".repeat(32),
            },
            &ctx,
        );
        match resp {
            CliResponse::Error { message } => {
                assert!(message.contains("not found"), "got: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_list_conflicts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::ListConflicts {
                folder_id_hex: None,
            },
            &ctx,
        );
        match resp {
            CliResponse::Conflicts { conflicts } => assert!(conflicts.is_empty()),
            other => panic!("expected Conflicts, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_resolve_nonexistent_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::ResolveConflict {
                folder_id_hex: "aa".repeat(32),
                path: "test.txt".to_string(),
                chosen_hash_hex: "bb".repeat(32),
            },
            &ctx,
        );
        match resp {
            CliResponse::Error { .. } => {} // Expected error.
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_file_history_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::FileHistory {
                folder_id_hex: "aa".repeat(32),
                path: "test.txt".to_string(),
            },
            &ctx,
        );
        match resp {
            CliResponse::FileVersions { versions } => assert!(versions.is_empty()),
            other => panic!("expected FileVersions, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_set_folder_mode() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create a folder.
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "Setmode".to_string(),
                local_path: None,
                ignore_patterns: None,
            },
            &ctx,
        );
        let folder_id_hex = match resp {
            CliResponse::Ok { message } => {
                let paren_start = message.find('(').unwrap();
                let paren_end = message.find(')').unwrap();
                message[paren_start + 1..paren_end].to_string()
            }
            other => panic!("expected Ok, got {other:?}"),
        };

        // Change mode to read-only.
        let resp2 = process_request(
            CliRequest::SetFolderMode {
                folder_id_hex: folder_id_hex.clone(),
                mode: "read-only".to_string(),
            },
            &ctx,
        );
        match resp2 {
            CliResponse::Ok { message } => {
                assert!(message.contains("Receive only"), "got: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        // Verify mode changed.
        let resp3 = process_request(CliRequest::ListFolders, &ctx);
        match resp3 {
            CliResponse::Folders { folders } => {
                let f = folders.iter().find(|f| f.name == "Setmode").unwrap();
                assert_eq!(f.mode.as_deref(), Some("receive-only"));
            }
            other => panic!("expected Folders, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_subscribe_nonexistent_folder() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::SubscribeFolder {
                folder_id_hex: "cc".repeat(32),
                name: None,
                local_path: "/tmp/test".to_string(),
                mode: "read-write".to_string(),
            },
            &ctx,
        );
        match resp {
            CliResponse::Error { .. } => {} // Expected: folder doesn't exist.
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_folder_files_after_add() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create a folder.
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "TestFiles".to_string(),
                local_path: None,
                ignore_patterns: None,
            },
            &ctx,
        );
        let folder_id_hex = match resp {
            CliResponse::Ok { message } => {
                let paren_start = message.find('(').unwrap();
                let paren_end = message.find(')').unwrap();
                message[paren_start + 1..paren_end].to_string()
            }
            other => panic!("expected Ok, got {other:?}"),
        };

        // Add a file via engine directly to this folder.
        {
            let mut eng = ctx.engine.lock().unwrap();
            let folder_id = parse_folder_id(&folder_id_hex).unwrap();
            let data = b"test content";
            let blob_hash = murmur_types::BlobHash::from_data(data);
            let meta = murmur_types::FileMetadata {
                blob_hash,
                folder_id,
                path: "hello.txt".to_string(),
                size: data.len() as u64,
                mime_type: Some("text/plain".to_string()),
                created_at: 1000,
                modified_at: 1000,
                device_origin: eng.device_id(),
            };
            eng.add_file(meta, data.to_vec()).unwrap();
        }

        // List folder files.
        let resp2 = process_request(
            CliRequest::FolderFiles {
                folder_id_hex: folder_id_hex.clone(),
            },
            &ctx,
        );
        match resp2 {
            CliResponse::Files { files } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, "hello.txt");
            }
            other => panic!("expected Files, got {other:?}"),
        }
    }

    #[test]
    fn test_event_broadcast_channel() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Subscribe to events.
        let mut rx = ctx.event_broadcast.subscribe();

        // Send an event through the broadcast channel.
        let event = murmur_engine::EngineEvent::DagSynced { new_entries: 5 };
        ctx.event_broadcast.send(event.clone()).unwrap();

        // Receive the event.
        let received = rx.blocking_recv().unwrap();
        assert_eq!(received, event);
    }

    #[test]
    fn test_engine_event_to_ipc_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let event = murmur_engine::EngineEvent::FolderCreated {
            folder_id: FolderId::from_bytes([0xaa; 32]),
            name: "TestFolder".to_string(),
        };

        let ipc = engine_event_to_ipc(&event, &ctx);
        assert_eq!(ipc.event_type, "folder_created");
        assert!(ipc.data.contains("TestFolder"));
    }

    #[test]
    fn test_parse_folder_id_valid() {
        let hex = "ab".repeat(32);
        let id = parse_folder_id(&hex).unwrap();
        assert_eq!(id, FolderId::from_bytes([0xab; 32]));
    }

    #[test]
    fn test_parse_folder_id_invalid() {
        assert!(parse_folder_id("too-short").is_err());
        assert!(parse_folder_id(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn test_parse_blob_hash_valid() {
        let hex = "cd".repeat(32);
        let hash = parse_blob_hash(&hex).unwrap();
        assert_eq!(hash, BlobHash::from_bytes([0xcd; 32]));
    }

    #[test]
    fn test_parse_blob_hash_invalid() {
        assert!(parse_blob_hash("too-short").is_err());
    }

    #[test]
    fn test_escape_json_special_chars() {
        assert_eq!(escape_json("hello"), "hello");
        assert_eq!(escape_json(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_json("back\\slash"), "back\\\\slash");
        assert_eq!(escape_json("line\nbreak"), "line\\nbreak");
        assert_eq!(escape_json("tab\there"), "tab\\there");
        assert_eq!(escape_json("return\rhere"), "return\\rhere");
    }

    #[test]
    fn test_engine_event_to_ipc_escapes_path() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Path with double quotes — must produce valid JSON.
        let event = murmur_engine::EngineEvent::FileSynced {
            blob_hash: BlobHash::from_bytes([0xaa; 32]),
            folder_id: FolderId::from_bytes([0xbb; 32]),
            path: r#"dir/he said "hello".txt"#.to_string(),
        };

        let ipc = engine_event_to_ipc(&event, &ctx);
        assert_eq!(ipc.event_type, "file_synced");
        // The data field must be valid JSON — no unescaped quotes.
        assert!(ipc.data.contains(r#"\"hello\""#), "got: {}", ipc.data);
    }

    #[test]
    fn test_find_version_info_from_dag() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create a folder and add a file.
        let folder_id_hex;
        let blob_hash;
        let file_hlc;
        {
            let mut eng = ctx.engine.lock().unwrap();
            let (folder, _) = eng.create_folder("History").unwrap();
            folder_id_hex = folder.folder_id.to_string();

            let data = b"version one";
            blob_hash = murmur_types::BlobHash::from_data(data);
            let meta = murmur_types::FileMetadata {
                blob_hash,
                folder_id: folder.folder_id,
                path: "notes.txt".to_string(),
                size: data.len() as u64,
                mime_type: None,
                created_at: 1000,
                modified_at: 1000,
                device_origin: eng.device_id(),
            };
            eng.add_file(meta, data.to_vec()).unwrap();

            // Get the HLC from file history.
            let history = eng.file_history(folder.folder_id, "notes.txt");
            assert_eq!(history.len(), 1);
            file_hlc = history[0].1;
        }

        // Now query file history via IPC and verify we get real device info.
        let resp = process_request(
            CliRequest::FileHistory {
                folder_id_hex,
                path: "notes.txt".to_string(),
            },
            &ctx,
        );
        match resp {
            CliResponse::FileVersions { versions } => {
                assert_eq!(versions.len(), 1);
                let v = &versions[0];
                assert_eq!(v.blob_hash, blob_hash.to_string());
                assert_eq!(v.modified_at, file_hlc);
                // Device name should be resolved, not "unknown".
                assert_eq!(v.device_name, "TestDaemon");
                assert_ne!(v.device_id, "unknown");
                assert!(v.size > 0);
            }
            other => panic!("expected FileVersions, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_list_conflicts_with_folder_filter() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create two folders.
        let resp1 = process_request(
            CliRequest::CreateFolder {
                name: "A".to_string(),
                local_path: None,
                ignore_patterns: None,
            },
            &ctx,
        );
        let fid_a = match resp1 {
            CliResponse::Ok { message } => {
                let s = message.find('(').unwrap();
                let e = message.find(')').unwrap();
                message[s + 1..e].to_string()
            }
            other => panic!("expected Ok, got {other:?}"),
        };

        // List conflicts filtered by folder A — should return empty, not error.
        let resp2 = process_request(
            CliRequest::ListConflicts {
                folder_id_hex: Some(fid_a),
            },
            &ctx,
        );
        match resp2 {
            CliResponse::Conflicts { conflicts } => assert!(conflicts.is_empty()),
            other => panic!("expected Conflicts, got {other:?}"),
        }

        // Invalid folder hex should return error.
        let resp3 = process_request(
            CliRequest::ListConflicts {
                folder_id_hex: Some("bad-hex".to_string()),
            },
            &ctx,
        );
        assert!(matches!(resp3, CliResponse::Error { .. }));
    }

    #[test]
    fn test_ipc_set_folder_name() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create a folder with a local path so it appears in config.
        let local = dir.path().join("sync");
        std::fs::create_dir_all(&local).unwrap();
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "Original".to_string(),
                local_path: Some(local.to_string_lossy().to_string()),
                ignore_patterns: None,
            },
            &ctx,
        );
        let folder_id_hex = match resp {
            CliResponse::Ok { message } => {
                let s = message.find('(').unwrap();
                let e = message.find(')').unwrap();
                message[s + 1..e].to_string()
            }
            other => panic!("expected Ok, got {other:?}"),
        };

        // Rename the folder.
        let resp2 = process_request(
            CliRequest::SetFolderName {
                folder_id_hex: folder_id_hex.clone(),
                name: "Renamed".to_string(),
            },
            &ctx,
        );
        match resp2 {
            CliResponse::Ok { message } => {
                assert!(message.contains("Renamed"), "got: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        // Verify renamed name appears in folder list.
        let resp3 = process_request(CliRequest::ListFolders, &ctx);
        match resp3 {
            CliResponse::Folders { folders } => {
                let f = folders
                    .iter()
                    .find(|f| f.folder_id == folder_id_hex)
                    .unwrap();
                assert_eq!(f.name, "Renamed");
            }
            other => panic!("expected Folders, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_folder_list_includes_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Create a folder with a local path.
        let local = dir.path().join("photos");
        std::fs::create_dir_all(&local).unwrap();
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "Photos".to_string(),
                local_path: Some(local.to_string_lossy().to_string()),
                ignore_patterns: None,
            },
            &ctx,
        );
        let folder_id_hex = match resp {
            CliResponse::Ok { message } => {
                let s = message.find('(').unwrap();
                let e = message.find(')').unwrap();
                message[s + 1..e].to_string()
            }
            other => panic!("expected Ok, got {other:?}"),
        };

        // Verify local_path appears in folder list.
        let resp2 = process_request(CliRequest::ListFolders, &ctx);
        match resp2 {
            CliResponse::Folders { folders } => {
                let f = folders
                    .iter()
                    .find(|f| f.folder_id == folder_id_hex)
                    .unwrap();
                assert_eq!(
                    f.local_path.as_deref(),
                    Some(local.to_string_lossy().as_ref())
                );
            }
            other => panic!("expected Folders, got {other:?}"),
        }
    }

    #[test]
    fn test_folders_survive_daemon_restart() {
        let dir = tempfile::tempdir().unwrap();

        // Phase 1: create context, create a folder with local_path, verify.
        let mnemonic = murmur_seed::generate_mnemonic(murmur_seed::WordCount::Twelve);
        let identity = murmur_seed::NetworkIdentity::from_mnemonic(&mnemonic, "");
        let device_id = identity.first_device_id();
        let signing_key = identity.first_device_signing_key().clone();
        let network_id_hex = identity.network_id().to_string();

        let local = dir.path().join("sync");
        std::fs::create_dir_all(&local).unwrap();

        let folder_id_hex;
        let config_path = dir.path().join("config.toml");
        {
            let storage =
                Arc::new(Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap());
            let platform = Arc::new(FjallPlatform::new(storage.clone()));

            let engine = murmur_engine::MurmurEngine::create_network(
                device_id,
                signing_key.clone(),
                "TestDaemon".to_string(),
                platform,
            );

            let (broadcast_tx, _rx) = mpsc::unbounded_channel();
            let (event_broadcast, _) =
                tokio::sync::broadcast::channel::<murmur_engine::EngineEvent>(16);

            let config = Config::new(dir.path(), "TestDaemon");
            let ctx = DaemonCtx {
                engine: Arc::new(Mutex::new(engine)),
                storage: storage.clone(),
                device_name: "TestDaemon".to_string(),
                network_id_hex: network_id_hex.clone(),
                mnemonic: mnemonic.to_string(),
                start_time: Instant::now(),
                broadcast_tx,
                connected_peers: Arc::new(AtomicU64::new(0)),
                event_broadcast,
                synced_folders: Arc::new(std::collections::HashMap::new()),
                base_dir: dir.path().to_path_buf(),
                config: Arc::new(Mutex::new(config)),
                sync_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                paused_folders: Arc::new(Mutex::new(std::collections::HashSet::new())),
                device_presence: Arc::new(Mutex::new(std::collections::HashMap::new())),
                endpoint: None,
                used_invite_nonces: Arc::new(Mutex::new(std::collections::HashSet::new())),
                signing_key: Arc::new(signing_key.clone()),
                device_id,
                transfer_tracker: Arc::new(TransferTracker::new()),
            };

            // Create a folder with a local path.
            let resp = process_request(
                CliRequest::CreateFolder {
                    name: "Photos".to_string(),
                    local_path: Some(local.to_string_lossy().to_string()),
                    ignore_patterns: None,
                },
                &ctx,
            );
            folder_id_hex = match resp {
                CliResponse::Ok { message } => {
                    let s = message.find('(').unwrap();
                    let e = message.find(')').unwrap();
                    message[s + 1..e].to_string()
                }
                other => panic!("expected Ok, got {other:?}"),
            };

            // Verify folder exists before restart.
            let resp2 = process_request(CliRequest::ListFolders, &ctx);
            match resp2 {
                CliResponse::Folders { folders } => {
                    assert_eq!(folders.len(), 1);
                    assert_eq!(folders[0].name, "Photos");
                    assert!(folders[0].subscribed);
                    assert_eq!(
                        folders[0].local_path.as_deref(),
                        Some(local.to_string_lossy().as_ref())
                    );
                }
                other => panic!("expected Folders, got {other:?}"),
            }

            // Save config.
            let cfg = ctx.config.lock().unwrap();
            cfg.save(&config_path).unwrap();

            // Flush storage to persist DAG entries.
            storage.flush().unwrap();
        }
        // Everything dropped — simulates daemon stop.

        // Phase 2: reopen storage, reload DAG entries, rebuild engine.
        {
            let storage =
                Arc::new(Storage::open(&dir.path().join("db"), &dir.path().join("blobs")).unwrap());
            let platform = Arc::new(FjallPlatform::new(storage.clone()));

            let persisted_entries = storage.load_all_dag_entries().unwrap();
            assert!(
                !persisted_entries.is_empty(),
                "should have persisted DAG entries"
            );

            let dag = murmur_dag::Dag::new(device_id, signing_key.clone());
            let mut engine = murmur_engine::MurmurEngine::from_dag(dag, platform);
            for entry_bytes in &persisted_entries {
                engine.load_entry_bytes(entry_bytes).unwrap();
            }
            engine.rebuild_conflicts();

            let (broadcast_tx, _rx) = mpsc::unbounded_channel();
            let (event_broadcast, _) =
                tokio::sync::broadcast::channel::<murmur_engine::EngineEvent>(16);

            let config = Config::load(&config_path).unwrap();
            let ctx = DaemonCtx {
                engine: Arc::new(Mutex::new(engine)),
                storage,
                device_name: "TestDaemon".to_string(),
                network_id_hex,
                mnemonic: mnemonic.to_string(),
                start_time: Instant::now(),
                broadcast_tx,
                connected_peers: Arc::new(AtomicU64::new(0)),
                event_broadcast,
                synced_folders: Arc::new(std::collections::HashMap::new()),
                base_dir: dir.path().to_path_buf(),
                config: Arc::new(Mutex::new(config)),
                sync_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                paused_folders: Arc::new(Mutex::new(std::collections::HashSet::new())),
                device_presence: Arc::new(Mutex::new(std::collections::HashMap::new())),
                endpoint: None,
                used_invite_nonces: Arc::new(Mutex::new(std::collections::HashSet::new())),
                signing_key: Arc::new(signing_key),
                device_id,
                transfer_tracker: Arc::new(TransferTracker::new()),
            };

            // Verify folder survives restart.
            let resp = process_request(CliRequest::ListFolders, &ctx);
            match resp {
                CliResponse::Folders { folders } => {
                    assert!(
                        !folders.is_empty(),
                        "folders should survive daemon restart, got empty list"
                    );
                    let f = folders
                        .iter()
                        .find(|f| f.folder_id == folder_id_hex)
                        .expect("Photos folder should exist after restart");
                    assert_eq!(f.name, "Photos");
                    assert!(f.subscribed, "should still be subscribed after restart");
                    assert_eq!(
                        f.local_path.as_deref(),
                        Some(local.to_string_lossy().as_ref()),
                        "local_path should survive restart"
                    );
                }
                other => panic!("expected Folders, got {other:?}"),
            }
        }
    }

    // -- Onboarding: pairing invites --

    #[test]
    fn test_issue_pairing_invite_returns_valid_url() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(CliRequest::IssuePairingInvite, &ctx);
        match resp {
            CliResponse::PairingInvite {
                url,
                expires_at_unix,
            } => {
                assert!(url.starts_with("murmur://join?token="));
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                assert!(expires_at_unix > now);
                assert!(expires_at_unix <= now + 301); // default 5 min + slack
                // URL should parse back into a valid token.
                let t = murmur_seed::PairingToken::from_url(&url).expect("parse");
                assert_eq!(t.expires_at_unix, expires_at_unix);
            }
            other => panic!("expected PairingInvite, got {other:?}"),
        }
    }

    #[test]
    fn test_issued_invite_redeems_to_same_mnemonic() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let expected_mnemonic = ctx.mnemonic.clone();

        let url = match process_request(CliRequest::IssuePairingInvite, &ctx) {
            CliResponse::PairingInvite { url, .. } => url,
            other => panic!("expected PairingInvite, got {other:?}"),
        };

        let resp = process_request(CliRequest::RedeemPairingInvite { url }, &ctx);
        match resp {
            CliResponse::RedeemedMnemonic {
                mnemonic,
                issued_by,
            } => {
                assert_eq!(mnemonic, expected_mnemonic);
                assert_eq!(issued_by, ctx.device_id.to_string());
            }
            other => panic!("expected RedeemedMnemonic, got {other:?}"),
        }
    }

    #[test]
    fn test_redeem_rejects_replay() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let url = match process_request(CliRequest::IssuePairingInvite, &ctx) {
            CliResponse::PairingInvite { url, .. } => url,
            other => panic!("got {other:?}"),
        };

        // First redeem succeeds.
        let first = process_request(CliRequest::RedeemPairingInvite { url: url.clone() }, &ctx);
        assert!(matches!(first, CliResponse::RedeemedMnemonic { .. }));

        // Second redeem of the same URL is rejected.
        let second = process_request(CliRequest::RedeemPairingInvite { url }, &ctx);
        match second {
            CliResponse::Error { message } => {
                assert!(
                    message.contains("already redeemed"),
                    "unexpected error text: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_redeem_rejects_bad_url() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::RedeemPairingInvite {
                url: "not-a-valid-url".to_string(),
            },
            &ctx,
        );
        match resp {
            CliResponse::Error { message } => {
                assert!(message.contains("invalid invite URL"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_create_folder_writes_template_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("proj");
        std::fs::create_dir_all(&local).unwrap();
        let ctx = test_ctx(dir.path());

        let patterns = "target/\nCargo.lock\n".to_string();
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "Proj".to_string(),
                local_path: Some(local.to_string_lossy().to_string()),
                ignore_patterns: Some(patterns.clone()),
            },
            &ctx,
        );
        assert!(matches!(resp, CliResponse::Ok { .. }));

        let written = std::fs::read_to_string(local.join(".murmurignore")).unwrap();
        assert_eq!(written, patterns);
    }

    #[test]
    fn test_create_folder_without_template_writes_no_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("bare");
        std::fs::create_dir_all(&local).unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::CreateFolder {
                name: "Bare".to_string(),
                local_path: Some(local.to_string_lossy().to_string()),
                ignore_patterns: None,
            },
            &ctx,
        );
        assert!(matches!(resp, CliResponse::Ok { .. }));

        assert!(!local.join(".murmurignore").exists());
    }

    // -----------------------------------------------------------------------
    // M29 — Conflict Resolution Improvements
    // -----------------------------------------------------------------------

    /// Build a folder + seeded conflict inside the engine, and register it
    /// in both engine state and daemon config so M29 handlers see a normal
    /// pair-of-versions scenario.
    ///
    /// Returns `(folder_id_hex, conflict_path)`.
    fn seed_conflict(ctx: &DaemonCtx) -> (String, String) {
        use murmur_types::{BlobHash, ConflictInfo, ConflictVersion};

        // Create a folder and subscribe via the IPC path so the config reflects
        // reality.
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "M29Folder".to_string(),
                local_path: None,
                ignore_patterns: None,
            },
            ctx,
        );
        let folder_id_hex = match resp {
            CliResponse::Ok { message } => {
                let paren_start = message.find('(').unwrap();
                let paren_end = message.find(')').unwrap();
                message[paren_start + 1..paren_end].to_string()
            }
            other => panic!("expected Ok from CreateFolder, got {other:?}"),
        };

        // Also register in config so M29 handlers (SetFolderAutoResolve,
        // SetConflictExpiry, expire_conflicts) find a matching FolderConfig.
        {
            let mut cfg = ctx.config.lock().unwrap();
            cfg.folders.push(config::FolderConfig {
                folder_id: folder_id_hex.clone(),
                name: "M29Folder".to_string(),
                local_path: ctx.base_dir.join("m29"),
                mode: "full".to_string(),
                auto_resolve: "none".to_string(),
                conflict_expiry_days: None,
                color_hex: None,
                icon: None,
            });
        }

        // Inject a synthetic conflict directly into engine state. The two
        // versions are crafted so "newest" picks the higher-HLC one and
        // "mine" picks the local device's version.
        let path = "readme.txt".to_string();
        let folder_id = parse_folder_id(&folder_id_hex).unwrap();
        let local_device = ctx.engine.lock().unwrap().device_id();
        let remote_device = DeviceId::from_bytes([0x77; 32]);

        let local_bytes: &[u8] = b"local version content\n";
        let remote_bytes: &[u8] = b"remote version content\n";
        let local_hash = BlobHash::from_data(local_bytes);
        let remote_hash = BlobHash::from_data(remote_bytes);

        // Store the blobs so ConflictDiff can load them.
        ctx.storage.store_blob(local_hash, local_bytes).unwrap();
        ctx.storage.store_blob(remote_hash, remote_bytes).unwrap();

        // Register the remote device so ConflictDiff can resolve its name.
        {
            let mut eng = ctx.engine.lock().unwrap();
            let state = eng.state_mut();
            state.devices.insert(
                remote_device,
                murmur_types::DeviceInfo {
                    device_id: remote_device,
                    name: "Remote".to_string(),
                    approved: true,
                    approved_by: Some(local_device),
                    joined_at: 0,
                },
            );
            state.conflicts.push(ConflictInfo {
                folder_id,
                path: path.clone(),
                versions: vec![
                    ConflictVersion {
                        blob_hash: local_hash,
                        device_id: local_device,
                        hlc: 1_000,
                        dag_entry_hash: [0u8; 32],
                    },
                    ConflictVersion {
                        blob_hash: remote_hash,
                        device_id: remote_device,
                        hlc: 2_000,
                        dag_entry_hash: [0u8; 32],
                    },
                ],
                // `0` is interpreted as "epoch" so any positive `expiry_days`
                // immediately marks this conflict as expired. Tests that
                // require "not yet expired" override this explicitly.
                detected_at: 0,
            });
        }

        (folder_id_hex, path)
    }

    #[test]
    fn test_is_utf8_text_utf8_passes() {
        assert!(is_utf8_text(b"hello world\n"));
        assert!(is_utf8_text("héllo".as_bytes()));
    }

    #[test]
    fn test_is_utf8_text_binary_fails() {
        // Non-UTF-8 bytes.
        assert!(!is_utf8_text(&[0xff, 0xfe, 0x00, 0x01]));
        // Valid UTF-8 but contains a NUL — treated as binary.
        assert!(!is_utf8_text(b"hello\0world"));
    }

    #[test]
    fn test_ipc_conflict_diff_text() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let (folder_id_hex, path) = seed_conflict(&ctx);

        let resp = process_request(
            CliRequest::ConflictDiff {
                folder_id_hex,
                path,
            },
            &ctx,
        );
        match resp {
            CliResponse::ConflictDiff {
                is_text,
                left,
                right,
            } => {
                assert!(is_text, "text blobs should be detected as text");
                assert_eq!(left.bytes, b"local version content\n");
                assert_eq!(right.bytes, b"remote version content\n");
                assert_eq!(right.device_name, "Remote");
            }
            other => panic!("expected ConflictDiff, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_conflict_diff_binary() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        // Build a conflict with binary blobs (the seed_conflict helper writes
        // text; we need raw bytes here).
        let resp = process_request(
            CliRequest::CreateFolder {
                name: "BinFolder".to_string(),
                local_path: None,
                ignore_patterns: None,
            },
            &ctx,
        );
        let folder_id_hex = match resp {
            CliResponse::Ok { message } => {
                let paren_start = message.find('(').unwrap();
                let paren_end = message.find(')').unwrap();
                message[paren_start + 1..paren_end].to_string()
            }
            _ => unreachable!(),
        };
        let folder_id = parse_folder_id(&folder_id_hex).unwrap();

        let left = vec![0xffu8, 0xfe, 0x00, 0x01, 0x02];
        let right = vec![0xffu8, 0xfe, 0x00, 0x03, 0x04];
        let left_hash = BlobHash::from_data(&left);
        let right_hash = BlobHash::from_data(&right);
        ctx.storage.store_blob(left_hash, &left).unwrap();
        ctx.storage.store_blob(right_hash, &right).unwrap();

        {
            let mut eng = ctx.engine.lock().unwrap();
            let device_id = eng.device_id();
            let state = eng.state_mut();
            state.conflicts.push(murmur_types::ConflictInfo {
                folder_id,
                path: "pic.bin".to_string(),
                versions: vec![
                    murmur_types::ConflictVersion {
                        blob_hash: left_hash,
                        device_id,
                        hlc: 1,
                        dag_entry_hash: [0u8; 32],
                    },
                    murmur_types::ConflictVersion {
                        blob_hash: right_hash,
                        device_id,
                        hlc: 2,
                        dag_entry_hash: [0u8; 32],
                    },
                ],
                detected_at: 0,
            });
        }

        let resp = process_request(
            CliRequest::ConflictDiff {
                folder_id_hex,
                path: "pic.bin".to_string(),
            },
            &ctx,
        );
        match resp {
            CliResponse::ConflictDiff { is_text, .. } => {
                assert!(!is_text, "binary blobs should NOT be detected as text");
            }
            other => panic!("expected ConflictDiff, got {other:?}"),
        }
    }

    #[test]
    fn test_set_conflict_expiry_persists_to_config() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let (folder_id_hex, _) = seed_conflict(&ctx);

        let resp = process_request(
            CliRequest::SetConflictExpiry {
                folder_id_hex: folder_id_hex.clone(),
                days: 7,
            },
            &ctx,
        );
        assert!(matches!(resp, CliResponse::Ok { .. }));

        let cfg = ctx.config.lock().unwrap();
        let fc = cfg
            .folders
            .iter()
            .find(|f| f.folder_id == folder_id_hex)
            .unwrap();
        assert_eq!(fc.conflict_expiry_days, Some(7));

        drop(cfg);
        // `0` disables.
        let resp2 = process_request(
            CliRequest::SetConflictExpiry {
                folder_id_hex: folder_id_hex.clone(),
                days: 0,
            },
            &ctx,
        );
        assert!(matches!(resp2, CliResponse::Ok { .. }));
        let cfg2 = ctx.config.lock().unwrap();
        let fc2 = cfg2
            .folders
            .iter()
            .find(|f| f.folder_id == folder_id_hex)
            .unwrap();
        assert_eq!(fc2.conflict_expiry_days, None);
    }

    #[test]
    fn test_set_folder_auto_resolve_validates_strategy() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let (folder_id_hex, _) = seed_conflict(&ctx);

        // Valid strategy is accepted + persisted.
        let resp = process_request(
            CliRequest::SetFolderAutoResolve {
                folder_id_hex: folder_id_hex.clone(),
                strategy: "newest".to_string(),
            },
            &ctx,
        );
        assert!(matches!(resp, CliResponse::Ok { .. }));
        let cfg = ctx.config.lock().unwrap();
        assert_eq!(
            cfg.folders
                .iter()
                .find(|f| f.folder_id == folder_id_hex)
                .unwrap()
                .auto_resolve,
            "newest"
        );
        drop(cfg);

        // Garbage strategy is rejected.
        let resp2 = process_request(
            CliRequest::SetFolderAutoResolve {
                folder_id_hex,
                strategy: "bogus".to_string(),
            },
            &ctx,
        );
        assert!(matches!(resp2, CliResponse::Error { .. }));
    }

    #[test]
    fn test_expire_conflicts_newest_strategy_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let (folder_id_hex, path) = seed_conflict(&ctx);

        // Configure: auto-resolve=newest, expiry=0 (immediately).
        {
            let mut cfg = ctx.config.lock().unwrap();
            let fc = cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
                .unwrap();
            fc.auto_resolve = "newest".to_string();
            fc.conflict_expiry_days = Some(0);
        }

        // Subscribe to events so we can verify ConflictAutoResolved fires.
        let mut event_rx = ctx.event_broadcast.subscribe();

        expire_conflicts(&ctx);

        // Conflict list is now empty.
        let folder_id = parse_folder_id(&folder_id_hex).unwrap();
        let eng = ctx.engine.lock().unwrap();
        let remaining = eng.list_conflicts_in_folder(folder_id);
        assert!(
            remaining.is_empty(),
            "expected no conflicts after expiry tick, got {remaining:?}"
        );
        drop(eng);

        // Event was emitted.
        let event = event_rx
            .try_recv()
            .expect("expected ConflictAutoResolved event");
        match event {
            murmur_engine::EngineEvent::ConflictAutoResolved {
                strategy, path: ep, ..
            } => {
                assert_eq!(strategy, "newest");
                assert_eq!(ep, path);
            }
            other => panic!("expected ConflictAutoResolved, got {other:?}"),
        }
    }

    #[test]
    fn test_expire_conflicts_keep_both_when_strategy_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let (folder_id_hex, _) = seed_conflict(&ctx);

        // auto_resolve stays "none" (from seed); enable immediate expiry.
        {
            let mut cfg = ctx.config.lock().unwrap();
            let fc = cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
                .unwrap();
            fc.conflict_expiry_days = Some(0);
        }
        let mut event_rx = ctx.event_broadcast.subscribe();

        expire_conflicts(&ctx);

        let folder_id = parse_folder_id(&folder_id_hex).unwrap();
        let eng = ctx.engine.lock().unwrap();
        assert!(eng.list_conflicts_in_folder(folder_id).is_empty());
        drop(eng);

        let event = event_rx
            .try_recv()
            .expect("expected ConflictAutoResolved event");
        match event {
            murmur_engine::EngineEvent::ConflictAutoResolved { strategy, .. } => {
                assert_eq!(
                    strategy, "keep_both",
                    "strategy=none should fall back to keep_both"
                );
            }
            other => panic!("expected ConflictAutoResolved, got {other:?}"),
        }
    }

    #[test]
    fn test_expire_conflicts_respects_not_yet_expired() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let (folder_id_hex, _) = seed_conflict(&ctx);

        // Bump the conflict's detected_at to *now* so any non-zero expiry
        // window will not have elapsed yet.
        let folder_id = parse_folder_id(&folder_id_hex).unwrap();
        {
            let mut eng = ctx.engine.lock().unwrap();
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
            for c in eng
                .state_mut()
                .conflicts
                .iter_mut()
                .filter(|c| c.folder_id == folder_id)
            {
                c.detected_at = now_ns;
            }
        }
        {
            let mut cfg = ctx.config.lock().unwrap();
            let fc = cfg
                .folders
                .iter_mut()
                .find(|f| f.folder_id == folder_id_hex)
                .unwrap();
            fc.auto_resolve = "newest".to_string();
            fc.conflict_expiry_days = Some(7);
        }

        expire_conflicts(&ctx);

        let eng = ctx.engine.lock().unwrap();
        assert_eq!(
            eng.list_conflicts_in_folder(folder_id).len(),
            1,
            "recently-detected conflict should NOT be auto-resolved"
        );
    }
}
