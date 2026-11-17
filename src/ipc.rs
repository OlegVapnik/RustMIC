//! Local control socket between the daemon, the GUI and the CLI.
//!
//! Linux uses a Unix domain socket in `$XDG_RUNTIME_DIR` (mode 0600, so only the
//! current user can talk to the daemon). Windows uses a named pipe, which is
//! created with the default per-user pipe ACL.
//!
//! Messages are JSON Lines:
//! * daemon → client: a full [`AppState`] snapshot ~10 times per second;
//! * client → daemon: a [`CommandMessage`] envelope with a protocol version.

#[cfg(unix)]
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{ConnectOptions, Listener, ListenerOptions, Stream};
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::{ConnectWaitMode, TryClone};

use crate::config;
use crate::core::{AppState, Command, CommandMessage, PROTOCOL_VERSION};
use crate::logging;

const TICK: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(600);

#[cfg(unix)]
fn create_listener() -> io::Result<Listener> {
    let path = config::socket_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let name = path.as_os_str().to_fs_name::<GenericFilePath>()?;
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    // try_overwrite can recreate the file; tighten permissions either way.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(listener)
}

#[cfg(windows)]
fn create_listener() -> io::Result<Listener> {
    let name = "RustMIC".to_ns_name::<GenericNamespaced>()?;
    ListenerOptions::new().name(name).create_sync()
}

/// Start accepting clients in a background thread.
pub fn start_server(
    state: Arc<RwLock<AppState>>,
    commands: Sender<(u64, Command)>,
) -> io::Result<()> {
    let listener = create_listener()?;
    thread::Builder::new()
        .name("RustMIC-ipc".to_string())
        .spawn(move || {
            for connection in listener.incoming() {
                match connection {
                    Ok(stream) => {
                        // Never serve a client on the accept thread: one long-lived
                        // GUI connection would otherwise block every other client
                        // (CLI, tray, a second window) forever.
                        let state = Arc::clone(&state);
                        let commands = commands.clone();
                        thread::spawn(move || serve_connection(stream, &state, &commands));
                    }
                    Err(error) => {
                        logging::log(&format!("ipc: accept failed: {error}"));
                        thread::sleep(TICK);
                    }
                }
            }
        })?;
    logging::log(&format!(
        "ipc: listening on {} (protocol {})",
        config::socket_path().display(),
        PROTOCOL_VERSION
    ));
    Ok(())
}

fn serve_connection(
    stream: Stream,
    state: &Arc<RwLock<AppState>>,
    commands: &Sender<(u64, Command)>,
) {
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            logging::log(&format!("ipc: clone failed: {error}"));
            return;
        }
    };
    let alive = Arc::new(AtomicBool::new(true));
    let writer_alive = alive.clone();
    let state = state.clone();
    thread::spawn(move || {
        let mut writer = writer;
        while writer_alive.load(Ordering::Relaxed) {
            let snapshot = match state.read() {
                Ok(state) => state.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            let line = match serde_json::to_string(&snapshot) {
                Ok(line) => line,
                Err(error) => {
                    logging::log(&format!("ipc: state encode failed: {error}"));
                    break;
                }
            };
            if writer.write_all(line.as_bytes()).is_err()
                || writer.write_all(b"\n").is_err()
            {
                break;
            }
            thread::sleep(TICK);
        }
    });

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let message: CommandMessage = match serde_json::from_str(line.trim()) {
                    Ok(message) => message,
                    Err(error) => {
                        logging::log(&format!("ipc: bad command: {error}"));
                        continue;
                    }
                };
                if let Err(error) = message.validate() {
                    logging::log(&format!("ipc: {error}"));
                    continue;
                }
                if commands.send((message.id, message.command)).is_err() {
                    break;
                }
            }
            Err(error) => {
                logging::log(&format!("ipc: read failed: {error}"));
                break;
            }
        }
    }
    alive.store(false, Ordering::Relaxed);
}

/// Blocking client used by the CLI and by the small `ctl` helper commands.
pub struct Client {
    reader: BufReader<Stream>,
    next_id: u64,
}

impl Client {
    pub fn connect() -> io::Result<Self> {
        #[cfg(unix)]
        let socket_path = config::socket_path();
        #[cfg(unix)]
        let name = socket_path.as_os_str().to_fs_name::<GenericFilePath>()?;
        #[cfg(windows)]
        let name = "RustMIC".to_ns_name::<GenericNamespaced>()?;

        let stream = ConnectOptions::new()
            .name(name)
            .wait_mode(ConnectWaitMode::Timeout(CONNECT_TIMEOUT))
            .connect_sync()?;
        Ok(Self {
            reader: BufReader::new(stream),
            // IDs must increase across separate CLI processes, otherwise a
            // fresh client's first request can match the daemon's last one.
            next_id: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(1),
        })
    }

    /// Send a command and return its request ID.
    pub fn send(&mut self, command: Command) -> io::Result<u64> {
        self.next_id = self.next_id.wrapping_add(1);
        let message = CommandMessage::with_id(self.next_id, command);
        let mut line = serde_json::to_string(&message)?;
        line.push('\n');
        self.reader.get_mut().write_all(line.as_bytes())?;
        Ok(self.next_id)
    }

    /// Read the next state snapshot. The state protocol version is validated.
    pub fn recv(&mut self) -> io::Result<AppState> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon closed the control socket",
            ));
        }
        let state: AppState = serde_json::from_str(line.trim())?;
        if state.protocol != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon protocol {} does not match client protocol {}",
                    state.protocol, PROTOCOL_VERSION
                ),
            ));
        }
        Ok(state)
    }

    /// Send one command and wait until the daemon reports it as applied.
    pub fn request(&mut self, command: Command) -> io::Result<AppState> {
        let id = self.send(command)?;
        loop {
            let state = self.recv()?;
            if state.last_command_id >= id {
                return Ok(state);
            }
        }
    }
}

/// Remove a stale Unix socket file when the daemon is shutting down.
pub fn cleanup() {
    #[cfg(unix)]
    let _ = fs::remove_file(config::socket_path());
}

/// Path helper used by diagnostics.
pub fn socket_display() -> String {
    config::socket_path().display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn protocol_version_is_on_the_wire() {
        let message = CommandMessage::new(Command::Ping);
        let text = serde_json::to_string(&message).unwrap();
        assert!(text.contains("\"protocol\":1"));
    }

    #[test]
    fn socket_path_is_per_user() {
        let path = config::socket_path();
        if cfg!(unix) {
            assert!(path.is_absolute(), "socket path must be absolute");
            assert_eq!(path.file_name().unwrap(), Path::new("RustMIC.sock").as_os_str());
        }
    }
}
