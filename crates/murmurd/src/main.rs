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
mod watcher;

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use murmur_ipc::{
    CliRequest, CliResponse, ConflictInfoIpc, ConflictVersionIpc, DeviceInfoIpc, EngineEventIpc,
    FileInfoIpc, FileVersionIpc, FolderInfoIpc, TransferInfoIpc,
};
use murmur_types::{BlobHash, DeviceId, FolderId, SyncMode};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zeroize::Zeroize;

use config::Config;
use storage::{FjallPlatform, Storage};
use sync::SyncedFolder;

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

    /// Device role: source, backup, or full (only used on first run).
    #[arg(long, default_value = "backup")]
    role: String,

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

    run_daemon(&cli.data_dir, &cli.name, &cli.role, cli.http_port)
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// Run the daemon: load state, listen on socket, handle signals.
///
/// On first run (no config.toml), automatically creates a new network,
/// generates a mnemonic, writes config, and prints the mnemonic.
fn run_daemon(
    base_dir: &Path,
    default_name: &str,
    default_role: &str,
    http_port: Option<u16>,
) -> Result<()> {
    let config_path = Config::config_path(base_dir);

    // Auto-initialize on first run.
    let first_run = !config_path.exists();
    if first_run {
        auto_init(base_dir, default_name, default_role)?;
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
    let (device_id, signing_key) = if device_key_path.exists() {
        let mut raw_bytes = std::fs::read(&device_key_path).context("read device key")?;
        let bytes: [u8; 32] = raw_bytes
            .clone()
            .try_into()
            .map_err(|_| anyhow::anyhow!("device key file must be 32 bytes"))?;
        raw_bytes.zeroize();
        let kp = murmur_seed::DeviceKeyPair::from_bytes(bytes);
        (kp.device_id(), kp.signing_key().clone())
    } else {
        (
            identity.first_device_id(),
            identity.first_device_signing_key().clone(),
        )
    };

    // Open storage with blob encryption.
    let mut storage_inner = Storage::open(&config.storage.data_dir, &config.storage.blob_dir)?;
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
    let device_role = config
        .parse_role()
        .unwrap_or(murmur_types::DeviceRole::Backup);
    let persisted_entries = storage.load_all_dag_entries()?;
    let is_joiner = device_key_path.exists();

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
            device_role,
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

    // Set up socket listener.
    let sock_path = murmur_ipc::socket_path(base_dir);
    cleanup_stale_socket(&sock_path);
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("bind socket at {}", sock_path.display()))?;
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

        let net_handle = networking::start_networking(
            engine.clone(),
            storage.clone(),
            device_id,
            creator_iroh_key,
            is_creator,
            topic,
            config.network.throttle.clone(),
        )
        .await
        .context("start networking")?;

        info!("gossip networking started");

        // Optionally start HTTP health/metrics server.
        #[cfg(feature = "metrics")]
        if let Some(port) = http_port {
            let http_state = http::server::HttpState {
                engine: engine.clone(),
                storage: storage.clone(),
                connected_peers: net_handle.connected_peers.clone(),
                start_time,
            };
            tokio::spawn(http::server::start_http_server(http_state, port));
        }
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
            let mode = match fc.mode.as_str() {
                "read-only" => SyncMode::ReadOnly,
                _ => SyncMode::ReadWrite,
            };

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
        tokio::spawn(async move {
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
        tokio::spawn(async move {
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
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                // Forward to IPC event subscribers (best-effort, ignore if no subscribers).
                let _ = ipc_event_tx_for_forward.send(event.clone());

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

        // Abort the accept loop (drop the listener).
        accept_handle.abort();

        // Stop mDNS if running.
        if let Some(handle) = _mdns_handle {
            handle.shutdown();
        }

        // Close the iroh endpoint gracefully so peers see us leave.
        net_handle.close().await;

        Ok::<(), anyhow::Error>(())
    })?;

    // Cleanup: flush storage and remove socket.
    storage.flush()?;
    let _ = std::fs::remove_file(&sock_path);
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
        CliRequest::ApproveDevice {
            device_id_hex,
            role,
        } => {
            let device_id = match parse_device_id(&device_id_hex) {
                Ok(id) => id,
                Err(e) => {
                    return CliResponse::Error {
                        message: format!("{e:#}"),
                    };
                }
            };
            let device_role = match role.as_str() {
                "source" => murmur_types::DeviceRole::Source,
                "backup" => murmur_types::DeviceRole::Backup,
                "full" => murmur_types::DeviceRole::Full,
                other => {
                    return CliResponse::Error {
                        message: format!("unknown role: {other:?}"),
                    };
                }
            };
            let mut eng = ctx.engine.lock().unwrap();
            match eng.approve_device(device_id, device_role) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after approve");
                    }
                    CliResponse::Ok {
                        message: format!("Device {device_id} approved with role {role}."),
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
            let transfers = items
                .into_iter()
                .map(|(blob_hash, _retry_count)| {
                    let total_bytes = match ctx.storage.load_blob(blob_hash) {
                        Ok(Some(data)) => data.len() as u64,
                        _ => 0,
                    };
                    TransferInfoIpc {
                        blob_hash: blob_hash.to_string(),
                        bytes_transferred: 0,
                        total_bytes,
                    }
                })
                .collect();
            CliResponse::TransferStatus { transfers }
        }
        CliRequest::AddFile { path } => process_add_file(path, ctx),

        // -- Folder management (M17) --
        CliRequest::CreateFolder { name } => {
            let mut eng = ctx.engine.lock().unwrap();
            match eng.create_folder(&name) {
                Ok((folder, entries)) => {
                    for entry in &entries {
                        let _ = ctx.broadcast_tx.send(entry.to_bytes());
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
            let folders = eng
                .list_folders()
                .into_iter()
                .map(|f| {
                    let file_count = eng.folder_files(f.folder_id).len() as u64;
                    let sub = eng
                        .folder_subscriptions(f.folder_id)
                        .into_iter()
                        .find(|s| s.device_id == device_id);
                    FolderInfoIpc {
                        folder_id: f.folder_id.to_string(),
                        name: f.name,
                        created_by: f.created_by.to_string(),
                        file_count,
                        subscribed: sub.is_some(),
                        mode: sub.map(|s| s.mode.to_string()),
                    }
                })
                .collect();
            CliResponse::Folders { folders }
        }
        CliRequest::SubscribeFolder {
            folder_id_hex,
            local_path: _,
            mode,
        } => {
            let folder_id = match parse_folder_id(&folder_id_hex) {
                Ok(id) => id,
                Err(e) => return CliResponse::Error { message: e },
            };
            let sync_mode = match mode.as_str() {
                "read-only" => SyncMode::ReadOnly,
                _ => SyncMode::ReadWrite,
            };
            let mut eng = ctx.engine.lock().unwrap();
            match eng.subscribe_folder(folder_id, sync_mode) {
                Ok(entry) => {
                    let _ = ctx.broadcast_tx.send(entry.to_bytes());
                    if let Err(e) = ctx.storage.flush() {
                        error!(error = %e, "flush after subscribe_folder");
                    }
                    CliResponse::Ok {
                        message: format!("Subscribed to folder {folder_id} as {sync_mode}."),
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
            let sync_mode = match mode.as_str() {
                "read-only" => SyncMode::ReadOnly,
                "read-write" => SyncMode::ReadWrite,
                other => {
                    return CliResponse::Error {
                        message: format!("unknown mode: {other:?} (use read-write or read-only)"),
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
        // SubscribeEvents is handled in handle_connection before we get here.
        CliRequest::SubscribeEvents => CliResponse::Error {
            message: "event streaming handled elsewhere".to_string(),
        },
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

    // Get or create a default folder for the file.
    let folder_id = {
        let folders = eng.list_folders();
        if let Some(f) = folders.first() {
            f.folder_id
        } else {
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
        }
    };

    // Auto-subscribe to the folder if not already subscribed.
    let device_id = eng.device_id();
    let already_subscribed = eng
        .folder_subscriptions(folder_id)
        .iter()
        .any(|s| s.device_id == device_id);
    if !already_subscribed {
        match eng.subscribe_folder(folder_id, murmur_types::SyncMode::ReadWrite) {
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
fn auto_init(base_dir: &Path, name: &str, role: &str) -> Result<()> {
    info!("first run — creating new network");

    std::fs::create_dir_all(base_dir).context("create base directory")?;

    // Generate mnemonic.
    let mnemonic = murmur_seed::generate_mnemonic(murmur_seed::WordCount::TwentyFour);
    let identity = murmur_seed::NetworkIdentity::from_mnemonic(&mnemonic, "");
    let device_id = identity.first_device_id();

    // Save mnemonic.
    std::fs::write(
        Config::mnemonic_path(base_dir),
        mnemonic.to_string().as_bytes(),
    )
    .context("save mnemonic")?;

    // Write config.
    let config = Config::new(base_dir, name, role);
    let toml_str = toml::to_string_pretty(&config).context("serialize config")?;
    std::fs::write(Config::config_path(base_dir), toml_str).context("write config")?;

    println!("New Murmur network created.");
    println!();
    println!("IMPORTANT — Write down your mnemonic and store it safely:");
    println!();
    println!("  {mnemonic}");
    println!();
    println!("Device ID: {device_id}");
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a `DeviceInfo` to IPC format.
fn device_to_ipc(dev: murmur_types::DeviceInfo) -> DeviceInfoIpc {
    DeviceInfoIpc {
        device_id: dev.device_id.to_string(),
        name: dev.name,
        role: format!("{:?}", dev.role).to_lowercase(),
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
fn guess_mime(filename: &str) -> Option<String> {
    let ext = filename.rsplit('.').next()?.to_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => return None,
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
        EngineEvent::DeviceApproved { device_id, role } => EngineEventIpc {
            event_type: "device_approved".to_string(),
            data: format!("{{\"device_id\":\"{device_id}\",\"role\":\"{role:?}\"}}"),
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
    }
}

/// Remove a stale socket file if no process is listening.
fn cleanup_stale_socket(path: &Path) {
    if path.exists() {
        // Try connecting to see if a daemon is running.
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                // Another daemon is running.
                warn!(
                    path = %path.display(),
                    "another murmurd is already running on this socket"
                );
            }
            Err(_) => {
                // Stale socket — remove it.
                info!(path = %path.display(), "removing stale socket");
                let _ = std::fs::remove_file(path);
            }
        }
    }
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

        let engine = murmur_engine::MurmurEngine::create_network(
            device_id,
            signing_key,
            "TestDaemon".to_string(),
            murmur_types::DeviceRole::Backup,
            platform,
        );

        let (broadcast_tx, _rx) = mpsc::unbounded_channel();
        let (event_broadcast, _) =
            tokio::sync::broadcast::channel::<murmur_engine::EngineEvent>(16);

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
                role: "backup".to_string(),
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
    fn test_process_request_approve_invalid_role() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let resp = process_request(
            CliRequest::ApproveDevice {
                device_id_hex: "ff".repeat(32),
                role: "bogus".to_string(),
            },
            &ctx,
        );

        match resp {
            CliResponse::Error { message } => {
                assert!(message.contains("unknown role"), "got: {message}");
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
    fn test_socket_cleanup_stale() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("stale.sock");

        // Create a stale socket file (not a real listener).
        std::fs::write(&sock_path, "stale").unwrap();
        assert!(sock_path.exists());

        cleanup_stale_socket(&sock_path);

        // Stale socket should be removed.
        assert!(!sock_path.exists());
    }

    #[test]
    fn test_auto_init_creates_config_and_mnemonic() {
        let dir = tempfile::tempdir().unwrap();
        auto_init(dir.path(), "TestNAS", "backup").unwrap();

        assert!(Config::config_path(dir.path()).exists());
        assert!(Config::mnemonic_path(dir.path()).exists());
        // First device uses seed key, no device.key file.
        assert!(!Config::device_key_path(dir.path()).exists());

        // Config should be loadable and have correct values.
        let config = Config::load(&Config::config_path(dir.path())).unwrap();
        assert_eq!(config.device.name, "TestNAS");
        assert_eq!(config.device.role, "backup");

        // Mnemonic should be valid.
        let mnemonic_str = std::fs::read_to_string(Config::mnemonic_path(dir.path())).unwrap();
        murmur_seed::parse_mnemonic(mnemonic_str.trim()).unwrap();
    }

    #[test]
    fn test_auto_init_skipped_when_config_exists() {
        let dir = tempfile::tempdir().unwrap();
        auto_init(dir.path(), "First", "backup").unwrap();

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
    fn test_guess_mime() {
        assert_eq!(guess_mime("photo.jpg"), Some("image/jpeg".to_string()));
        assert_eq!(guess_mime("doc.pdf"), Some("application/pdf".to_string()));
        assert_eq!(guess_mime("video.mp4"), Some("video/mp4".to_string()));
        assert_eq!(guess_mime("unknown.xyz"), None);
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
                assert_eq!(f.mode.as_deref(), Some("read-write"));
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
                assert_eq!(f.mode.as_deref(), Some("read-only"));
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
                assert!(message.contains("read-only"), "got: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        // Verify mode changed.
        let resp3 = process_request(CliRequest::ListFolders, &ctx);
        match resp3 {
            CliResponse::Folders { folders } => {
                let f = folders.iter().find(|f| f.name == "Setmode").unwrap();
                assert_eq!(f.mode.as_deref(), Some("read-only"));
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
}
