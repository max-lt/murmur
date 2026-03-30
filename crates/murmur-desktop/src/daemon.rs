//! Daemon lifecycle management: spawn, monitor, and clean up murmurd.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::ipc;

/// Global handle to the daemon child so we can kill it on exit.
///
/// Used by both the `atexit` handler (covers `std::process::exit()`) and
/// signal handlers (covers SIGINT/SIGTERM).
pub static DAEMON_CHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// Kill the daemon child and exit. Called from atexit and signal handlers.
fn kill_daemon_child() {
    if let Ok(mut guard) = DAEMON_CHILD.lock()
        && let Some(ref mut child) = *guard
    {
        let pid = child.id() as i32;
        unsafe { libc::kill(pid, libc::SIGTERM) };
        for _ in 0..20 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub extern "C" fn on_exit() {
    kill_daemon_child();
}

pub extern "C" fn on_signal(sig: libc::c_int) {
    kill_daemon_child();
    // Re-raise with default handler so the process actually exits.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Check if a process is alive and not a zombie.
fn is_pid_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in status.lines() {
            if let Some(state) = line.strip_prefix("State:") {
                return !state.trim().starts_with('Z');
            }
        }
    }
    true
}

/// Resolve the murmurd binary path.
///
/// Looks for `murmurd` next to the current executable first (same build dir),
/// then falls back to PATH lookup.
fn resolve_murmurd() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("murmurd");
        if sibling.exists() {
            tracing::info!(path = %sibling.display(), "using sibling murmurd binary");
            return sibling;
        }
    }
    PathBuf::from("murmurd")
}

/// Spawn murmurd, monitor the child process, and poll the socket until ready.
///
/// This is the single entry point for all daemon launch paths (auto-launch on
/// restart and first-time Setup). It:
/// 1. Optionally writes a mnemonic to disk (Join flow)
/// 2. Spawns murmurd and stores the `Child` handle
/// 3. Polls the socket for up to 10 seconds
/// 4. On each poll iteration, checks if the child process crashed
///
/// Stale socket cleanup is left to murmurd itself — the desktop app never
/// removes the socket file, avoiding races with a daemon that is still
/// shutting down.
///
/// Returns `Ok(())` when the socket is connectable, or `Err` with a
/// descriptive message if the process died or timed out.
pub async fn launch_and_wait(
    socket_path: PathBuf,
    name: Option<String>,
    mnemonic: Option<String>,
) -> Result<(), String> {
    use std::io::Read as _;
    use std::process::Stdio;

    // Check if a daemon is already running (another process, or previous launch).
    if ipc::daemon_is_running(socket_path.clone()).await {
        tracing::info!("daemon already running — skipping launch");
        return Ok(());
    }

    // Kill any previous child we spawned that might still be shutting down.
    {
        let mut guard = DAEMON_CHILD.lock().unwrap();
        if let Some(ref mut old) = *guard {
            tracing::info!(pid = old.id(), "killing previous daemon before re-launch");
            let _ = old.kill();
            let _ = old.wait();
            *guard = None;
        }
    }

    tokio::task::spawn_blocking(move || {
        let base = murmur_ipc::default_base_dir();
        std::fs::create_dir_all(&base).map_err(|e| format!("create base dir: {e}"))?;

        // Kill any orphan daemon from a previous desktop session that didn't
        // clean up (e.g. the desktop was SIGKILL'd and Drop didn't fire).
        let pid_path = base.join("murmurd.pid");
        if let Ok(contents) = std::fs::read_to_string(&pid_path)
            && let Ok(pid) = contents.trim().parse::<i32>()
            && is_pid_alive(pid)
        {
            tracing::info!(pid, "killing orphan murmurd from previous session");
            unsafe { libc::kill(pid, libc::SIGTERM) };
            // Wait up to 3 seconds for graceful exit.
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
            }
            // Force-kill if still alive.
            if unsafe { libc::kill(pid, 0) } == 0 {
                tracing::warn!(pid, "orphan murmurd did not exit, sending SIGKILL");
                unsafe { libc::kill(pid, libc::SIGKILL) };
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        let _ = std::fs::remove_file(&pid_path);

        // Write mnemonic for Join flow.
        if let Some(ref m) = mnemonic {
            std::fs::write(base.join("mnemonic"), m).map_err(|e| format!("write mnemonic: {e}"))?;
        }

        // Build command.
        let bin = resolve_murmurd();
        tracing::info!(base = %base.display(), bin = %bin.display(), "launching murmurd");
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("--data-dir").arg(&base);
        if let Some(ref n) = name {
            cmd.arg("--name").arg(n);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let child = cmd
            .spawn()
            .map_err(|e| format!("spawn murmurd ({bin:?}): {e}"))?;
        tracing::info!(pid = child.id(), "murmurd process spawned");

        // Store the child handle so atexit can kill it on exit.
        *DAEMON_CHILD.lock().unwrap() = Some(child);

        let sock = murmur_ipc::socket_path(&base);

        // Poll: check socket readiness + process health.
        for i in 0..20u32 {
            std::thread::sleep(std::time::Duration::from_millis(500));

            // Check if the process is still alive.
            let mut guard = DAEMON_CHILD.lock().unwrap();
            if let Some(ref mut c) = *guard {
                match c.try_wait() {
                    Ok(Some(status)) => {
                        // Process exited — read stderr for diagnostics.
                        let stderr = c
                            .stderr
                            .take()
                            .map(|mut s| {
                                let mut buf = String::new();
                                let _ = s.read_to_string(&mut buf);
                                buf
                            })
                            .unwrap_or_default();
                        let msg = if stderr.trim().is_empty() {
                            format!("murmurd exited with {status}")
                        } else {
                            // Show last meaningful line of stderr.
                            let last = stderr
                                .lines()
                                .rev()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or("(no output)");
                            format!("murmurd exited with {status}: {last}")
                        };
                        *guard = None;
                        return Err(msg);
                    }
                    Ok(None) => {} // Still running — good.
                    Err(e) => {
                        return Err(format!("check murmurd process: {e}"));
                    }
                }
            }
            drop(guard);

            // Check socket.
            if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                tracing::info!(attempts = i + 1, "murmurd socket is ready");
                return Ok(());
            }
        }

        Err("murmurd did not become ready in 10 seconds — check logs".to_string())
    })
    .await
    .map_err(|e| format!("launch task panicked: {e}"))?
}
