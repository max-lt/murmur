//! Murmur desktop application for Linux, macOS, and Windows.
//!
//! Built with [`iced`](https://iced.rs), a pure-Rust cross-platform UI toolkit.
//! Provides a graphical interface for device management, file syncing,
//! and network status.

mod networking;
mod storage;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use iced::widget::{button, column, container, row, rule, scrollable, text, text_input};
use iced::{Element, Length, Task, Theme};

use murmur_engine::{EngineEvent, MurmurEngine};
use murmur_types::{BlobHash, DeviceInfo, DeviceRole, FileMetadata, NetworkId};

use storage::{DesktopPlatform, Storage};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    iced::application(App::default, App::update, App::view)
        .title("Murmur")
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size(iced::Size::new(960.0, 640.0))
        .run()
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Which screen is currently active.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Screen {
    Setup,
    Devices,
    Files,
    Status,
}

/// Setup wizard step.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SetupStep {
    /// Choose between Create or Join.
    ChooseMode,
    /// Fill in the form (device name, mnemonic).
    Form,
}

/// Application state.
struct App {
    screen: Screen,

    // Setup
    setup_step: SetupStep,
    device_name: String,
    mnemonic_input: String,
    join_mode: bool,
    generated_mnemonic: Option<String>,
    setup_error: Option<String>,

    // Engine (populated after setup) — wrapped in Arc<Mutex<>> for networking.
    engine: Option<Arc<Mutex<MurmurEngine>>>,
    storage: Option<Arc<Storage>>,
    events_queue: Arc<Mutex<Vec<EngineEvent>>>,

    // Network info (populated after setup/load)
    network_id: Option<NetworkId>,
    config_device_name: Option<String>,

    // Networking
    tokio_rt: Option<tokio::runtime::Runtime>,
    net_state: Option<networking::NetworkState>,

    // Derived UI state
    devices: Vec<DeviceInfo>,
    pending: Vec<DeviceInfo>,
    files: Vec<FileMetadata>,
    event_log: Vec<String>,

    // File add
    file_path_input: String,

    // Disconnect confirmation
    confirm_disconnect: bool,

    // Persistent data directory
    data_dir: PathBuf,
}

impl Default for App {
    fn default() -> Self {
        let data_dir = std::env::var("MURMUR_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::data_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("murmur-desktop")
            });

        let mut app = Self {
            screen: Screen::Setup,
            setup_step: SetupStep::ChooseMode,
            device_name: String::new(),
            mnemonic_input: String::new(),
            join_mode: false,
            generated_mnemonic: None,
            setup_error: None,
            engine: None,
            storage: None,
            events_queue: Arc::new(Mutex::new(Vec::new())),
            network_id: None,
            config_device_name: None,
            tokio_rt: None,
            net_state: None,
            devices: Vec::new(),
            pending: Vec::new(),
            files: Vec::new(),
            event_log: Vec::new(),
            file_path_input: String::new(),
            confirm_disconnect: false,
            data_dir,
        };

        // If already initialized, load the engine and skip setup.
        app.try_load_existing();
        app
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// All possible UI messages.
#[derive(Debug, Clone)]
enum Message {
    // Setup
    SetupChooseCreate,
    SetupChooseJoin,
    SetupBack,
    DeviceNameChanged(String),
    MnemonicInputChanged(String),
    GenerateMnemonic,
    CopyMnemonic,
    CreateOrJoinNetwork,

    // Navigation
    Navigate(Screen),

    // Devices
    ApproveDevice(usize),
    RevokeDevice(usize),

    // Files
    FilePathChanged(String),
    AddFile,

    // Network
    DisconnectRequested,
    DisconnectConfirmed,
    DisconnectCancelled,

