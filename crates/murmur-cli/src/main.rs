//! `murmur-cli` — CLI tool for managing a running `murmurd` daemon.
//!
//! The `join` command works offline (no daemon required) to set up config for
//! joining an existing network. All other commands connect to `murmurd` via
//! Unix socket.

mod offline;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use murmur_ipc::{CliRequest, CliResponse};

/// Murmur network management CLI.
#[derive(Parser)]
#[command(name = "murmur-cli", about = "Manage a murmurd daemon")]
struct Cli {
    /// Base directory for murmur data.
    #[arg(long, default_value_os_t = murmur_ipc::default_base_dir())]
    data_dir: PathBuf,

    /// Output as JSON instead of plain text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Join an existing Murmur network (offline — no daemon required).
    ///
    /// Sets up config so that when murmurd starts, it joins an existing network
    /// instead of creating a new one. For creating a new network, just run
    /// murmurd directly — it auto-initializes on first run.
    Join {
        /// The BIP39 mnemonic phrase (quoted).
        mnemonic: String,
        /// Device name.
        #[arg(long, default_value = "murmurd")]
        name: String,
    },
    /// Show daemon status.
    Status,
    /// List approved devices.
    Devices,
    /// List devices pending approval.
    Pending,
    /// Approve a pending device.
    Approve {
        /// Device ID (64-character hex).
        device_id: String,
    },
    /// Revoke an approved device.
    Revoke {
        /// Device ID (64-character hex).
        device_id: String,
    },
    /// Display the network mnemonic.
    Mnemonic,
    /// List synced files.
    Files,
    /// Add a file to the network.
    Add {
        /// Path to the file to add.
        path: String,
    },
    /// Show in-flight blob transfer status.
    Transfers,
    /// Folder management commands.
    #[command(subcommand)]
    Folder(FolderCommand),
    /// Conflict inspection and diffs.
    #[command(subcommand)]
    Conflicts(ConflictsCommand),
    /// Resolve a file conflict.
    Resolve {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// File path within the folder.
        path: String,
        /// Blob hash of the chosen version (64-character hex).
        chosen_hash: String,
    },
    /// Show version history for a file.
    History {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// File path within the folder.
        path: String,
    },
    /// Pairing-invite commands
    #[command(subcommand)]
    Pair(PairCommand),
    /// Mnemonic helpers, including a raw-mnemonic QR fallback
    #[command(subcommand)]
    MnemonicCmd(MnemonicCommand),
}

/// Conflict subcommands (M29).
#[derive(Subcommand)]
enum ConflictsCommand {
    /// List active conflicts.
    List {
        /// Filter by folder ID (optional).
        #[arg(long)]
        folder: Option<String>,
    },
    /// Show a unified diff between the two versions of a conflicted file.
    Diff {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// File path within the folder.
        path: String,
    },
}

/// Pairing-invite subcommands.
#[derive(Subcommand)]
enum PairCommand {
    /// Issue a signed pairing invite, rendered as a `murmur://` URL + QR.
    Invite,
    /// Redeem a `murmur://join?token=…` URL — prints the mnemonic.
    ///
    /// Works offline — the URL is self-contained.
    Redeem {
        /// The `murmur://join?token=…` URL.
        url: String,
    },
}

/// Mnemonic subcommands (subcommands of `mnemonic-cmd` to avoid clashing with
/// the older top-level `mnemonic` command that prints the current network's
/// phrase).
#[derive(Subcommand)]
enum MnemonicCommand {
    /// Render the current mnemonic as a QR code (dangerous — prefer
    /// `pair invite`). Requires an explicit consent flag.
    Qr {
        /// Explicit acknowledgement that the mnemonic is a secret.
        #[arg(long = "i-understand-this-is-secret")]
        acknowledged: bool,
    },
}

