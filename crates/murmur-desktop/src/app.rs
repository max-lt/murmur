//! Application state, initialization, and IPC fetch helpers.

use std::path::PathBuf;

use iced::{Color, Task, Theme};

use murmur_ipc::{
    CliRequest, ConflictDiffSide, ConflictInfoIpc, DeviceInfoIpc, DevicePresenceIpc, FileInfoIpc,
    FileVersionIpc, FolderInfoIpc, FolderSubscriberIpc, NetworkFolderInfoIpc,
    NotificationSettingsIpc, PeerInfoIpc, TransferInfoIpc,
};

use crate::daemon;
use crate::ipc;
use crate::message::Message;
use crate::style;

pub const MAX_EVENT_LOG: usize = 500;

/// Ring-buffer capacity for the activity feed (M31).
pub const MAX_ACTIVITY_ENTRIES: usize = 200;

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
    Activity,
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
    /// Current pairing invite (URL + expiry) shown in the Devices view.
    pub(crate) pairing_invite: Option<PairingInviteCache>,
    /// Cached conflict diffs keyed by (folder_id, path) (M29).
    pub(crate) conflict_diffs: std::collections::HashMap<(String, String), ConflictDiffCache>,
    /// Which conflict diff panels are currently expanded (M29).
    pub(crate) expanded_conflict_diffs: std::collections::HashSet<(String, String)>,
    /// Draft text for the current folder's auto-resolve setting.
    pub(crate) folder_auto_resolve_input: String,
    /// Draft text for the current folder's conflict-expiry input (days).
    pub(crate) folder_conflict_expiry_input: String,
    /// Per-event notification preferences (M31).
    pub(crate) notification_settings: NotificationSettingsIpc,
    /// Live transfer snapshot from `TransferStatus` (M31).
    pub(crate) transfers: Vec<TransferInfoIpc>,
    /// Ring-buffered activity feed (M31).
    pub(crate) activity: std::collections::VecDeque<ActivityEntry>,
    /// Draft text for the current folder's color input (M31).
    pub(crate) folder_color_input: String,
    /// Draft text for the current folder's icon input (M31).
    pub(crate) folder_icon_input: String,
}

/// One entry in the activity feed (M31).
#[derive(Debug, Clone)]
pub struct ActivityEntry {
    /// Event type string (matches `EngineEventIpc::event_type`).
    pub event_type: String,
    /// Human-readable summary shown in the feed.
    pub summary: String,
    /// Local monotonic timestamp string for display (HH:MM:SS).
    pub when: String,
}

/// Cached conflict diff data (M29).
#[derive(Debug, Clone)]
pub struct ConflictDiffCache {
    pub is_text: bool,
    pub left: ConflictDiffSide,
    pub right: ConflictDiffSide,
}

/// Cached pairing-invite state for display.
#[derive(Debug, Clone)]
pub struct PairingInviteCache {
    pub url: String,
    pub expires_at_unix: u64,
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
            pairing_invite: None,
            conflict_diffs: std::collections::HashMap::new(),
            expanded_conflict_diffs: std::collections::HashSet::new(),
            folder_auto_resolve_input: String::new(),
            folder_conflict_expiry_input: String::new(),
            notification_settings: NotificationSettingsIpc::default(),
            transfers: Vec::new(),
            activity: std::collections::VecDeque::with_capacity(MAX_ACTIVITY_ENTRIES),
            folder_color_input: String::new(),
            folder_icon_input: String::new(),
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

    /// Push a tray-style notification only if the matching per-event toggle
    /// is enabled. Returns `true` if the event was logged (M31).
    ///
    /// The "tray" is implemented as `event_log` until a real platform tray
    /// integration lands — the gating logic is independent of the surface.
    pub fn push_notification(&mut self, event_type: &str, entry: String) -> bool {
        if !self.notification_settings_allow(event_type) {
            return false;
        }
        self.push_event(entry);
        true
    }

    /// Map an engine-event type string to its notification-settings toggle.
    ///
    /// Unknown events default to `true` so new event types surface by default.
    pub fn notification_settings_allow(&self, event_type: &str) -> bool {
        let s = &self.notification_settings;
        match event_type {
            "conflict_detected" | "conflict_auto_resolved" => s.conflict,
            "file_synced" | "blob_received" | "blob_transfer_progress" => s.transfer_completed,
            "device_join_requested" | "device_approved" => s.device_joined,
            "error" => s.error,
            _ => true,
        }
    }

