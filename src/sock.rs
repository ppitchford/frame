// Command bus over a Unix socket at `$XDG_RUNTIME_DIR/frame.sock`.
//
// A second `frame` invocation signals the running one rather than starting a
// rival session — which is what lets a single keybinding both start and stop a
// scroll capture. The roadmap specifies this as a small command bus rather than
// a one-off stop flag, so the shape generalises; only `stop` is implemented,
// because only `stop` has a caller.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Stop the running capture.
pub const STOP: &str = "stop";

/// How long a tick will wait for a connected client to send its line. A client
/// writes immediately over a local socket, so this only bounds the damage if one
/// connects and stalls: at ~30 fps it costs about one frame, never the session.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

fn socket_path() -> Result<PathBuf, String> {
    let dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(dir).join("frame.sock"))
}

/// A listening command bus. The socket file is removed when this is dropped, so
/// a clean exit leaves nothing behind for the next run to reason about.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

impl Server {
    pub fn bind() -> Result<Server, String> {
        Server::bind_at(socket_path()?)
    }

    /// Bind at `path`. Split out from `bind` so tests can use a temp path
    /// instead of the real runtime directory.
    pub fn bind_at(path: PathBuf) -> Result<Server, String> {
        // A socket file left behind by a process that died is indistinguishable
        // from a live one by inspection — the only way to tell is to try
        // connecting. If something accepts, a session really is running and
        // clobbering it would strand that capture. If nothing does, the file is
        // stale and removing it is the only way to bind.
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err("another frame session is already listening".into());
            }
            std::fs::remove_file(&path).map_err(|e| format!("removing stale socket: {e}"))?;
        }

        let listener =
            UnixListener::bind(&path).map_err(|e| format!("binding {}: {e}", path.display()))?;
        // Non-blocking, so polling for a command cannot stall a capture frame.
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking: {e}"))?;

        Ok(Server { listener, path })
    }

    /// One pending command, or `None` if nothing arrived this tick. The capture
    /// loop calls this every frame, so it must never block on an idle socket.
    pub fn take_command(&self) -> Option<String> {
        // `Err` here is overwhelmingly `WouldBlock`, meaning simply no client.
        let (stream, _) = self.listener.accept().ok()?;
        // The accepted stream does not reliably inherit the listener's
        // non-blocking flag, so bound the read explicitly rather than trusting
        // it either way.
        stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;

        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).ok()?;
        Some(line.trim().to_string())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best effort. A leftover file is handled by the stale check on the next
        // bind, so failing here is not worth reporting to anyone.
        std::fs::remove_file(&self.path).ok();
    }
}

/// Send `command` to a running session.
///
/// An error means nothing is listening — which is not a failure but the answer
/// to a question: it is how a caller learns there is no session to signal, and
/// therefore that it should start one.
pub fn send(command: &str) -> Result<(), String> {
    send_to(&socket_path()?, command)
}

pub fn send_to(path: &Path, command: &str) -> Result<(), String> {
    let mut stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
    writeln!(stream, "{command}").map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique socket path per test, so a parallel run cannot collide.
    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("frame-test-{}-{tag}.sock", std::process::id()))
    }

    #[test]
    fn a_command_round_trips() {
        let path = temp_path("roundtrip");
        let server = Server::bind_at(path.clone()).expect("bind");
        assert_eq!(server.take_command(), None, "idle socket yields nothing");

        send_to(&path, STOP).expect("send");
        assert_eq!(server.take_command().as_deref(), Some(STOP));
        assert_eq!(server.take_command(), None, "the command is consumed once");
    }

    #[test]
    fn dropping_the_server_removes_the_socket() {
        let path = temp_path("drop");
        {
            let _server = Server::bind_at(path.clone()).expect("bind");
            assert!(path.exists());
        }
        assert!(!path.exists(), "a clean exit leaves no socket behind");
    }

    #[test]
    fn a_stale_socket_file_is_replaced() {
        let path = temp_path("stale");
        // Exactly what a killed process leaves: dropping a `UnixListener` closes
        // its descriptor but does not unlink the file, so the path survives with
        // nothing listening behind it.
        {
            let listener = UnixListener::bind(&path).expect("seed");
            drop(listener);
        }
        assert!(path.exists(), "the socket file outlives its listener");

        // Binding must succeed by clearing it, not fail.
        let server = Server::bind_at(path.clone());
        assert!(
            server.is_ok(),
            "stale socket should be replaced: {:?}",
            server.err()
        );
    }

    #[test]
    fn a_live_server_is_not_clobbered() {
        let path = temp_path("live");
        let _first = Server::bind_at(path.clone()).expect("first bind");
        let second = Server::bind_at(path.clone());
        assert!(second.is_err(), "a running session must not be displaced");
    }
}
