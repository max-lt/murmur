//! The main update (message handler) loop for the desktop app.

use iced::Task;

use murmur_ipc::{CliRequest, CliResponse};

use crate::app::{App, Screen, SetupStep, StorageStatsCache};
use crate::helpers::{dirs_home, format_size, pick_directory};
use crate::ipc;
use crate::message::Message;

impl App {
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::DaemonCheckResult(running) => {
                tracing::info!(running, screen = ?self.screen, "DaemonCheckResult received");
                self.daemon_running = Some(running);
                if running {
                    tracing::info!("daemon is running — transitioning to connected");
                    return Task::perform(
                        async {
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        },
                        |_| Message::DaemonConnected,
                    );
                }
                // Daemon not running. Auto-launch if a previous network exists,
                // otherwise show Setup.
                if self.daemon_launching {
                    return Task::none();
                }
                let base = murmur_ipc::default_base_dir();
                if base.join("config.toml").exists() {
                    tracing::info!("daemon is NOT running — auto-launching murmurd");
                    self.daemon_error = None;
                    self.daemon_launching = true;
                    return self.do_launch_daemon(None, None);
                }
                tracing::info!("no existing network — showing Setup screen");
                self.screen = Screen::Setup;
            }
            Message::DaemonLaunchResult(Ok(())) => {
                tracing::info!("daemon is ready");
                self.daemon_launching = false;
                self.daemon_running = Some(true);
                self.screen = Screen::Folders;
                return self.fetch_all();
            }
            Message::DaemonLaunchResult(Err(e)) => {
                tracing::warn!(error = %e, "daemon launch failed");
                self.daemon_launching = false;
                self.daemon_running = Some(false);
                self.daemon_error = Some(e);
                self.screen = Screen::DaemonCheck;
            }
            Message::DaemonConnected => {
                tracing::info!("DaemonConnected — navigating to Folders");
                self.screen = Screen::Folders;
                return self.fetch_all();
            }
            Message::RetryDaemonCheck => {
                tracing::info!(socket = %self.socket_path.display(), "RetryDaemonCheck");
                self.daemon_running = None;
                self.daemon_error = None;
                self.daemon_launching = false;
                self.screen = Screen::DaemonCheck;
                let p = self.socket_path.clone();
                return Task::perform(ipc::daemon_is_running(p), Message::DaemonCheckResult);
            }
            Message::SetupChooseCreate => {
                self.join_mode = false;
                self.setup_step = SetupStep::Form;
                self.setup_error = None;
            }
            Message::SetupChooseJoin => {
                self.join_mode = true;
                self.setup_step = SetupStep::Form;
                self.setup_error = None;
            }
            Message::SetupBack => {
                self.setup_step = SetupStep::ChooseMode;
                self.setup_error = None;
            }
            Message::DeviceNameChanged(n) => self.device_name = n,
            Message::MnemonicInputChanged(n) => self.mnemonic_input = n,
            Message::StartDaemon => {
                self.setup_error = None;
                self.daemon_launching = true;
                self.screen = Screen::DaemonCheck;
                let m = if self.join_mode {
                    Some(self.mnemonic_input.clone())
                } else {
                    None
                };
                let n = self.device_name.clone();
                return self.do_launch_daemon(Some(n), m);
            }
            Message::Navigate(screen) => {
                self.screen = screen.clone();
                return match screen {
                    Screen::Folders => {
                        Task::batch([self.fetch_folders(), self.fetch_network_folders()])
                    }
                    Screen::Conflicts => self.fetch_conflicts(),
                    Screen::Devices => Task::batch([self.fetch_devices(), self.fetch_presence()]),
                    Screen::Status => self.fetch_status(),
                    Screen::Settings => {
                        Task::batch([self.fetch_config(), self.fetch_mnemonic()])
                    }
                    Screen::NetworkHealth => {
                        Task::batch([self.fetch_peers(), self.fetch_storage_stats()])
                    }
                    _ => Task::none(),
                };
            }
            // IPC responses
            Message::GotStatus(Ok(CliResponse::Status {
                device_id,
                device_name,
                network_id,
                peer_count,
                dag_entries,
                uptime_secs,
            })) => {
                self.status_device_id = device_id;
                self.status_device_name = device_name;
                self.status_network_id = network_id;
                self.status_peer_count = peer_count;
                self.status_dag_entries = dag_entries;
                self.status_uptime_secs = uptime_secs;
            }
            Message::GotFolders(Ok(CliResponse::Folders { folders })) => self.folders = folders,
            Message::GotNetworkFolders(Ok(CliResponse::NetworkFolders { folders })) => {
                self.network_folders = folders
            }
            Message::GotFolderFiles(Ok(CliResponse::Files { files })) => self.folder_files = files,
            Message::GotFolderSubscribers(Ok(CliResponse::FolderSubscriberList {
                subscribers,
            })) => self.folder_subscribers = subscribers,
            Message::GotConflicts(Ok(CliResponse::Conflicts { conflicts })) => {
                self.conflicts = conflicts
            }
            Message::GotDevices(Ok(CliResponse::Devices { devices })) => self.devices = devices,
            Message::GotPending(Ok(CliResponse::Pending { devices })) => self.pending = devices,
            Message::GotDevicePresence(Ok(CliResponse::DevicePresence { devices })) => {
                self.device_presence = devices
            }
            Message::GotFileHistory(Ok(CliResponse::FileVersions { versions })) => {
                self.history_versions = versions
            }
            Message::GotConfig(Ok(CliResponse::Config {
                auto_approve,
                mdns,
                upload_throttle,
                download_throttle,
                sync_paused,
                device_name,
                ..
            })) => {
                self.cfg_auto_approve = auto_approve;
                self.cfg_mdns = mdns;
                self.cfg_upload_throttle = upload_throttle;
                self.cfg_download_throttle = download_throttle;
                self.sync_paused = sync_paused;
                self.status_device_name = device_name;
            }
            Message::GotMnemonic(Ok(CliResponse::Mnemonic { mnemonic })) => {
                self.cfg_mnemonic = mnemonic;
            }
            Message::CopyMnemonic => {
                if !self.cfg_mnemonic.is_empty() {
                    return iced::clipboard::write(self.cfg_mnemonic.clone());
                }
            }
            Message::GotIgnorePatterns(Ok(CliResponse::IgnorePatterns { patterns })) => {
                self.folder_ignore_patterns = patterns;
            }
            Message::GotPeers(Ok(CliResponse::Peers { peers })) => self.peers = peers,
            Message::GotStorageStats(Ok(CliResponse::StorageStatsResponse {
                total_blob_count,
                total_blob_bytes,
                orphaned_blob_count,
                orphaned_blob_bytes,
                dag_entry_count,
                ..
            })) => {
                self.storage_stats = Some(StorageStatsCache {
                    total_blob_count,
                    total_blob_bytes,
                    orphaned_blob_count,
                    orphaned_blob_bytes,
                    dag_entry_count,
                });
            }
            Message::GotConnectivity(Ok(CliResponse::ConnectivityResult {
                relay_reachable,
                latency_ms,
            })) => {
                self.connectivity_result = Some((relay_reachable, latency_ms));
            }
            Message::GotReclaim(Ok(CliResponse::ReclaimedBytes {
                bytes_freed,
                blobs_removed,
            })) => {
                self.settings_toast = Some(format!(
                    "Reclaimed {blobs_removed} blobs, {} freed",
                    format_size(bytes_freed)
                ));
                return self.fetch_storage_stats();
            }
            Message::GotGeneric(Ok(CliResponse::Ok { ref message })) => {
                tracing::info!(%message, "GotGeneric Ok");
                return Task::batch([self.fetch_folders(), self.fetch_conflicts()]);
            }
            Message::GotGeneric(Ok(ref other)) => {
                tracing::warn!(?other, "GotGeneric received unexpected response variant");
            }
            // Folder actions
            Message::CreateFolderFromPicker => {
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .set_title("Choose a folder to sync")
                            .pick_folder()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::PickedNewFolder,
                );
            }
            Message::PickedNewFolder(Some(path)) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unnamed".to_string());
                let local_path = path.to_string_lossy().to_string();
                let p = self.socket_path.clone();
                tracing::info!(%name, %local_path, "creating folder from picked directory");
                return Task::perform(
                    ipc::send(
                        p,
                        CliRequest::CreateFolder {
                            name,
                            local_path: Some(local_path),
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::PickedNewFolder(None) => {
                tracing::info!("new folder picker cancelled");
            }
            Message::SubscribeFolder(fid, fname) => {
                tracing::info!(folder_id = %fid, folder_name = %fname, "SubscribeFolder: opening directory picker");
                return Task::perform(pick_directory(fid.clone()), move |path| {
                    Message::PickedFolderPath(fid, fname, path)
                });
            }
            Message::PickedFolderPath(fid, fname, Some(path)) => {
                let p = self.socket_path.clone();
                let local_path = path.to_string_lossy().to_string();
                tracing::info!(%fid, %fname, %local_path, "subscribing to folder");
                return Task::perform(
                    ipc::send(
                        p,
                        CliRequest::SubscribeFolder {
                            folder_id_hex: fid,
                            name: Some(fname),
                            local_path,
                            mode: "read-write".to_string(),
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::PickedFolderPath(_fid, _fname, None) => {
                tracing::info!("directory picker cancelled");
            }
            Message::UnsubscribeFolder(fid) => {
                let p = self.socket_path.clone();
                self.selected_folder = None;
                self.screen = Screen::Folders;
                return Task::perform(
                    ipc::send(
                        p,
                        CliRequest::UnsubscribeFolder {
                            folder_id_hex: fid,
                            keep_local: true,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::RemoveFolder(fid) => {
                let p = self.socket_path.clone();
                self.selected_folder = None;
                self.screen = Screen::Folders;
                return Task::perform(
                    ipc::send(
                        p,
                        CliRequest::RemoveFolder {
                            folder_id_hex: fid,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::SelectFolder(folder) => {
                let fid = folder.folder_id.clone();
                self.selected_folder = Some(folder);
                self.screen = Screen::FolderDetail;
                let p = self.socket_path.clone();
                let p2 = self.socket_path.clone();
                let p3 = self.socket_path.clone();
                let fid2 = fid.clone();
                let fid3 = fid.clone();
                return Task::batch([
                    Task::perform(
                        ipc::send(p, CliRequest::FolderFiles { folder_id_hex: fid }),
                        Message::GotFolderFiles,
                    ),
                    Task::perform(
                        ipc::send(
                            p2,
                            CliRequest::FolderSubscribers {
                                folder_id_hex: fid2,
                            },
                        ),
                        Message::GotFolderSubscribers,
                    ),
                    Task::perform(
                        ipc::send(
                            p3,
                            CliRequest::GetIgnorePatterns {
                                folder_id_hex: fid3,
                            },
                        ),
                        Message::GotIgnorePatterns,
                    ),
                ]);
            }
            Message::ResolveConflict {
                folder_id,
                path,
                chosen_hash,
            } => {
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::ResolveConflict {
                            folder_id_hex: folder_id,
                            path,
                            chosen_hash_hex: chosen_hash,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::DismissConflict { folder_id, path } => {
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::DismissConflict {
                            folder_id_hex: folder_id,
                            path,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::BulkResolve {
                folder_id,
                strategy,
            } => {
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::BulkResolveConflicts {
                            folder_id_hex: folder_id,
                            strategy,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::ViewFileHistory { folder_id, path } => {
                self.history_folder_id = folder_id.clone();
                self.history_path = path.clone();
                self.screen = Screen::FileHistory;
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::FileHistory {
                            folder_id_hex: folder_id,
                            path,
                        },
                    ),
                    Message::GotFileHistory,
                );
            }
            Message::RestoreVersion {
                folder_id,
                path,
                blob_hash,
            } => {
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::RestoreFileVersion {
                            folder_id_hex: folder_id,
                            path,
                            blob_hash_hex: blob_hash,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::DeleteFile { folder_id, path } => {
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::DeleteFile {
                            folder_id_hex: folder_id,
                            path,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::StartRenameFolder(fid, current_name) => {
                self.renaming_folder_id = Some(fid);
                self.rename_input = current_name;
            }
            Message::RenameInputChanged(val) => self.rename_input = val,
            Message::CancelRenameFolder => {
                self.renaming_folder_id = None;
                self.rename_input.clear();
            }
            Message::SubmitRenameFolder => {
                if let Some(fid) = self.renaming_folder_id.take() {
                    let name = self.rename_input.clone();
                    self.rename_input.clear();
                    let p = self.socket_path.clone();
                    return Task::perform(
                        ipc::send(
                            p,
                            CliRequest::SetFolderName {
                                folder_id_hex: fid,
                                name,
                            },
                        ),
                        Message::GotGeneric,
                    );
                }
            }
            Message::SearchQueryChanged(q) => self.search_query = q,
            Message::SortBy(f) => {
                if self.sort_field == f {
                    self.sort_ascending = !self.sort_ascending;
                } else {
                    self.sort_field = f;
                    self.sort_ascending = true;
                }
            }
            Message::ApproveDevice(did) => {
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::ApproveDevice {
                            device_id_hex: did,
                            role: "full".to_string(),
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::ToggleGlobalSync => {
                let s = self.socket_path.clone();
                let req = if self.sync_paused {
                    CliRequest::ResumeSync
                } else {
                    CliRequest::PauseSync
                };
                self.sync_paused = !self.sync_paused;
                return Task::perform(ipc::send(s, req), Message::GotGeneric);
            }
            Message::ToggleFolderSync(fid) => {
                let s = self.socket_path.clone();
                let req = if self.folder_paused {
                    CliRequest::ResumeFolderSync { folder_id_hex: fid }
                } else {
                    CliRequest::PauseFolderSync { folder_id_hex: fid }
                };
                self.folder_paused = !self.folder_paused;
                return Task::perform(ipc::send(s, req), Message::GotGeneric);
            }
            // Settings
            Message::ToggleAutoApprove => {
                self.cfg_auto_approve = !self.cfg_auto_approve;
                let s = self.socket_path.clone();
                let enabled = self.cfg_auto_approve;
                return Task::perform(
                    ipc::send(s, CliRequest::SetAutoApprove { enabled }),
                    Message::GotGeneric,
                );
            }
            Message::ToggleMdns => {
                self.cfg_mdns = !self.cfg_mdns;
                let s = self.socket_path.clone();
                let enabled = self.cfg_mdns;
                return Task::perform(
                    ipc::send(s, CliRequest::SetMdns { enabled }),
                    Message::GotGeneric,
                );
            }
            Message::SetThrottle(up, down) => {
                self.cfg_upload_throttle = up;
                self.cfg_download_throttle = down;
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::SetThrottle {
                            upload_bytes_per_sec: up,
                            download_bytes_per_sec: down,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            Message::ReclaimOrphanedBlobs => {
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(s, CliRequest::ReclaimOrphanedBlobs),
                    Message::GotReclaim,
                );
            }
            Message::FolderIgnorePatternsChanged(p) => self.folder_ignore_patterns = p,
            Message::SaveIgnorePatterns(fid) => {
                let s = self.socket_path.clone();
                let patterns = self.folder_ignore_patterns.clone();
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::SetIgnorePatterns {
                            folder_id_hex: fid,
                            patterns,
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            // Leave network
            Message::LeaveNetworkStart => {
                self.leave_network_confirm = true;
            }
            Message::LeaveNetworkCancel => {
                self.leave_network_confirm = false;
            }
            Message::LeaveNetworkConfirm => {
                self.leave_network_confirm = false;
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(s, CliRequest::LeaveNetwork),
                    Message::GotLeaveNetwork,
                );
            }
            Message::GotLeaveNetwork(_) => {
                self.daemon_running = Some(false);
                self.screen = Screen::Setup;
                self.setup_step = SetupStep::ChooseMode;
                self.folders.clear();
                self.devices.clear();
                self.conflicts.clear();
                self.event_log.clear();
                self.daemon_error = None;
                self.setup_error = None;
            }
            // Diagnostics
            Message::RunConnectivityCheck => {
                self.connectivity_result = None;
                let s = self.socket_path.clone();
                return Task::perform(
                    ipc::send(s, CliRequest::RunConnectivityCheck),
                    Message::GotConnectivity,
                );
            }
            Message::ExportDiagnostics => {
                let s = self.socket_path.clone();
                let path = dirs_home().join("murmur-diagnostics").join(format!(
                    "diag-{}.json",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                ));
                return Task::perform(
                    ipc::send(
                        s,
                        CliRequest::ExportDiagnostics {
                            output_path: path.to_string_lossy().to_string(),
                        },
                    ),
                    Message::GotGeneric,
                );
            }
            // Events
            Message::DaemonEvent(CliResponse::Event { event }) => {
                self.push_event(format!("{}: {}", event.event_type, event.data));
                match event.event_type.as_str() {
                    "file_synced" | "dag_synced" => return self.fetch_folders(),
                    "conflict_detected" => return self.fetch_conflicts(),
                    "device_approved" | "device_join_requested"
                        if self.screen == Screen::Devices =>
                    {
                        return self.fetch_devices();
                    }
                    "folder_created" => {
                        return Task::batch([self.fetch_folders(), self.fetch_network_folders()]);
                    }
                    _ => {}
                }
            }
            Message::Tick => return self.fetch_status(),
            // Error handling
            Message::GotStatus(Err(e))
            | Message::GotFolders(Err(e))
            | Message::GotNetworkFolders(Err(e))
            | Message::GotFolderFiles(Err(e))
            | Message::GotFolderSubscribers(Err(e))
            | Message::GotConflicts(Err(e))
            | Message::GotDevices(Err(e))
            | Message::GotPending(Err(e))
            | Message::GotDevicePresence(Err(e))
            | Message::GotFileHistory(Err(e))
            | Message::GotGeneric(Err(e))
            | Message::GotConfig(Err(e))
            | Message::GotMnemonic(Err(e))
            | Message::GotIgnorePatterns(Err(e))
            | Message::GotPeers(Err(e))
            | Message::GotStorageStats(Err(e))
            | Message::GotConnectivity(Err(e))
            | Message::GotReclaim(Err(e)) => {
                tracing::warn!(error = %e, "IPC error response");
                if self.daemon_running == Some(true) {
                    self.daemon_error = Some(e);
                }
            }
            _ => {}
        }
        Task::none()
    }
}