    /// Record an entry in the chronological activity feed (M31).
    ///
    /// Activity feed is always full-fidelity regardless of notification
    /// preferences — those only gate the tray surface.
    pub fn push_activity(&mut self, event_type: String, summary: String) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let h = (secs / 3600) % 24;
        let m = (secs / 60) % 60;
        let s = secs % 60;
        let when = format!("{h:02}:{m:02}:{s:02}");
        if self.activity.len() >= MAX_ACTIVITY_ENTRIES {
            self.activity.pop_back();
        }
        self.activity.push_front(crate::app::ActivityEntry {
            event_type,
            summary,
            when,
        });
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
                // M31: window file-drop events surface as `FolderFilesDropped`
                // when the user is on a folder-detail view.
                iced::event::listen_with(drop_event_filter),
            ])
        } else {
            iced::Subscription::none()
        }
    }
}

/// Translate window file-drop events into `Message::FolderFileDropped`.
///
/// iced emits one `FileDropped` per path when multiple files are dropped at
/// once; the update handler routes each to the currently selected folder.
/// When no folder is selected the event is a no-op.
fn drop_event_filter(
    event: iced::Event,
    _status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    use iced::window::Event as WinEvent;
    match event {
        iced::Event::Window(WinEvent::FileDropped(path)) => Some(Message::FolderFileDropped(path)),
        _ => None,
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
            self.fetch_notification_settings(),
            self.fetch_transfers(),
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

    /// Fetch the current per-event notification preferences (M31).
    pub fn fetch_notification_settings(&self) -> Task<Message> {
        Task::perform(
            ipc::send(
                self.socket_path.clone(),
                CliRequest::GetNotificationSettings,
            ),
            Message::GotNotificationSettings,
        )
    }

    /// Fetch the live transfer snapshot (M31).
    pub fn fetch_transfers(&self) -> Task<Message> {
        Task::perform(
            ipc::send(self.socket_path.clone(), CliRequest::TransferStatus),
            Message::GotTransfers,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new().0
    }

    #[test]
    fn test_push_notification_respects_disabled_flags() {
        // M31: disabled per-event toggles keep the "tray" (event_log) silent
        // but must still leave the activity feed untouched.
        let mut a = app();
        a.notification_settings = NotificationSettingsIpc {
            conflict: false,
            transfer_completed: false,
            device_joined: true,
            error: true,
        };
        assert!(!a.push_notification("conflict_detected", "x".to_string()));
        assert!(!a.push_notification("file_synced", "y".to_string()));
        assert!(a.push_notification("device_approved", "z".to_string()));
        assert_eq!(a.event_log.len(), 1);
        assert_eq!(a.event_log[0], "z");
    }

    #[test]
    fn test_notification_settings_allow_unknown_event_defaults_on() {
        // Fail-safe: new event types surface until explicitly opted out.
        let mut a = app();
        a.notification_settings = NotificationSettingsIpc {
            conflict: false,
            transfer_completed: false,
            device_joined: false,
            error: false,
        };
        assert!(a.notification_settings_allow("future_event_type"));
    }

    #[test]
    fn test_push_activity_ring_buffers_newest_first() {
        // M31: activity feed keeps at most MAX_ACTIVITY_ENTRIES, newest first.
        let mut a = app();
        for i in 0..(MAX_ACTIVITY_ENTRIES + 5) {
            a.push_activity("file_synced".to_string(), format!("e{i}"));
        }
        assert_eq!(a.activity.len(), MAX_ACTIVITY_ENTRIES);
        // Newest pushed last (i = MAX+4) should sit at index 0.
        let expected_newest = format!("e{}", MAX_ACTIVITY_ENTRIES + 4);
        assert_eq!(a.activity.front().unwrap().summary, expected_newest);
    }

    #[test]
    fn test_push_activity_ignores_notification_settings() {
        // The activity feed is full-fidelity — toggles don't affect it.
        let mut a = app();
        a.notification_settings = NotificationSettingsIpc {
            conflict: false,
            transfer_completed: false,
            device_joined: false,
            error: false,
        };
        a.push_activity("conflict_detected".to_string(), "raw".to_string());
        assert_eq!(a.activity.len(), 1);
    }

    #[test]
    fn test_drop_event_filter_translates_file_dropped() {
        use std::path::PathBuf;
        let path = PathBuf::from("/tmp/dropped.bin");
        let ev = iced::Event::Window(iced::window::Event::FileDropped(path.clone()));
        let wid = iced::window::Id::unique();
        let msg = drop_event_filter(ev, iced::event::Status::Ignored, wid);
        match msg {
            Some(Message::FolderFileDropped(p)) => assert_eq!(p, path),
            other => panic!("expected FolderFileDropped, got {other:?}"),
        }
    }

    #[test]
    fn test_drop_event_filter_ignores_unrelated_events() {
        let ev = iced::Event::Window(iced::window::Event::FilesHoveredLeft);
        let wid = iced::window::Id::unique();
        assert!(drop_event_filter(ev, iced::event::Status::Ignored, wid).is_none());
    }
}
