//! Murmur desktop application — thin IPC client to murmurd.
//!
//! Built with [`iced`](https://iced.rs), a pure-Rust cross-platform UI toolkit.
//! All state is fetched from `murmurd` via Unix socket IPC. The desktop app
//! does not embed any engine, storage, or networking.

mod app;
mod daemon;
mod helpers;
mod ipc;
mod message;
mod style;
mod update;
mod views;

use app::App;

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Kill the daemon on any exit path:
    // - atexit: covers std::process::exit() (iced window close)
    // - SIGINT: covers Ctrl+C in terminal
    // - SIGTERM: covers `kill <pid>` or system shutdown
    // - SIGHUP: covers terminal close
    unsafe {
        libc::atexit(daemon::on_exit);
        libc::signal(
            libc::SIGINT,
            daemon::on_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            daemon::on_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGHUP,
            daemon::on_signal as *const () as libc::sighandler_t,
        );
    }

    iced::application(App::new, App::update, App::view)
        .title("Murmur")
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size(iced::Size::new(960.0, 640.0))
        .run()
}