    // Periodic refresh for networking events
    Tick,
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

impl App {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SetupChooseCreate => {
                self.join_mode = false;
                self.setup_step = SetupStep::Form;
                self.generated_mnemonic = None;
                self.setup_error = None;
            }
            Message::SetupChooseJoin => {
                self.join_mode = true;
                self.setup_step = SetupStep::Form;
                self.generated_mnemonic = None;
                self.setup_error = None;
            }
            Message::SetupBack => {
                self.setup_step = SetupStep::ChooseMode;
                self.setup_error = None;
            }
            Message::DeviceNameChanged(name) => {
                self.device_name = name;
            }
            Message::MnemonicInputChanged(input) => {
                self.mnemonic_input = input;
            }
            Message::GenerateMnemonic => {
                let m = murmur_seed::generate_mnemonic(murmur_seed::WordCount::TwentyFour);
                self.generated_mnemonic = Some(m.to_string());
            }
            Message::CopyMnemonic => {
                if let Some(ref m) = self.generated_mnemonic {
                    return iced::clipboard::write(m.clone());
                }
            }
            Message::CreateOrJoinNetwork => {
                self.setup_error = None;
                match self.init_network() {
                    Ok(()) => {
                        self.screen = Screen::Devices;
                    }
                    Err(e) => {
                        self.setup_error = Some(format!("{e:#}"));
                    }
                }
            }
            Message::Navigate(screen) => {
                self.screen = screen;
                self.refresh_state();
            }
            Message::ApproveDevice(idx) => {
                if let Some(dev) = self.pending.get(idx).cloned() {
                    let entry_bytes = {
                        let engine = self.engine.as_ref().unwrap();
                        let mut eng = engine.lock().unwrap();
                        match eng.approve_device(dev.device_id, DeviceRole::Full) {
                            Ok(entry) => {
                                self.event_log
                                    .push(format!("Approved device: {}", dev.name));
                                Some(entry.to_bytes())
                            }
                            Err(e) => {
                                self.event_log.push(format!("Error approving: {e}"));
                                None
                            }
                        }
                    };
                    if let Some(bytes) = entry_bytes {
                        self.broadcast_entry(bytes);
                    }
                    self.refresh_state();
                }
            }
            Message::RevokeDevice(idx) => {
                if let Some(dev) = self.devices.get(idx).cloned() {
                    let entry_bytes = {
                        let engine = self.engine.as_ref().unwrap();
                        let mut eng = engine.lock().unwrap();
                        match eng.revoke_device(dev.device_id) {
                            Ok(entry) => {
                                self.event_log.push(format!("Revoked device: {}", dev.name));
                                Some(entry.to_bytes())
                            }
                            Err(e) => {
                                self.event_log.push(format!("Error revoking: {e}"));
                                None
                            }
                        }
                    };
                    if let Some(bytes) = entry_bytes {
                        self.broadcast_entry(bytes);
                    }
                    self.refresh_state();
                }
            }
            Message::FilePathChanged(path) => {
                self.file_path_input = path;
            }
            Message::AddFile => {
                match self.add_file_from_path() {
                    Ok(entry_bytes) => {
                        self.broadcast_entry(entry_bytes);
                    }
                    Err(e) => {
                        self.event_log.push(format!("Error adding file: {e:#}"));
                    }
                }
                self.file_path_input.clear();
                self.refresh_state();
            }
            Message::DisconnectRequested => {
                self.confirm_disconnect = true;
            }
            Message::DisconnectCancelled => {
                self.confirm_disconnect = false;
            }
            Message::DisconnectConfirmed => {
                self.confirm_disconnect = false;
                if let Err(e) = self.disconnect_network() {
                    self.event_log.push(format!("Error disconnecting: {e:#}"));
                }
            }
            Message::Tick => {
                self.refresh_state();
            }
        }
        Task::none()
    }
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

impl App {
    /// Periodic tick to refresh UI state from networking events.
    fn subscription(&self) -> iced::Subscription<Message> {
        if self.engine.is_some() {
            iced::time::every(std::time::Duration::from_secs(2)).map(|_| Message::Tick)
        } else {
            iced::Subscription::none()
        }
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

impl App {
    fn view(&self) -> Element<'_, Message> {
        match self.screen {
            Screen::Setup => self.view_setup(),
            Screen::Devices | Screen::Files | Screen::Status => self.view_main(),
        }
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    /// Setup screen: two-step wizard.
    fn view_setup(&self) -> Element<'_, Message> {
        match self.setup_step {
            SetupStep::ChooseMode => self.view_setup_choose(),
            SetupStep::Form => self.view_setup_form(),
        }
    }

    /// Step 1: choose Create or Join.
    fn view_setup_choose(&self) -> Element<'_, Message> {
        let col = column![
            text("Murmur").size(32),
            text("Private Device Sync Network").size(16),
            rule::horizontal(1),
            button(text("Create Network").width(Length::Fill))
                .width(Length::Fill)
                .on_press(Message::SetupChooseCreate),
            button(text("Join Network").width(Length::Fill))
                .width(Length::Fill)
                .on_press(Message::SetupChooseJoin),
        ]
        .spacing(16)
        .padding(30)
        .max_width(400);

        container(col)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    /// Step 2: device name + mnemonic form.
    fn view_setup_form(&self) -> Element<'_, Message> {
        let title = if self.join_mode {
            "Join Network"
        } else {
            "Create Network"
        };

        let name_input = text_input("Device name (e.g., My Desktop)", &self.device_name)
            .on_input(Message::DeviceNameChanged)
            .padding(10);

        let mut col = column![
            button(text("Back")).on_press(Message::SetupBack),
            text(title).size(24),
            rule::horizontal(1),
            name_input,
        ]
        .spacing(12)
        .padding(30)
        .max_width(600);

        if self.join_mode {
            col = col.push(
                text_input("Enter mnemonic phrase…", &self.mnemonic_input)
                    .on_input(Message::MnemonicInputChanged)
                    .padding(10),
            );
        } else {
            col = col.push(button(text("Generate Mnemonic")).on_press(Message::GenerateMnemonic));
            if let Some(ref m) = self.generated_mnemonic {
                col = col.push(text("Write down your recovery phrase:").size(14));
                col = col.push(container(text(m).size(14)).padding(10).width(Length::Fill));
                col = col.push(button(text("Copy to clipboard")).on_press(Message::CopyMnemonic));
            }
        }

        let can_proceed = !self.device_name.is_empty()
            && if self.join_mode {
                !self.mnemonic_input.is_empty()
            } else {
                self.generated_mnemonic.is_some()
            };

        let action_label = if self.join_mode { "Join" } else { "Create" };
        let mut proceed_btn = button(text(action_label));
        if can_proceed {
            proceed_btn = proceed_btn.on_press(Message::CreateOrJoinNetwork);
        }
        col = col.push(proceed_btn);

        if let Some(ref err) = self.setup_error {
            col = col.push(text(format!("Error: {err}")).color([1.0, 0.3, 0.3]));
        }

        container(col)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    /// Main screen with sidebar navigation.
    fn view_main(&self) -> Element<'_, Message> {
        let devices_btn = self.nav_button("Devices", Screen::Devices);
        let files_btn = self.nav_button("Files", Screen::Files);
        let status_btn = self.nav_button("Status", Screen::Status);

        let sidebar = container(
            column![
                text("Murmur").size(20),
                rule::horizontal(1),
                devices_btn,
                files_btn,
                status_btn,
            ]
            .spacing(4)
            .padding(8)
            .width(160),
        );

        let content: Element<Message> = match self.screen {
            Screen::Devices => self.view_devices(),
            Screen::Files => self.view_files(),
            Screen::Status => self.view_status(),
            Screen::Setup => unreachable!(),
        };

        row![sidebar, container(content).width(Length::Fill).padding(16)]
            .height(Length::Fill)
            .into()
    }

