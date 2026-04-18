//! All messages (events) that drive the desktop app's update loop.

use std::path::PathBuf;

use murmur_ipc::{CliResponse, FolderInfoIpc};

use crate::app::{Screen, SortField};

#[derive(Debug, Clone)]
pub enum Message {
    DaemonCheckResult(bool),
    DaemonConnected,
    /// Launch-and-poll completed: Ok(()) means socket is ready, Err has details.
    DaemonLaunchResult(Result<(), String>),
    RetryDaemonCheck,
    SetupChooseCreate,
    SetupChooseJoin,
    SetupBack,
    DeviceNameChanged(String),
    MnemonicInputChanged(String),
    StartDaemon,
    Navigate(Screen),
    GotStatus(Result<CliResponse, String>),
    GotFolders(Result<CliResponse, String>),
    GotNetworkFolders(Result<CliResponse, String>),
    GotFolderFiles(Result<CliResponse, String>),
    GotFolderSubscribers(Result<CliResponse, String>),
    GotConflicts(Result<CliResponse, String>),
    GotDevices(Result<CliResponse, String>),
    GotPending(Result<CliResponse, String>),
    GotDevicePresence(Result<CliResponse, String>),
    GotFileHistory(Result<CliResponse, String>),
    GotGeneric(Result<CliResponse, String>),
    GotConfig(Result<CliResponse, String>),
    GotIgnorePatterns(Result<CliResponse, String>),
    GotPeers(Result<CliResponse, String>),
    GotStorageStats(Result<CliResponse, String>),
    GotConnectivity(Result<CliResponse, String>),
    GotReclaim(Result<CliResponse, String>),
    CreateFolderFromPicker,
    PickedNewFolder(Option<PathBuf>),
    /// User wants to subscribe — open directory picker first.
    SubscribeFolder(String, String),
    /// Directory picker returned a path for subscribing.
    PickedFolderPath(String, String, Option<PathBuf>),
    UnsubscribeFolder(String),
    RemoveFolder(String),
    SelectFolder(FolderInfoIpc),
    ResolveConflict {
        folder_id: String,
        path: String,
        chosen_hash: String,
    },
    DismissConflict {
        folder_id: String,
        path: String,
    },
    BulkResolve {
        folder_id: String,
        strategy: String,
    },
    ViewFileHistory {
        folder_id: String,
        path: String,
    },
    RestoreVersion {
        folder_id: String,
        path: String,
        blob_hash: String,
    },
    DeleteFile {
        folder_id: String,
        path: String,
    },
    StartRenameFolder(String, String),
    RenameInputChanged(String),
    SubmitRenameFolder,
    CancelRenameFolder,
    SearchQueryChanged(String),
    SortBy(SortField),
    ApproveDevice(String),
    ToggleGlobalSync,
    ToggleFolderSync(String),
    /// Change sync mode for a folder (folder_id, new mode string).
    SetFolderMode(String, String),
    // Settings
    ToggleAutoApprove,
    ToggleMdns,
    SetThrottle(u64, u64),
    ReclaimOrphanedBlobs,
    FolderIgnorePatternsChanged(String),
    SaveIgnorePatterns(String),
    // Mnemonic
    GotMnemonic(Result<CliResponse, String>),
    CopyMnemonic,
    // Leave network
    LeaveNetworkStart,
    LeaveNetworkConfirm,
    LeaveNetworkCancel,
    GotLeaveNetwork(#[allow(dead_code)] Result<CliResponse, String>),
    // Diagnostics
    RunConnectivityCheck,
    ExportDiagnostics,
    // Onboarding
    /// User clicked "Invite device" — daemon should mint a PairingInvite.
    IssuePairingInvite,
    /// Response from `IssuePairingInvite`.
    GotPairingInvite(Result<CliResponse, String>),
    /// User dismissed the pairing invite card.
    ClearPairingInvite,
    /// Copy the current pairing invite URL to the clipboard.
    CopyPairingInviteUrl,
    /// User picked a folder-template slug — fill the ignore-patterns input.
    ApplyFolderTemplate(String),
    // M29: conflict diff + per-folder resolve settings
    /// Toggle (expand/collapse) the inline diff panel for a conflict.
    ToggleConflictDiff {
        folder_id: String,
        path: String,
    },
    /// Response from a `ConflictDiff` fetch. Carries the key so the app can
    /// store the result in the right slot of `conflict_diffs`.
    GotConflictDiff {
        folder_id: String,
        path: String,
        response: Result<CliResponse, String>,
    },
    /// User edited the draft auto-resolve strategy for the selected folder.
    FolderAutoResolveInputChanged(String),
    /// User edited the draft conflict-expiry days for the selected folder.
    FolderConflictExpiryInputChanged(String),
    /// Save the current draft auto-resolve strategy.
    SaveFolderAutoResolve(String),
    /// Save the current draft conflict-expiry days.
    SaveFolderConflictExpiry(String),
    DaemonEvent(CliResponse),
    Tick,
    // M31: Sync Progress & Desktop UX Polish
    /// Response from `GetNotificationSettings`.
    GotNotificationSettings(Result<CliResponse, String>),
    /// Toggle one per-event notification flag and persist.
    ToggleNotification(NotificationKind),
    /// Response from `TransferStatus`.
    GotTransfers(Result<CliResponse, String>),
    /// A single file was dropped onto the window (M31).
    ///
    /// iced emits one event per file even on multi-file drops. The handler
    /// routes the path to the currently-selected folder (no-op otherwise)
    /// and copies the file into that folder's local directory; the folder
    /// watcher then picks it up through forward sync.
    FolderFileDropped(PathBuf),
    /// User typed a hex color in the folder-detail color input.
    FolderColorInputChanged(String),
    /// User typed an icon slug/emoji in the folder-detail icon input.
    FolderIconInputChanged(String),
    /// Persist the current folder's color.
    SaveFolderColor {
        /// Folder ID (hex).
        folder_id: String,
    },
    /// Persist the current folder's icon.
    SaveFolderIcon {
        /// Folder ID (hex).
        folder_id: String,
    },
}

/// Per-event notification flag, used by [`Message::ToggleNotification`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    /// Conflict detected / auto-resolved.
    Conflict,
    /// Transfer completed.
    TransferCompleted,
    /// Device joined or was approved.
    DeviceJoined,
    /// Error events.
    Error,
}
