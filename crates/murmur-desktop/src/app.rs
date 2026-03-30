//! Application state, initialization, and IPC fetch helpers.

use std::path::PathBuf;

use iced::{Color, Task, Theme};

use murmur_ipc::{
    CliRequest, ConflictInfoIpc, DeviceInfoIpc, DevicePresenceIpc, FileInfoIpc, FileVersionIpc,
    FolderInfoIpc, FolderSubscriberIpc, NetworkFolderInfoIpc, PeerInfoIpc,
};

use crate::daemon;
use crate::ipc;
use crate::message::Message;
use crate::style;

pub const MAX_EVENT_LOG: usize = 500;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    DaemonCheck,
    Setup,
    Folders,
    FolderDetail,
    Conflicts,
    FileHistory,
    Devices,
    Status,
    RecentFiles,
    Settings,
    NetworkHealth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStep {
    ChooseMode,
    Form,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    Name,
    Size,
    Type,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct App {
    pub(crate) screen: Screen,
    pub(crate) socket_path: PathBuf,
    pub(crate) daemon_running: Option<bool>,
    pub(crate) daemon_error: Option<String>,
    /// True while a launch-and-poll task is in flight; prevents double-spawn.
    pub(crate) daemon_launching: bool,
    pub(crate) setup_step: SetupStep,
    pub(crate) device_name: String,
    pub(crate) mnemonic_input: String,
    pub(crate) join_mode: bool,
    pub(crate) setup_error: Option<String>,
    pub(crate) status_device_id: String,
    pub(crate) status_device_name: String,
    pub(crate) status_network_id: String,
    pub(crate) status_peer_count: u64,
    pub(crate) status_dag_entries: u64,
    pub(crate) status_uptime_secs: u64,
    pub(crate) folders: Vec<FolderInfoIpc>,
    pub(crate) network_folders: Vec<NetworkFolderInfoIpc>,
    pub(crate) selected_folder: Option<FolderInfoIpc>,
    pub(crate) folder_files: Vec<FileInfoIpc>,
    pub(crate) folder_subscribers: Vec<FolderSubscriberIpc>,
    pub(crate) folder_paused: bool,
    pub(crate) conflicts: Vec<ConflictInfoIpc>,
    pub(crate) history_folder_id: String,
    pub(crate) history_path: String,
    pub(crate) history_versions: Vec<FileVersionIpc>,
    pub(crate) devices: Vec<DeviceInfoIpc>,
    pub(crate) pending: Vec<DeviceInfoIpc>,
    pub(crate) device_presence: Vec<DevicePresenceIpc>,
    pub(crate) sync_paused: bool,
    pub(crate) search_query: String,
    pub(crate) sort_field: SortField,
    pub(crate) sort_ascending: bool,
    pub(crate) event_log: Vec<String>,
    // Settings
    pub(crate) leave_network_confirm: bool,
    pub(crate) cfg_auto_approve: bool,
    pub(crate) cfg_mdns: bool,
    pub(crate) cfg_upload_throttle: u64,
    pub(crate) cfg_download_throttle: u64,
    pub(crate) settings_toast: Option<String>,
    pub(crate) cfg_mnemonic: String,
    // Folder settings
    pub(crate) folder_ignore_patterns: String,
    // Folder rename state
    pub(crate) renaming_folder_id: Option<String>,
    pub(crate) rename_input: String,
    // Diagnostics
    pub(crate) peers: Vec<PeerInfoIpc>,
    pub(crate) storage_stats: Option<StorageStatsCache>,
    pub(crate) connectivity_result: Option<(bool, Option<u64>)>,
}

/// Cached storage stats for display.
#[derive(Debug, Clone)]
pub struct StorageStatsCache {
    pub total_blob_count: u64,
    pub total_blob_bytes: u64,
    pub orphaned_blob_count: u64,
    pub orphaned_blob_bytes: u64,
    pub dag_entry_count: u64,
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

impl App {
    pub fn new() -> (Self, Task<Message>) {
        let socket_path = murmur_ipc::default_socket_path();
        let app = Self {
            screen: Screen::DaemonCheck,
            socket_path,
            daemon_running: None,
            daemon_error: None,
            daemon_launching: false,
            setup_step: SetupStep::ChooseMode,
            device_name: String::new(),
            mnemonic_input: String::new(),
            join_mode: false,
            setup_error: None,
            status_device_id: String::new(),
            status_device_name: String::new(),
            status_network_id: String::new(),
            status_peer_count: 0,
            status_dag_entries: 0,
            status_uptime_secs: 0,
            folders: Vec::new(),
            network_folders: Vec::new(),
            selected_folder: None,
            folder_files: Vec::new(),
            folder_subscribers: Vec::new(),
            folder_paused: false,
            conflicts: Vec::new(),
            history_folder_id: String::new(),
            history_path: String::new(),
            history_versions: Vec::new(),
            devices: Vec::new(),
            pending: Vec::new(),
            device_presence: Vec::new(),
            sync_paused: false,
            search_query: String::new(),
            sort_field: SortField::Name,
            sort_ascending: true,
            event_log: Vec::new(),
            leave_network_confirm: false,
            cfg_auto_approve: false,
            cfg_mdns: false,
            cfg_upload_throttle: 0,
            cfg_download_throttle: 0,
            settings_toast: None,
            cfg_mnemonic: String::new(),
            folder_ignore_patterns: String::new(),
            renaming_folder_id: None,
            rename_input: String::new(),
            peers: Vec::new(),
            storage_stats: None,
            connectivity_result: None,
        };
        let path = app.socket_path.clone();
        (
            app,
            Task::perform(ipc::daemon_is_running(path), Message::DaemonCheckResult),
        )
    }
}

// ---------------------------------------------------------------------------
// Small helpers on App
// ---------------------------------------------------------------------------

impl App {
    pub fn push_event(&mut self, entry: String) {
        if self.event_log.len() >= MAX_EVENT_LOG {
            self.event_log.remove(0);
        }
        self.event_log.push(entry);
    }

    pub fn theme(&self) -> Theme {
        Theme::custom(
            "Murmur".to_string(),
            iced::theme::Palette {
                background: style::PANEL_BG,
                text: Color::WHITE,
                primary: style::ACCENT,
                success: style::ACCENT,
                danger: style::ERROR,
                warning: style::WARNING,
            },
        )
    }

    pub fn subscription(&self) -> iced::Subscription<Message> {
        if self.daemon_running == Some(true) {
            iced::Subscription::batch([
                ipc::event_subscription(self.socket_path.clone()).map(Message::DaemonEvent),
                iced::time::every(std::time::Duration::from_secs(5)).map(|_| Message::Tick),
            ])
        } else {
            iced::Subscription::none()
        }
    }
}

// ---------------------------------------------------------------------------
// IPC fetch helpers
// ---------------------------------------------------------------------------

impl App {
    /// Spawn murmurd, monitor the process, and poll the socket.
    ///
    /// If `name` is provided, the daemon is launched with `--name` (Setup flow).
    /// If `mnemonic` is provided, it is written to disk before launch (Join flow).
    /// Returns `DaemonLaunchResult` when the socket is ready or the process dies.
    pub fn do_launch_daemon(
        &self,
        name: Option<String>,
        mnemonic: Option<String>,
    ) -> Task<Message> {
        let socket_path = self.socket_path.clone();
        Task::perform(
            daemon::launch_and_wait(socket_path, name, mnemonic),
            Message::DaemonLaunchResult,
        )
    }

    pub fn fetch_all(&self) -> Task<Message> {
        Task::batch([
            self.fetch_status(),
            self.fetch_folders(),
            self.fetch_network_folders(),
            self.fetch_conflicts(),
            self.fetch_devices(),
            self.fetch_presence(),
        ])
    }

    pub fn fetch_status(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::Status),
            Message::GotStatus,
        )
    }

    pub fn fetch_folders(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::ListFolders),
            Message::GotFolders,
        )
    }

    pub fn fetch_network_folders(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::ListNetworkFolders),
            Message::GotNetworkFolders,
        )
    }

    pub fn fetch_conflicts(&self) -> Task<Message> {
        Task::perform(
            ipc::send(
                self.socket_path.clone(),
                CliRequest::ListConflicts {
                    folder_id_hex: None,
                },
            ),
            Message::GotConflicts,
        )
    }

    pub fn fetch_devices(&self) -> Task<Message> {
        let p = self.socket_path.clone();
        let p2 = self.socket_path.clone();
        Task::batch([
            Task::perform(ipc::send(p, CliRequest::ListDevices), Message::GotDevices),
            Task::perform(ipc::send(p2, CliRequest::ListPending), Message::GotPending),
        ])
    }

    pub fn fetch_presence(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::GetDevicePresence),
            Message::GotDevicePresence,
        )
    }

    pub fn fetch_config(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::GetConfig),
            Message::GotConfig,
        )
    }

    pub fn fetch_mnemonic(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::ShowMnemonic),
            Message::GotMnemonic,
        )
    }

    pub fn fetch_peers(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::ListPeers),
            Message::GotPeers,
        )
    }

    pub fn fetch_storage_stats(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::StorageStats),
            Message::GotStorageStats,
        )
    }
}
