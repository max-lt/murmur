//! `murmurd` — Murmur headless backup daemon.
//!
//! Runs as a pure daemon with no subcommands. Management is done via
//! `murmur-cli` which connects over a Unix domain socket.

mod config;
mod crypto;
#[cfg(feature = "metrics")]
mod http;
mod mdns;
mod metrics;
mod networking;
mod storage;

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use murmur_ipc::{CliRequest, CliResponse, DeviceInfoIpc, FileInfoIpc, TransferInfoIpc};
use murmur_types::DeviceId;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use zeroize::Zeroize;

use config::Config;
use storage::{FjallPlatform, Storage};

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
    let platform = Arc::new(FjallPlatform::new(storage.clone()));

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
        info!(
            entries = persisted_entries.len(),
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
}

/// Handle a single CLI connection.
fn handle_connection(mut stream: std::os::unix::net::UnixStream, ctx: &DaemonCtx) -> Result<()> {
    let request: CliRequest = murmur_ipc::recv_message(&mut stream)?;
    info!(?request, "received CLI request");

    let response = process_request(request, ctx);

    murmur_ipc::send_message(&mut stream, &response)?;
    Ok(())
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
                    // Check blob size on disk for total_bytes.
                    let total_bytes = match ctx.storage.load_blob(blob_hash) {
                        Ok(Some(data)) => data.len() as u64,
                        _ => 0,
                    };
                    TransferInfoIpc {
                        blob_hash: blob_hash.to_string(),
                        bytes_transferred: 0, // pending = not yet transferred
                        total_bytes,
                    }
                })
                .collect();
            CliResponse::TransferStatus { transfers }
        }
        CliRequest::AddFile { path } => {
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
                    // Enqueue blob for push sync to peers.
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

        DaemonCtx {
            engine: Arc::new(Mutex::new(engine)),
            storage,
            device_name: "TestDaemon".to_string(),
            network_id_hex,
            mnemonic: mnemonic.to_string(),
            start_time: Instant::now(),
            broadcast_tx,
            connected_peers: Arc::new(AtomicU64::new(0)),
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
}