/// Folder management subcommands.
#[derive(Subcommand)]
enum FolderCommand {
    /// Create a new shared folder.
    Create {
        /// Folder name.
        name: String,
        /// Optional local directory path to back the folder.
        #[arg(long)]
        local_path: Option<String>,
        /// Optional built-in ignore-file template (requires `--local-path`).
        ///
        /// One of: `rust`, `node`, `python`, `photos`, `documents`, `office`.
        #[arg(long)]
        template: Option<String>,
    },
    /// List all shared folders.
    List,
    /// Subscribe to a folder.
    Subscribe {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// Local directory path for the folder's files.
        local_path: String,
        /// Display name for the folder (defaults to folder's original name).
        #[arg(long)]
        name: Option<String>,
        /// Sync mode: full (default), send-only, or receive-only.
        #[arg(long, default_value = "full")]
        mode: String,
    },
    /// Unsubscribe from a folder.
    Unsubscribe {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// Keep local files after unsubscribing.
        #[arg(long)]
        keep_local: bool,
    },
    /// List files in a folder.
    Files {
        /// Folder ID (64-character hex).
        folder_id: String,
    },
    /// Show folder status.
    Status {
        /// Folder ID (64-character hex).
        folder_id: String,
    },
    /// Remove a shared folder.
    Remove {
        /// Folder ID (64-character hex).
        folder_id: String,
    },
    /// Change sync mode for a folder.
    Mode {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// New mode: full, send-only, or receive-only.
        mode: String,
    },
    /// Rename a folder's display name.
    Rename {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// New display name.
        name: String,
    },
    /// Set the conflict-expiry window. `0` disables (M29).
    SetConflictExpiry {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// Days until unresolved conflicts auto-resolve. `0` disables.
        days: u64,
    },
    /// Set the auto-resolve strategy for conflicts (`none`, `newest`, `mine`).
    SetAutoResolve {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// Strategy: `none`, `newest`, or `mine`.
        strategy: String,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Join { mnemonic, name } => offline::cmd_join(&cli.data_dir, &mnemonic, &name),
        // Pair redeem works fully offline — the URL is self-contained.
        Command::Pair(PairCommand::Redeem { ref url }) => cmd_pair_redeem_offline(url),
        // Pair invite: contact daemon, render URL as QR.
        Command::Pair(PairCommand::Invite) => cmd_pair_invite_online(&cli.data_dir, cli.json),
        // Mnemonic QR fallback: gated by an explicit --i-understand-this-is-secret flag.
        Command::MnemonicCmd(MnemonicCommand::Qr { acknowledged }) => {
            cmd_mnemonic_qr_online(&cli.data_dir, acknowledged, cli.json)
        }
        // Conflicts diff needs custom rendering (unified diff / binary summary).
        Command::Conflicts(ConflictsCommand::Diff {
            ref folder_id,
            ref path,
        }) => cmd_conflicts_diff(&cli.data_dir, folder_id, path, cli.json),
        // All other online commands go through the socket.
        cmd => run_online(&cli.data_dir, cmd, cli.json),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}

/// Offline redeem: parse + verify the URL locally and print the mnemonic.
///
/// This works even when the joiner doesn't have a running daemon, which is
/// the common case — they haven't joined the network yet.
fn cmd_pair_redeem_offline(url: &str) -> Result<()> {
    let token = murmur_seed::PairingToken::from_url(url).context("parse invite URL")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let mnemonic = token.redeem(now).context("redeem invite")?;
    println!("Invite valid — issued by {}.", token.issued_by);
    println!("Mnemonic:");
    println!("{mnemonic}");
    println!();
    println!("To join this network, run: murmur-cli join \"{mnemonic}\" --name <your device name>");
    Ok(())
}

/// Online pair invite: ask daemon to mint an invite, then render the returned
/// URL as an ASCII QR code.
fn cmd_pair_invite_online(base_dir: &std::path::Path, json: bool) -> Result<()> {
    let response = send_single(base_dir, CliRequest::IssuePairingInvite)?;
    if json {
        return print_json(&response);
    }
    match response {
        CliResponse::PairingInvite {
            url,
            expires_at_unix,
        } => {
            println!("Pairing invite (valid for 5 minutes):");
            println!();
            println!("{url}");
            println!();
            println!("{}", render_qr_ascii(&url));
            println!("Expires at UNIX {expires_at_unix}.");
            println!(
                "The joining device can scan this QR or run `murmur-cli pair redeem '<url>'`."
            );
            Ok(())
        }
        CliResponse::Error { message } => {
            eprintln!("error: {message}");
            process::exit(1);
        }
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Online mnemonic-QR fallback. Requires the caller to pass
/// `--i-understand-this-is-secret` to affirm they grasp the risk.
fn cmd_mnemonic_qr_online(
    base_dir: &std::path::Path,
    acknowledged: bool,
    json: bool,
) -> Result<()> {
    if !acknowledged {
        anyhow::bail!(
            "refusing to render a raw mnemonic QR. Re-run with \
             --i-understand-this-is-secret to acknowledge that anyone who sees \
             the QR gains full control of your network."
        );
    }
    let response = send_single(base_dir, CliRequest::ShowMnemonic)?;
    if json {
        return print_json(&response);
    }
    match response {
        CliResponse::Mnemonic { mnemonic } => {
            eprintln!(
                "WARNING: the QR below encodes your raw mnemonic. \
                 Keep the screen hidden and clear it when done."
            );
            eprintln!();
            println!("{}", render_qr_ascii(&mnemonic));
            Ok(())
        }
        CliResponse::Error { message } => {
            eprintln!("error: {message}");
            process::exit(1);
        }
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Render a conflict as a unified diff (text) or size summary (binary).
///
/// The daemon decides `is_text` so all clients agree on rendering. For text,
/// we delegate to `similar::TextDiff` at line granularity. For binary, we
/// print a one-line summary instead of a meaningless hex dump.
fn cmd_conflicts_diff(
    base_dir: &std::path::Path,
    folder_id: &str,
    path: &str,
    json: bool,
) -> Result<()> {
    let response = send_single(
        base_dir,
        CliRequest::ConflictDiff {
            folder_id_hex: folder_id.to_string(),
            path: path.to_string(),
        },
    )?;

    if json {
        return print_json(&response);
    }

    match response {
        CliResponse::ConflictDiff {
            is_text,
            left,
            right,
        } => {
            println!("--- {} ({} bytes)", left.device_name, left.size);
            println!("+++ {} ({} bytes)", right.device_name, right.size);
            if !is_text {
                println!(
                    "binary files differ — {} vs {} bytes",
                    left.size, right.size
                );
                return Ok(());
            }
            let left_text = String::from_utf8_lossy(&left.bytes);
            let right_text = String::from_utf8_lossy(&right.bytes);
            print!("{}", render_unified_diff(&left_text, &right_text));
            Ok(())
        }
        CliResponse::Error { message } => {
            eprintln!("error: {message}");
            process::exit(1);
        }
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Build a unified-diff string using `similar::TextDiff` at line granularity.
///
/// Pure so it can be unit-tested without standing up a daemon.
fn render_unified_diff(left: &str, right: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(left, right);
    let mut out = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Send a single request to the daemon and return its response.
///
/// Kept private because [`run_online`] already handles the common case with
/// automatic rendering — this wrapper is for callers that need to inspect the
/// response structurally.
fn send_single(base_dir: &std::path::Path, request: CliRequest) -> Result<CliResponse> {
    let sock_path = murmur_ipc::socket_path(base_dir);
    let mut stream = UnixStream::connect(&sock_path).with_context(|| {
        format!(
            "murmurd is not running (socket not found at {})",
            sock_path.display()
        )
    })?;
    murmur_ipc::send_message(&mut stream, &request)?;
    let response = murmur_ipc::recv_message(&mut stream)?;
    Ok(response)
}

/// Render arbitrary text as a monochrome ASCII QR code using full/half blocks.
///
/// Uses `qrcodegen` at medium error-correction level — sufficient for invite
/// URLs displayed on screen or printed on paper.
fn render_qr_ascii(text: &str) -> String {
    use qrcodegen::{QrCode, QrCodeEcc};
    let qr = match QrCode::encode_text(text, QrCodeEcc::Medium) {
        Ok(qr) => qr,
        Err(_) => return format!("<failed to render QR for {text}>"),
    };
    let size = qr.size();
    let mut out = String::new();
    // Quiet zone: 2 rows top/bottom, 2 modules left/right, using half-block
    // characters so each terminal row encodes 2 QR rows.
    let border = 2i32;
    let h_border = "  ".repeat((size as usize + border as usize * 2).max(0));
    out.push_str(&h_border);
    out.push('\n');
    let mut y = -border;
    while y < size + border {
        // Left quiet zone.
        for _ in 0..border {
            out.push_str("  ");
        }
        for x in -border..size + border {
            let top = qr.get_module(x, y);
            let bottom = if y + 1 < size + border {
                qr.get_module(x, y + 1)
            } else {
                false
            };
            // Two horizontal spaces per module for aspect ratio.
            let ch = match (top, bottom) {
                (false, false) => "  ",
                (true, false) => "\u{2580}\u{2580}", // upper half
                (false, true) => "\u{2584}\u{2584}", // lower half
                (true, true) => "\u{2588}\u{2588}",  // full block
            };
            out.push_str(ch);
        }
        // Right quiet zone.
        for _ in 0..border {
            out.push_str("  ");
        }
        out.push('\n');
        y += 2;
    }
    out
}

/// Execute an online command by connecting to the daemon socket.
fn run_online(base_dir: &std::path::Path, command: Command, json: bool) -> Result<()> {
    let sock_path = murmur_ipc::socket_path(base_dir);

    let mut stream = UnixStream::connect(&sock_path).with_context(|| {
        format!(
            "murmurd is not running (socket not found at {})",
            sock_path.display()
        )
    })?;

    let request = command_to_request(command);

    murmur_ipc::send_message(&mut stream, &request)?;
    let response: CliResponse = murmur_ipc::recv_message(&mut stream)?;

    if json {
        print_json(&response)?;
    } else {
        print_plain(&response);
    }

    // Exit with non-zero if the response was an error.
    if matches!(response, CliResponse::Error { .. }) {
        process::exit(1);
    }

    Ok(())
}

/// Convert a CLI command to an IPC request.
fn command_to_request(command: Command) -> CliRequest {
    match command {
        Command::Status => CliRequest::Status,
        Command::Devices => CliRequest::ListDevices,
        Command::Pending => CliRequest::ListPending,
        Command::Approve { device_id } => CliRequest::ApproveDevice {
            device_id_hex: device_id,
        },
        Command::Revoke { device_id } => CliRequest::RevokeDevice {
            device_id_hex: device_id,
        },
        Command::Mnemonic => CliRequest::ShowMnemonic,
        Command::Files => CliRequest::ListFiles,
        Command::Add { path } => CliRequest::AddFile { path },
        Command::Transfers => CliRequest::TransferStatus,
        Command::Folder(sub) => match sub {
            FolderCommand::Create {
                name,
                local_path,
                template,
            } => {
                let ignore_patterns = match template.as_deref() {
                    Some(slug) => match offline::template_patterns(slug) {
                        Some(p) => Some(p),
                        None => {
                            eprintln!(
                                "error: unknown template '{slug}'. Available: {}",
                                offline::TEMPLATES.join(", ")
                            );
                            process::exit(1);
                        }
                    },
                    None => None,
                };
                if template.is_some() && local_path.is_none() {
                    eprintln!(
                        "error: --template requires --local-path (the ignore file is \
                         written to the folder's local directory)."
                    );
                    process::exit(1);
                }
                CliRequest::CreateFolder {
                    name,
                    local_path,
                    ignore_patterns,
                }
            }
            FolderCommand::List => CliRequest::ListFolders,
            FolderCommand::Subscribe {
                folder_id,
                local_path,
                name,
                mode,
            } => CliRequest::SubscribeFolder {
                folder_id_hex: folder_id,
                name,
                local_path,
                mode,
            },
            FolderCommand::Unsubscribe {
                folder_id,
                keep_local,
            } => CliRequest::UnsubscribeFolder {
                folder_id_hex: folder_id,
                keep_local,
            },
            FolderCommand::Files { folder_id } => CliRequest::FolderFiles {
                folder_id_hex: folder_id,
            },
            FolderCommand::Status { folder_id } => CliRequest::FolderStatus {
                folder_id_hex: folder_id,
            },
            FolderCommand::Remove { folder_id } => CliRequest::RemoveFolder {
                folder_id_hex: folder_id,
            },
            FolderCommand::Mode { folder_id, mode } => CliRequest::SetFolderMode {
                folder_id_hex: folder_id,
                mode,
            },
            FolderCommand::Rename { folder_id, name } => CliRequest::SetFolderName {
                folder_id_hex: folder_id,
                name,
            },
            FolderCommand::SetConflictExpiry { folder_id, days } => CliRequest::SetConflictExpiry {
                folder_id_hex: folder_id,
                days,
            },
            FolderCommand::SetAutoResolve {
                folder_id,
                strategy,
            } => CliRequest::SetFolderAutoResolve {
                folder_id_hex: folder_id,
                strategy,
            },
        },
        Command::Conflicts(ConflictsCommand::List { folder }) => CliRequest::ListConflicts {
            folder_id_hex: folder,
        },
        // Diff is dispatched in `main()` before this function runs — it
        // needs custom rendering (unified diff / binary summary) rather
        // than the generic response printer.
        Command::Conflicts(ConflictsCommand::Diff { .. }) => unreachable!(),
        Command::Resolve {
            folder_id,
            path,
            chosen_hash,
        } => CliRequest::ResolveConflict {
            folder_id_hex: folder_id,
            path,
            chosen_hash_hex: chosen_hash,
        },
        Command::History { folder_id, path } => CliRequest::FileHistory {
            folder_id_hex: folder_id,
            path,
        },
        Command::Pair(PairCommand::Invite) => CliRequest::IssuePairingInvite,
        // MnemonicCmd::Qr needs the current mnemonic from the daemon.
        Command::MnemonicCmd(MnemonicCommand::Qr { .. }) => CliRequest::ShowMnemonic,
        // Join and Pair Redeem are handled before we get here.
        Command::Join { .. } | Command::Pair(PairCommand::Redeem { .. }) => unreachable!(),
    }
}

/// Print a response as JSON.
fn print_json(response: &CliResponse) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(response).context("serialize response")?
    );
    Ok(())
}

/// Print a response as plain text.
fn print_plain(response: &CliResponse) {
    match response {
        CliResponse::Status {
            device_id,
            device_name,
            network_id,
            peer_count,
            dag_entries,
            uptime_secs,
        } => {
            println!("Device:     {device_name}");
            println!("Device ID:  {device_id}");
            println!("Network ID: {network_id}");
            println!("Peers:      {peer_count}");
            println!("DAG entries: {dag_entries}");
            println!("Uptime:     {}s", uptime_secs);
        }
        CliResponse::Devices { devices } => {
            if devices.is_empty() {
                println!("No approved devices.");
            } else {
                println!("Approved devices ({}):", devices.len());
                for dev in devices {
                    println!("  {} {}", dev.device_id, dev.name);
                }
            }
        }
        CliResponse::Pending { devices } => {
            if devices.is_empty() {
                println!("No pending requests.");
            } else {
                println!("Pending approval ({}):", devices.len());
                for dev in devices {
                    println!("  {} {}", dev.device_id, dev.name);
                }
            }
        }
        CliResponse::Mnemonic { mnemonic } => {
            println!("{mnemonic}");
        }
        CliResponse::Files { files } => {
            if files.is_empty() {
                println!("No synced files.");
            } else {
                println!("Synced files ({}):", files.len());
                for f in files {
                    let mime = f.mime_type.as_deref().unwrap_or("unknown");
                    println!("  {} {} ({} bytes, {mime})", f.blob_hash, f.path, f.size);
                }
            }
        }
        CliResponse::TransferStatus { transfers } => {
            if transfers.is_empty() {
                println!("No active transfers.");
            } else {
                println!("Pending transfers ({}):", transfers.len());
                for t in transfers {
                    let pct = if t.total_bytes > 0 {
                        (t.bytes_transferred as f64 / t.total_bytes as f64) * 100.0
                    } else {
                        0.0
                    };
                    // M31: surface smoothed speed / ETA when the daemon has
                    // seen progress samples for this blob. "--" when unknown.
                    let speed = if t.bytes_per_sec_smoothed == 0 {
                        "-- B/s".to_string()
                    } else {
                        format!("{} B/s", t.bytes_per_sec_smoothed)
                    };
                    let eta = match t.eta_seconds {
                        None => "--".to_string(),
                        Some(s) if s < 60 => format!("{s}s"),
                        Some(s) if s < 3600 => format!("{} min", s / 60),
                        Some(s) => format!("{} h", s / 3600),
                    };
                    println!(
                        "  {} {}/{} bytes ({pct:.0}%) — {speed} — ~{eta} remaining",
                        t.blob_hash, t.bytes_transferred, t.total_bytes
                    );
                }
            }
        }
        CliResponse::Folders { folders } => {
            if folders.is_empty() {
                println!("No shared folders.");
            } else {
                println!("Shared folders ({}):", folders.len());
                for f in folders {
                    let sub = if f.subscribed {
                        format!("subscribed, {}", f.mode.as_deref().unwrap_or("unknown"))
                    } else {
                        "not subscribed".to_string()
                    };
                    let path = f
                        .local_path
                        .as_deref()
                        .map(|p| format!(" -> {p}"))
                        .unwrap_or_default();
                    println!(
                        "  {} {}{} ({} files, {})",
                        f.folder_id, f.name, path, f.file_count, sub
                    );
                }
            }
        }
        CliResponse::FolderStatus {
            folder_id,
            name,
            file_count,
            conflict_count,
            sync_status,
        } => {
            println!("Folder:     {name}");
            println!("Folder ID:  {folder_id}");
            println!("Files:      {file_count}");
            println!("Conflicts:  {conflict_count}");
            println!("Status:     {sync_status}");
        }
        CliResponse::Conflicts { conflicts } => {
            if conflicts.is_empty() {
                println!("No active conflicts.");
            } else {
                println!("Active conflicts ({}):", conflicts.len());
                for c in conflicts {
                    println!(
                        "  {} ({}) — {} versions",
                        c.path,
                        c.folder_name,
                        c.versions.len()
                    );
                    for v in &c.versions {
                        println!(
                            "    {} by {} ({}) at {}",
                            v.blob_hash, v.device_name, v.device_id, v.hlc
                        );
                    }
                }
            }
        }
        CliResponse::FileVersions { versions } => {
            if versions.is_empty() {
                println!("No version history.");
            } else {
                println!("File versions ({}):", versions.len());
                for v in versions {
                    println!(
                        "  {} {} bytes by {} ({}) at {}",
                        v.blob_hash, v.size, v.device_name, v.device_id, v.modified_at
                    );
                }
            }
        }
        CliResponse::BlobData { data } => {
            println!("{} bytes of blob data", data.len());
        }
        CliResponse::Event { event } => {
            println!("[{}] {}", event.event_type, event.data);
        }
        CliResponse::Config {
            device_name,
            network_id,
            folders,
            auto_approve,
            mdns,
            upload_throttle,
            download_throttle,
            sync_paused,
        } => {
            println!("Device:       {device_name}");
            println!("Network:      {network_id}");
            println!("Auto-approve: {auto_approve}");
            println!("mDNS:         {mdns}");
            println!("Sync paused:  {sync_paused}");
            println!("Throttle:     up={upload_throttle} B/s, down={download_throttle} B/s");
            if folders.is_empty() {
                println!("Folders:      (none)");
            } else {
                println!("Folders:");
                for f in folders {
                    let expiry = match f.conflict_expiry_days {
                        Some(d) => format!(" conflict_expiry={d}d"),
                        None => String::new(),
                    };
                    println!(
                        "  {} ({}) -> {} [{}] auto_resolve={}{expiry}",
                        f.name, f.folder_id, f.local_path, f.mode, f.auto_resolve
                    );
                }
            }
        }
        CliResponse::NetworkFolders { folders } => {
            if folders.is_empty() {
                println!("No folders on the network.");
            } else {
                println!("Network folders ({}):", folders.len());
                for f in folders {
                    let sub = if f.subscribed {
                        "subscribed"
                    } else {
                        "available"
                    };
                    println!(
                        "  {} — {} files, {} subs [{}]",
                        f.name, f.file_count, f.subscriber_count, sub
                    );
                }
            }
        }
        CliResponse::FolderSubscriberList { subscribers } => {
            if subscribers.is_empty() {
                println!("No subscribers.");
            } else {
                println!("Subscribers ({}):", subscribers.len());
                for s in subscribers {
                    println!("  {} ({}) [{}]", s.device_name, s.device_id, s.mode);
                }
            }
        }
        CliResponse::DevicePresence { devices } => {
            if devices.is_empty() {
                println!("No devices.");
            } else {
                for d in devices {
                    let status = if d.online { "online" } else { "offline" };
                    println!(
                        "  {} ({}) — {} (last seen: {})",
                        d.device_name, d.device_id, status, d.last_seen_unix
                    );
                }
            }
        }
        // M26a
        CliResponse::IgnorePatterns { patterns } => {
            if patterns.is_empty() {
                println!("(no ignore patterns)");
            } else {
                println!("{patterns}");
            }
        }
        CliResponse::ReclaimedBytes {
            bytes_freed,
            blobs_removed,
        } => {
            println!("Reclaimed {blobs_removed} orphaned blobs, {bytes_freed} bytes freed.");
        }
        // M27a
        CliResponse::Peers { peers } => {
            if peers.is_empty() {
                println!("No peers connected.");
            } else {
                println!("Peers ({}):", peers.len());
                for p in peers {
                    println!(
                        "  {} ({}) [{}] last seen: {}",
                        p.device_name, p.device_id, p.connection_type, p.last_seen_unix
                    );
                }
            }
        }
        CliResponse::StorageStatsResponse {
            folders,
            total_blob_count,
            total_blob_bytes,
            orphaned_blob_count,
            orphaned_blob_bytes,
            dag_entry_count,
        } => {
            println!("Storage Statistics:");
            println!("  DAG entries:     {dag_entry_count}");
            println!("  Total blobs:     {total_blob_count} ({total_blob_bytes} bytes)");
            println!("  Orphaned blobs:  {orphaned_blob_count} ({orphaned_blob_bytes} bytes)");
            for f in folders {
                println!(
                    "  Folder {} ({}): {} files, {} bytes",
                    f.name, f.folder_id, f.file_count, f.total_bytes
                );
            }
        }
        CliResponse::ConnectivityResult {
            relay_reachable,
            latency_ms,
        } => {
            let status = if *relay_reachable {
                "reachable"
            } else {
                "unreachable"
            };
            let latency = latency_ms
                .map(|ms| format!(" ({ms} ms)"))
                .unwrap_or_default();
            println!("Relay: {status}{latency}");
        }
        CliResponse::Ok { message } => {
            println!("{message}");
        }
        CliResponse::Error { message } => {
            eprintln!("error: {message}");
        }
        // handled directly by dedicated commands; included here so the
        // generic dispatch path (JSON / unexpected shape) still formats them
        // without panicking.
        CliResponse::PairingInvite {
            url,
            expires_at_unix,
        } => {
            println!("Pairing invite URL: {url}");
            println!("Expires at UNIX {expires_at_unix}.");
        }
        CliResponse::RedeemedMnemonic {
            mnemonic,
            issued_by,
        } => {
            println!("Issuer: {issued_by}");
            println!("{mnemonic}");
        }
        // Normally rendered by `cmd_conflicts_diff`; this arm keeps the
        // dispatcher total in case the response comes through another path.
        CliResponse::ConflictDiff {
            is_text,
            left,
            right,
        } => {
            println!(
                "ConflictDiff (is_text={is_text}): {} ({} bytes) vs {} ({} bytes)",
                left.device_name, left.size, right.device_name, right.size
            );
        }
        // M31: per-event notification preferences.
        CliResponse::NotificationSettings { settings } => {
            let fmt = |b: bool| if b { "on" } else { "off" };
            println!("Notification settings:");
            println!("  conflict:            {}", fmt(settings.conflict));
            println!(
                "  transfer_completed:  {}",
                fmt(settings.transfer_completed)
            );
            println!("  device_joined:       {}", fmt(settings.device_joined));
            println!("  error:               {}", fmt(settings.error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_online_command_no_daemon_shows_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_online(dir.path(), Command::Status, false);
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("not running"),
            "expected 'not running' in: {err_msg}"
        );
    }

    #[test]
    fn test_render_unified_diff_shows_additions_and_deletions() {
        let left = "alpha\nbeta\ngamma\n";
        let right = "alpha\nBETA\ngamma\n";
        let out = render_unified_diff(left, right);
        assert!(
            out.contains("-beta"),
            "expected removed line marker, got: {out}"
        );
        assert!(
            out.contains("+BETA"),
            "expected added line marker, got: {out}"
        );
        // Unchanged lines are prefixed with a space.
        assert!(
            out.contains(" alpha"),
            "expected context line marker, got: {out}"
        );
    }

    #[test]
    fn test_render_unified_diff_identical_inputs_have_no_changes() {
        let out = render_unified_diff("same\nlines\n", "same\nlines\n");
        assert!(
            !out.contains('+') && !out.contains('-'),
            "identical inputs should produce no insert/delete markers, got: {out:?}"
        );
    }
}
