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
        /// Device role: source, backup, or full.
        #[arg(long, default_value = "backup")]
        role: String,
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
        /// Role to assign: source, backup, or full.
        #[arg(long, default_value = "backup")]
        role: String,
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
    /// List active file conflicts.
    Conflicts {
        /// Filter by folder ID (optional).
        #[arg(long)]
        folder: Option<String>,
    },
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
}

/// Folder management subcommands.
#[derive(Subcommand)]
enum FolderCommand {
    /// Create a new shared folder.
    Create {
        /// Folder name.
        name: String,
    },
    /// List all shared folders.
    List,
    /// Subscribe to a folder.
    Subscribe {
        /// Folder ID (64-character hex).
        folder_id: String,
        /// Local directory path for the folder's files.
        local_path: String,
        /// Subscribe as read-only.
        #[arg(long)]
        read_only: bool,
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
        /// New mode: read-write or read-only.
        mode: String,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Join {
            mnemonic,
            name,
            role,
        } => offline::cmd_join(&cli.data_dir, &mnemonic, &name, &role),
        // All online commands go through the socket.
        cmd => run_online(&cli.data_dir, cmd, cli.json),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
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
        Command::Approve { device_id, role } => CliRequest::ApproveDevice {
            device_id_hex: device_id,
            role,
        },
        Command::Revoke { device_id } => CliRequest::RevokeDevice {
            device_id_hex: device_id,
        },
        Command::Mnemonic => CliRequest::ShowMnemonic,
        Command::Files => CliRequest::ListFiles,
        Command::Add { path } => CliRequest::AddFile { path },
        Command::Transfers => CliRequest::TransferStatus,
        Command::Folder(sub) => match sub {
            FolderCommand::Create { name } => CliRequest::CreateFolder { name },
            FolderCommand::List => CliRequest::ListFolders,
            FolderCommand::Subscribe {
                folder_id,
                local_path,
                read_only,
            } => CliRequest::SubscribeFolder {
                folder_id_hex: folder_id,
                local_path,
                mode: if read_only {
                    "read-only".to_string()
                } else {
                    "read-write".to_string()
                },
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
        },
        Command::Conflicts { folder } => CliRequest::ListConflicts {
            folder_id_hex: folder,
        },
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
        // Join is handled before we get here.
        Command::Join { .. } => unreachable!(),
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
                    println!("  {} {} ({})", dev.device_id, dev.name, dev.role);
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
                    println!(
                        "  {} {}/{} bytes ({pct:.0}%)",
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
                    println!(
                        "  {} {} ({} files, {})",
                        f.folder_id, f.name, f.file_count, sub
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
        CliResponse::Event { event } => {
            println!("[{}] {}", event.event_type, event.data);
        }
        CliResponse::Ok { message } => {
            println!("{message}");
        }
        CliResponse::Error { message } => {
            eprintln!("error: {message}");
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
}