    /// Devices tab: this device, approved + pending devices.
    fn view_devices(&self) -> Element<'_, Message> {
        let mut col = column![text("Devices").size(24)].spacing(8);
        let my_id = self.engine.as_ref().map(|e| e.lock().unwrap().device_id());

        if !self.pending.is_empty() {
            col = col.push(text("Pending Approval").size(18));
            for (i, dev) in self.pending.iter().enumerate() {
                let r = row![
                    text(format!("{} ({})", dev.name, dev.device_id)).width(Length::Fill),
                    button(text("Approve")).on_press(Message::ApproveDevice(i)),
                ]
                .spacing(8);
                col = col.push(r);
            }
            col = col.push(rule::horizontal(1));
        }

        // Other approved devices (excluding this device).
        let approved: Vec<_> = self
            .devices
            .iter()
            .enumerate()
            .filter(|(_, d)| d.approved && Some(d.device_id) != my_id)
            .collect();

        col = col.push(text("Other Devices").size(18));
        if approved.is_empty() {
            col = col.push(text("No other devices.").size(14));
        } else {
            for (i, dev) in approved {
                let r = row![
                    text(format!("{} ({:?})", dev.name, dev.role)).width(Length::Fill),
                    text(format!("{}", dev.device_id)).size(11),
                    button(text("Revoke")).on_press(Message::RevokeDevice(i)),
                ]
                .spacing(8);
                col = col.push(r);
            }
        }

        scrollable(col).into()
    }

    /// Files tab: file list + add by path.
    fn view_files(&self) -> Element<'_, Message> {
        let mut col = column![text("Files").size(24)].spacing(8);

        let add_row = row![
            text_input("File path…", &self.file_path_input)
                .on_input(Message::FilePathChanged)
                .padding(8)
                .width(Length::Fill),
            button(text("Add File")).on_press(Message::AddFile),
        ]
        .spacing(8);
        col = col.push(add_row);
        col = col.push(rule::horizontal(1));

        if self.files.is_empty() {
            col = col.push(text("No files synced yet.").size(14));
        } else {
            // Header
            let header = row![
                text("Filename").width(Length::Fill),
                text("Size").width(Length::Fixed(100.0)),
                text("Type").width(Length::Fixed(120.0)),
            ]
            .spacing(8);
            col = col.push(header);

            for file in &self.files {
                let size_str = format_size(file.size);
                let mime = file.mime_type.as_deref().unwrap_or("—");
                let r = row![
                    text(&file.filename).width(Length::Fill),
                    text(size_str).width(Length::Fixed(100.0)),
                    text(mime).width(Length::Fixed(120.0)),
                ]
                .spacing(8);
                col = col.push(r);
            }
        }

        scrollable(col).into()
    }

    /// Status tab: network info, device info, disconnect, and event log.
    fn view_status(&self) -> Element<'_, Message> {
        let mut col = column![text("Status").size(24)].spacing(8);

        // Network info section.
        col = col.push(text("Network").size(18));
        if let Some(nid) = &self.network_id {
            let nid_hex = nid.to_string();
            let short = if nid_hex.len() > 16 {
                format!("{}...", &nid_hex[..16])
            } else {
                nid_hex
            };
            col = col.push(text(format!("Network ID: {short}")));
        }
        if let Some(name) = &self.config_device_name {
            col = col.push(text(format!("Device name: {name}")));
        }

        if let Some(engine) = &self.engine {
            let eng = engine.lock().unwrap();
            col = col.push(text(format!("Device ID: {}", eng.device_id())));
            col = col.push(text(format!("DAG entries: {}", eng.dag().len())));
            col = col.push(text(format!(
                "Files: {}  Devices: {}",
                self.files.len(),
                self.devices.len()
            )));
        }

        // Peer count from networking.
        let peer_count = self
            .net_state
            .as_ref()
            .map(|s| s.connected_peers.load(Ordering::Relaxed))
            .unwrap_or(0);
        col = col.push(text(format!("Connected peers: {peer_count}")));

        col = col.push(text(format!("Data dir: {}", self.data_dir.display())).size(12));

        // Disconnect section.
        col = col.push(rule::horizontal(1));
        if self.confirm_disconnect {
            col = col.push(
                text("This will delete all local network data. Are you sure?")
                    .color([1.0, 0.3, 0.3]),
            );
            col = col.push(
                row![
                    button(text("Yes, disconnect"))
                        .on_press(Message::DisconnectConfirmed)
                        .style(button::danger),
                    button(text("Cancel")).on_press(Message::DisconnectCancelled),
                ]
                .spacing(8),
            );
        } else {
            col = col.push(
                button(text("Disconnect from network"))
                    .on_press(Message::DisconnectRequested)
                    .style(button::danger),
            );
        }

        col = col.push(rule::horizontal(1));
        col = col.push(text("Event Log").size(18));

        if self.event_log.is_empty() {
            col = col.push(text("No events yet.").size(14));
        } else {
            for event in self.event_log.iter().rev().take(50) {
                col = col.push(text(event).size(12));
            }
        }

        scrollable(col).into()
    }
}

// ---------------------------------------------------------------------------
// Engine integration
// ---------------------------------------------------------------------------

impl App {
    /// Try to load an existing network from disk on startup.
    fn try_load_existing(&mut self) {
        let config_path = self.data_dir.join("config.toml");
        if !config_path.exists() {
            return;
        }
        match self.load_engine() {
            Ok(()) => {
                self.screen = Screen::Devices;
                tracing::info!("loaded existing network from {}", self.data_dir.display());
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load existing config, showing setup");
            }
        }
    }

    /// Load engine from persisted config, mnemonic, and DAG entries.
    fn load_engine(&mut self) -> anyhow::Result<()> {
        let config_str = std::fs::read_to_string(self.data_dir.join("config.toml"))?;
        let config: toml::Value = toml::from_str(&config_str)?;

        let storage_config = config
            .get("storage")
            .ok_or_else(|| anyhow::anyhow!("missing [storage] in config"))?;
        let data_dir_str = storage_config
            .get("data_dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing storage.data_dir"))?;
        let blob_dir_str = storage_config
            .get("blob_dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing storage.blob_dir"))?;

        let device_name = config
            .get("device")
            .and_then(|d| d.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let mnemonic_str = std::fs::read_to_string(self.data_dir.join("mnemonic"))?;
        let mnemonic = murmur_seed::parse_mnemonic(mnemonic_str.trim())?;
        let identity = murmur_seed::NetworkIdentity::from_mnemonic(&mnemonic, "");

        self.network_id = Some(identity.network_id());
        self.config_device_name = device_name;

        let device_key_path = self.data_dir.join("device.key");
        let is_creator = !device_key_path.exists();
        let (device_id, signing_key) = if device_key_path.exists() {
            let bytes: [u8; 32] = std::fs::read(&device_key_path)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("device key must be 32 bytes"))?;
            let kp = murmur_seed::DeviceKeyPair::from_bytes(bytes);
            (kp.device_id(), kp.signing_key().clone())
        } else {
            (
                identity.first_device_id(),
                identity.first_device_signing_key().clone(),
            )
        };

        let storage = Arc::new(Storage::open(
            &PathBuf::from(data_dir_str),
            &PathBuf::from(blob_dir_str),
        )?);

        let platform = Arc::new(DesktopPlatform::new(
            storage.clone(),
            self.events_queue.clone(),
        ));

        let mut engine =
            MurmurEngine::from_dag(murmur_dag::Dag::new(device_id, signing_key), platform);

        for entry_bytes in storage.load_all_dag_entries()? {
            engine.load_entry_bytes(&entry_bytes)?;
        }

        let engine = Arc::new(Mutex::new(engine));

        // Start networking.
        let topic = murmur_net::topic_from_network_id(&identity.network_id());
        let creator_iroh_key = identity.creator_iroh_key_bytes();
        self.start_networking(
            engine.clone(),
            storage.clone(),
            device_id,
            creator_iroh_key,
            is_creator,
            topic,
        );

        self.storage = Some(storage);
        self.engine = Some(engine);
        self.refresh_state();

        Ok(())
    }

    /// Initialize a new network or join an existing one.
    fn init_network(&mut self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;

        let mnemonic = if self.join_mode {
            murmur_seed::parse_mnemonic(&self.mnemonic_input)?
        } else {
            let phrase = self
                .generated_mnemonic
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no mnemonic generated"))?;
            murmur_seed::parse_mnemonic(phrase)?
        };

        let identity = murmur_seed::NetworkIdentity::from_mnemonic(&mnemonic, "");

        self.network_id = Some(identity.network_id());
        self.config_device_name = Some(self.device_name.clone());

        let is_creator = !self.join_mode;
        let (device_id, signing_key) = if self.join_mode {
            let kp = murmur_seed::DeviceKeyPair::generate();
            std::fs::write(self.data_dir.join("device.key"), kp.to_bytes())?;
            (kp.device_id(), kp.signing_key().clone())
        } else {
            (
                identity.first_device_id(),
                identity.first_device_signing_key().clone(),
            )
        };

        // Save mnemonic.
        std::fs::write(self.data_dir.join("mnemonic"), mnemonic.to_string())?;

        // Save config.
        let db_dir = self.data_dir.join("db");
        let blob_dir = self.data_dir.join("blobs");
        let config = format!(
            "[device]\nname = \"{}\"\nrole = \"full\"\n\n\
             [storage]\ndata_dir = \"{}\"\nblob_dir = \"{}\"\n\n\
             [network]\nauto_approve = false\n",
            self.device_name,
            db_dir.display(),
            blob_dir.display(),
        );
        std::fs::write(self.data_dir.join("config.toml"), &config)?;

        // Open storage.
        let storage = Arc::new(Storage::open(&db_dir, &blob_dir)?);
        let platform = Arc::new(DesktopPlatform::new(
            storage.clone(),
            self.events_queue.clone(),
        ));

        // Create or join engine.
        let engine = if self.join_mode {
            MurmurEngine::join_network(device_id, signing_key, self.device_name.clone(), platform)
        } else {
            MurmurEngine::create_network(
                device_id,
                signing_key,
                self.device_name.clone(),
                DeviceRole::Full,
                platform,
            )
        };

        storage.flush()?;

        let engine = Arc::new(Mutex::new(engine));

        // Start networking.
        let topic = murmur_net::topic_from_network_id(&identity.network_id());
        let creator_iroh_key = identity.creator_iroh_key_bytes();
        self.start_networking(
            engine.clone(),
            storage.clone(),
            device_id,
            creator_iroh_key,
            is_creator,
            topic,
        );

        self.storage = Some(storage);
        self.engine = Some(engine);
        self.refresh_state();

        Ok(())
    }

    /// Start the networking layer in a dedicated tokio runtime.
    fn start_networking(
        &mut self,
        engine: Arc<Mutex<MurmurEngine>>,
        storage: Arc<Storage>,
        device_id: murmur_types::DeviceId,
        creator_iroh_key_bytes: [u8; 32],
        is_creator: bool,
        topic: iroh_gossip::TopicId,
    ) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("create tokio runtime");

        match rt.block_on(networking::start_networking(
            engine,
            storage,
            device_id,
            creator_iroh_key_bytes,
            is_creator,
            topic,
        )) {
            Ok(state) => {
                self.net_state = Some(state);
                tracing::info!("desktop networking started");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to start networking");
                self.event_log.push(format!("Networking failed: {e:#}"));
            }
        }

        self.tokio_rt = Some(rt);
    }

    /// Broadcast a DAG entry to peers via gossip.
    fn broadcast_entry(&self, entry_bytes: Vec<u8>) {
        if let Some(ref net) = self.net_state
            && let Err(e) = net.broadcast_tx.send(entry_bytes)
        {
            tracing::warn!(error = %e, "failed to send entry to broadcast channel");
        }
    }

    /// Add a file from a local filesystem path. Returns the entry bytes for broadcasting.
    fn add_file_from_path(&mut self) -> anyhow::Result<Vec<u8>> {
        let path = PathBuf::from(&self.file_path_input);
        if !path.exists() {
            anyhow::bail!("file not found: {}", path.display());
        }

        let data = std::fs::read(&path)?;
        let blob_hash = BlobHash::from_data(&data);
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let mime_type = match path.extension().and_then(|e| e.to_str()) {
            Some("jpg" | "jpeg") => Some("image/jpeg".to_string()),
            Some("png") => Some("image/png".to_string()),
            Some("gif") => Some("image/gif".to_string()),
            Some("pdf") => Some("application/pdf".to_string()),
            Some("txt") => Some("text/plain".to_string()),
            Some("mp4") => Some("video/mp4".to_string()),
            _ => None,
        };

        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("engine not initialized"))?;
        let mut eng = engine.lock().unwrap();

        let metadata = FileMetadata {
            blob_hash,
            filename: filename.clone(),
            size: data.len() as u64,
            mime_type,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            device_origin: eng.device_id(),
        };

        let entry = eng.add_file(metadata, data)?;
        self.event_log.push(format!("Added file: {filename}"));

        Ok(entry.to_bytes())
    }

    /// Disconnect from the current network by clearing all local state.
    fn disconnect_network(&mut self) -> anyhow::Result<()> {
        // Stop networking first.
        if let Some(net) = self.net_state.take()
            && let Some(ref rt) = self.tokio_rt
        {
            rt.block_on(net.close());
        }
        if let Some(rt) = self.tokio_rt.take() {
            rt.shutdown_background();
        }

        // Drop engine and storage so file handles are released.
        self.engine = None;
        self.storage = None;

        // Remove all data files.
        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)?;
            tracing::info!(dir = %self.data_dir.display(), "removed network data");
        }

        // Reset UI state.
        self.network_id = None;
        self.config_device_name = None;
        self.devices.clear();
        self.pending.clear();
        self.files.clear();
        self.event_log.clear();
        self.device_name.clear();
        self.mnemonic_input.clear();
        self.generated_mnemonic = None;
        self.setup_error = None;
        self.join_mode = false;
        self.setup_step = SetupStep::ChooseMode;
        self.file_path_input.clear();
        self.screen = Screen::Setup;

        Ok(())
    }

    /// Refresh derived UI state from the engine.
    fn refresh_state(&mut self) {
        if let Some(engine) = &self.engine {
            let eng = engine.lock().unwrap();
            self.devices = eng.list_devices();
            self.pending = eng.pending_requests();
            self.files = eng.state().files.values().cloned().collect();
        }
        // Drain event queue.
        if let Ok(mut events) = self.events_queue.lock() {
            for event in events.drain(..) {
                self.event_log.push(format!("{event:?}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

impl App {
    /// Create a sidebar navigation button, disabled when already on that screen.
    fn nav_button(&self, label: &str, target: Screen) -> iced::widget::Button<'_, Message> {
        let mut btn = button(text(label.to_string()).width(Length::Fill)).width(Length::Fill);
        if self.screen != target {
            btn = btn.on_press(Message::Navigate(target));
        }
        btn
    }
}

/// Format a byte count for display.
fn format_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
