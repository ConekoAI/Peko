//! Connection Manager — Daemon Discovery and Socket Lifecycle
//!
//! This module handles the real-world concerns of finding and
//! maintaining a connection to the daemon:
//!
//! 1. **Discovery**: Find the daemon via env var, default path, or port
//! 2. **Reconnection**: Handle transient failures
//!
//! `ConnectionManager` is separate from `DaemonClient` per SRP.
//! `DaemonClient` sends/receives packets; `ConnectionManager` handles
//! connection lifecycle.
//!
//! The CLI does NOT auto-start the daemon. Like Docker, the daemon must be
//! started explicitly by the user (`peko daemon start`). This avoids:
//! - Privilege boundary issues (daemon may need elevated permissions)
//! - Ambiguity about where to start the daemon (local vs remote)
//! - Unexpected resource consumption from background processes
//! - System stability issues from implicit service startup
//!
//! Discovery ladder (ADR-021 + ADR-038):
//!   1. `PEKO_DAEMON_SOCK` env var → Unix socket (Unix only)
//!   2. `PEKO_DAEMON_PIPE` env var → Windows named pipe (Windows only)
//!   3. Default Unix socket / default named pipe (per platform)
//!
//! There is no network step: loopback UDP is retired (it trusted any
//! process on the computer, and a CLI with no engine of its own could
//! reach another user's).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
use tokio::net::UnixDatagram;
use tracing::debug;

#[cfg(test)]
use super::default_pid_path;
#[cfg(unix)]
use super::default_socket_path;
use super::DAEMON_SOCK_ENV;
#[cfg(windows)]
use super::{default_pipe_name, DAEMON_PIPE_ENV};

/// Platform-specific socket handle
///
/// On Unix, this wraps a `UnixDatagram` (reliable, file-permission auth).
/// On Windows, this wraps a `NamedPipeClient` (reliable, kernel-enforced
/// DACL auth per ADR-038).
///
/// Unix sockets are wrapped in `Arc` so that cloning the handle
/// shares the same underlying socket — this ensures responses from the
/// daemon reach the receiver task (which uses the cloned handle).
/// Windows named-pipe clients are wrapped in `Arc<Mutex<…>>` because
/// each `try_clone` would otherwise open a new connection (the kernel
/// doesn't support sharing a single pipe handle), but the receiver
/// task needs to read from the same connection the sender wrote to. The
/// mutex is uncontended in practice because the call pattern is
/// sequential send-then-receive.
///
/// Mirrors the server-side `ServerSocket` enum in `server.rs`.
#[derive(Debug, Clone)]
pub enum ConnectionHandle {
    /// Unix domain datagram socket (Unix only)
    #[cfg(unix)]
    Unix {
        socket: Arc<UnixDatagram>,
        path: PathBuf,
        /// The reply socket's file, removed when the last clone drops.
        reply: Option<Arc<ReplySocketFile>>,
    },
    /// Windows named-pipe client (ADR-038), split into halves with a lock
    /// each: `DaemonClient`'s receiver task waits in `recv` while requests
    /// go out through `send`. Under one lock every send waited for the
    /// receiver's idle timeout to release it.
    #[cfg(windows)]
    NamedPipe {
        reader: Arc<tokio::sync::Mutex<tokio::io::ReadHalf<NamedPipeClient>>>,
        writer: Arc<tokio::sync::Mutex<tokio::io::WriteHalf<NamedPipeClient>>>,
        name: String,
    },
}

impl ConnectionHandle {
    /// Send a packet (raw bytes) to the daemon
    ///
    /// # Errors
    /// Returns error if send fails
    pub async fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix { socket, path, .. } => {
                socket.send_to(bytes, path).await?;
            }
            #[cfg(windows)]
            Self::NamedPipe { writer, .. } => {
                use tokio::io::AsyncWriteExt;
                let mut g = writer.lock().await;
                g.write_all(bytes).await?;
            }
        }
        Ok(())
    }

    /// Receive a packet (raw bytes) from the daemon
    ///
    /// # Errors
    /// Returns error if receive fails or times out
    pub async fn recv(&self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let len = match self {
            #[cfg(unix)]
            Self::Unix { socket, .. } => socket.recv(buf).await?,
            #[cfg(windows)]
            Self::NamedPipe { reader, .. } => {
                use tokio::io::AsyncReadExt;
                let mut g = reader.lock().await;
                g.read(buf).await?
            }
        };
        Ok(len)
    }

    /// Receive with timeout
    ///
    /// # Errors
    /// Returns error if receive fails or times out
    pub async fn recv_timeout(&self, buf: &mut [u8], timeout: Duration) -> anyhow::Result<usize> {
        match tokio::time::timeout(timeout, self.recv(buf)).await {
            Ok(Ok(len)) => Ok(len),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("Receive timed out after {:?}", timeout),
        }
    }

    /// Clone the handle (shares the same underlying socket)
    ///
    /// # Errors
    /// Returns error if socket creation fails
    pub async fn try_clone(&self) -> anyhow::Result<Self> {
        match self {
            #[cfg(unix)]
            Self::Unix {
                socket,
                path,
                reply,
            } => {
                // CRITICAL: share the same underlying `Arc<UnixDatagram>`,
                // not a freshly-bound temp socket. The daemon learns the
                // CLI's reply path from the first `recv_from` on the
                // request it receives, and `send_to`s responses back to
                // *that* path. If we bound a new path here, the receiver
                // task spawned by `spawn_receiver` would sit on a socket
                // the daemon has no idea exists, and `peko send` would
                // hang forever waiting for response packets.
                Ok(Self::Unix {
                    socket: Arc::clone(socket),
                    path: path.clone(),
                    reply: reply.clone(),
                })
            }
            #[cfg(windows)]
            Self::NamedPipe {
                reader,
                writer,
                name,
            } => {
                // Share the same underlying `NamedPipeClient` halves via
                // `Arc` clone: a receiver task on one handle reads while
                // the other handle sends.
                Ok(Self::NamedPipe {
                    reader: Arc::clone(reader),
                    writer: Arc::clone(writer),
                    name: name.clone(),
                })
            }
        }
    }
}

/// Manages daemon discovery and connection lifecycle
pub struct ConnectionManager;

impl ConnectionManager {
    /// Connect to the daemon, failing if it's not running.
    ///
    /// The CLI does NOT auto-start the daemon. Start it manually with:
    ///   peko daemon start
    ///
    /// # Errors
    /// Returns error if daemon is not reachable
    pub async fn connect() -> anyhow::Result<ConnectionHandle> {
        Self::try_connect().await
    }

    /// Try to connect to an already-running daemon.
    ///
    /// Uses a 2-second timeout for the ping handshake.
    ///
    /// # Errors
    /// Returns error if daemon is not reachable
    pub async fn try_connect() -> anyhow::Result<ConnectionHandle> {
        Self::try_connect_with_timeout(Duration::from_secs(2)).await
    }

    /// Quick connect check with a short timeout.
    ///
    /// Used by retry loops to avoid accumulating long waits.
    /// Uses a 200ms timeout for the ping handshake.
    ///
    /// # Errors
    /// Returns error if daemon is not reachable within the short timeout
    pub async fn try_connect_quick() -> anyhow::Result<ConnectionHandle> {
        Self::try_connect_with_timeout(Duration::from_millis(200)).await
    }

    async fn try_connect_with_timeout(ping_timeout: Duration) -> anyhow::Result<ConnectionHandle> {
        // 1. Try env var Unix socket
        if let Ok(sock_path) = std::env::var(DAEMON_SOCK_ENV) {
            debug!("Trying Unix socket from env: {}", sock_path);
            #[cfg(unix)]
            if let Ok(handle) = Self::connect_unix_with_timeout(&sock_path, ping_timeout).await {
                return Ok(handle);
            }
            // On non-Unix, fall through (the env var is silently ignored).
        }

        // 2. Try env var Windows named pipe (ADR-038)
        #[cfg(windows)]
        {
            if let Ok(name) = std::env::var(DAEMON_PIPE_ENV) {
                debug!("Trying Windows named pipe from env: {}", name);
                // An explicit pipe is the only daemon this caller means.
                // Falling through would reach whatever answers the default
                // pipe: the user's own daemon from an isolated test.
                return Self::connect_pipe_with_timeout(name.clone(), ping_timeout)
                    .await
                    .map_err(|e| anyhow::anyhow!("No engine on {name}: {e:#}"));
            }
        }

        // 3. Try default Unix socket (Unix) or default pipe (Windows)
        #[cfg(unix)]
        {
            let default_sock = default_socket_path();
            debug!("Trying default Unix socket: {}", default_sock.display());
            if let Ok(handle) =
                Self::connect_unix_with_timeout(&default_sock.to_string_lossy(), ping_timeout).await
            {
                return Ok(handle);
            }
        }
        #[cfg(windows)]
        {
            let default_pipe = default_pipe_name();
            debug!("Trying default Windows named pipe: {}", default_pipe);
            if let Ok(handle) = Self::connect_pipe_with_timeout(default_pipe, ping_timeout).await {
                return Ok(handle);
            }
        }

        anyhow::bail!("No engine found")
    }

    /// Ping a *specific* Unix socket path, rather than whatever
    /// `try_connect*` resolves from the environment.
    ///
    /// `pub(crate)` so `IpcServer::bind_unix_socket` can ask "is anything
    /// actually serving the socket I just failed to bind?" — probing the
    /// ambient default would answer the wrong question whenever the two
    /// paths differ.
    #[cfg(unix)]
    pub(crate) async fn connect_unix_with_timeout(
        path: &str,
        timeout: Duration,
    ) -> anyhow::Result<ConnectionHandle> {
        let path_buf = PathBuf::from(path);
        if !path_buf.exists() {
            anyhow::bail!("Unix socket does not exist: {}", path);
        }

        let (socket, reply) = bind_reply_socket(&path_buf)?;

        // The client also sends full request packets. macOS defaults to a
        // 2048-byte SO_SNDBUF, rejecting even modest assignment prompts.
        if let Err(e) = crate::ipc::server::bump_send_buffer(&socket) {
            tracing::warn!(
                "failed to bump client unix socket SO_SNDBUF to 256KiB: {e}; \
                 large requests may hit EMSGSIZE"
            );
        }

        // Raise the receive buffer so a multi-KiB response (e.g.
        // `principal_log` returning a populated session JSONL) doesn't
        // hit `ENOBUFS` on macOS. The default SO_RCVBUF on macOS is
        // small enough that anything over a few KiB is silently
        // dropped by the receiver's queue; the daemon's server socket
        // already bumps SO_SNDBUF on its side, but that doesn't help
        // if our local kernel queue is the bottleneck.
        //
        // Mirrors the desktop-side fix in
        // `peko-desktop/src-tauri/src/sidecar/mod.rs` for the foreign-
        // daemon adoption probe.
        if let Err(e) = crate::ipc::server::bump_recv_buffer(&socket) {
            tracing::warn!(
                "failed to bump client unix socket SO_RCVBUF to 256KiB: {e}; \
                 large responses may hit ENOBUFS"
            );
        }

        // Test connectivity with a ping
        let ping = super::packet::RequestPacket::Ping { request_id: 0 };
        let ping_bytes = ping.to_bytes()?;
        socket.send_to(&ping_bytes, &path_buf).await?;

        let mut buf = vec![0u8; 65536];
        let len = tokio::time::timeout(timeout, socket.recv(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("Unix socket ping timeout"))?
            .map_err(|e| anyhow::anyhow!("Unix socket recv error: {e}"))?;

        let response = super::packet::ResponsePacket::from_bytes(&buf[..len])?;
        match response {
            super::packet::ResponsePacket::Pong { .. } => {}
            _ => return Err(crate::ipc::unexpected_response(&response)),
        }

        Ok(ConnectionHandle::Unix {
            socket: Arc::new(socket),
            path: path_buf,
            reply: Some(Arc::new(reply)),
        })
    }

    /// Connect to a Windows named pipe (ADR-038) and round-trip a Ping.
    ///
    /// Mirrors `connect_unix_with_timeout`
    /// in shape. Tokio's `ClientOptions::open` is synchronous (it calls
    /// `CreateFileW`), so we wrap the whole open-and-ping in
    /// `spawn_blocking` to keep the async runtime non-blocked, then
    /// apply a timeout.
    #[cfg(windows)]
    async fn connect_pipe_with_timeout(
        name: String,
        timeout: Duration,
    ) -> anyhow::Result<ConnectionHandle> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let name_for_blocking = name.clone();
        let join = tokio::task::spawn_blocking(move || -> std::io::Result<NamedPipeHandle> {
            // Message read mode: each read returns one packet. The default
            // (byte mode) let one read return two packets the daemon wrote
            // back to back, and the stream dropped both ("trailing
            // characters"; `peko send` then failed with "Stream closed
            // unexpectedly" on fast release builds).
            let client = tokio::net::windows::named_pipe::ClientOptions::new()
                .pipe_mode(tokio::net::windows::named_pipe::PipeMode::Message)
                .open(&name_for_blocking)?;
            Ok(NamedPipeHandle {
                client,
                name: name_for_blocking,
            })
        });

        let mut handle = match tokio::time::timeout(timeout, join).await {
            Ok(Ok(Ok(h))) => h,
            Ok(Ok(Err(e))) => {
                anyhow::bail!("Named pipe connect error ({name}): {e}");
            }
            Ok(Err(join_err)) => {
                anyhow::bail!("Named pipe connect task panicked: {join_err}");
            }
            Err(_) => {
                anyhow::bail!("Named pipe connect timeout: {name}");
            }
        };

        // Test connectivity with a Ping. We hold `handle.client` outside
        // the Mutex for the duration of the write+read; the receiver
        // task only starts after this function returns and the handle
        // is wrapped in the Mutex, so there is no contention.
        let ping = super::packet::RequestPacket::Ping { request_id: 0 };
        let ping_bytes = ping.to_bytes()?;
        handle
            .client
            .write_all(&ping_bytes)
            .await
            .map_err(|e| anyhow::anyhow!("Named pipe write error: {e}"))?;

        let mut buf = vec![0u8; 65536];
        let len = tokio::time::timeout(timeout, handle.client.read(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("Named pipe read timeout"))?
            .map_err(|e| anyhow::anyhow!("Named pipe read error: {e}"))?;

        let response = super::packet::ResponsePacket::from_bytes(&buf[..len])?;
        match response {
            super::packet::ResponsePacket::Pong { .. } => {}
            _ => return Err(crate::ipc::unexpected_response(&response)),
        }

        let (reader, writer) = tokio::io::split(handle.client);
        Ok(ConnectionHandle::NamedPipe {
            reader: Arc::new(tokio::sync::Mutex::new(reader)),
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            name: handle.name,
        })
    }
}

/// Local helper struct used by `connect_pipe_with_timeout` to shuttle the
/// freshly-opened `NamedPipeClient` and its name out of the blocking
/// `spawn_blocking` closure. On non-Windows builds the struct is unused
/// and the type alias resolves to a unit stub.
#[cfg(windows)]
struct NamedPipeHandle {
    client: tokio::net::windows::named_pipe::NamedPipeClient,
    name: String,
}

#[cfg(all(not(windows), not(unix)))]
struct NamedPipeHandle {}

/// Stub for non-Unix platforms
#[cfg(not(unix))]
impl ConnectionManager {
    #[allow(dead_code)]
    async fn connect_unix(_path: &str) -> anyhow::Result<ConnectionHandle> {
        anyhow::bail!("Unix sockets not supported on this platform")
    }

    #[allow(dead_code)]
    pub(crate) async fn connect_unix_with_timeout(
        _path: &str,
        _timeout: Duration,
    ) -> anyhow::Result<ConnectionHandle> {
        anyhow::bail!("Unix sockets not supported on this platform")
    }
}

/// The file of a CLI's reply socket; removing it when the connection is
/// gone keeps the run directory from filling with dead sockets.
#[cfg(unix)]
#[derive(Debug)]
pub struct ReplySocketFile(PathBuf);

#[cfg(unix)]
impl Drop for ReplySocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Longest socket path every Unix accepts (`sun_path` is 104 bytes on
/// macOS, 108 on Linux, both counting the terminating NUL).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

/// Remove the reply sockets of commands that are gone.
///
/// A command that exits through `std::process::exit`, or is killed, never
/// removes its socket, and the run directory is on the runtime's volume,
/// so they would pile up. Whether one is still in use cannot be told from
/// its pid (it may be another container's); connecting to it can: the
/// kernel refuses a connection to a socket file nobody holds. Connecting
/// a datagram socket sends nothing.
#[cfg(unix)]
fn remove_dead_reply_sockets(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("cli-") && name.ends_with(".sock")) {
            continue;
        }
        let path = entry.path();
        let Ok(probe) = std::os::unix::net::UnixDatagram::unbound() else {
            return;
        };
        if let Err(e) = probe.connect(&path) {
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Bind the socket the daemon answers on.
///
/// It goes beside the daemon's own socket, so a CLI in another container
/// that shares the runtime's volume still hears back: the daemon replies
/// to the path the request came from, and a path in this process's own
/// `/tmp` does not exist where the daemon runs. The run directory is
/// also private to the runtime's user (0700), which `/tmp` is not. The
/// name is random, not the pid: two containers both run their commands
/// with low pids. When the daemon's directory cannot take it (a long or
/// read-only path), the socket goes in the temporary directory as before.
#[cfg(unix)]
fn bind_reply_socket(
    daemon_socket: &std::path::Path,
) -> anyhow::Result<(UnixDatagram, ReplySocketFile)> {
    let name = format!(
        "cli-{}-{}.sock",
        std::process::id(),
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let beside = daemon_socket
        .parent()
        .map(|dir| dir.join(&name))
        .filter(|p| p.as_os_str().len() <= MAX_SOCKET_PATH);
    if let Some(path) = beside {
        if let Some(dir) = path.parent() {
            remove_dead_reply_sockets(dir);
        }
        if let Ok(socket) = UnixDatagram::bind(&path) {
            return Ok((socket, ReplySocketFile(path)));
        }
    }
    let path = std::env::temp_dir().join(name);
    let socket = UnixDatagram::bind(&path)
        .map_err(|e| anyhow::anyhow!("Failed to bind Unix socket: {e}"))?;
    Ok((socket, ReplySocketFile(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::default_socket_path;

    #[test]
    fn test_default_paths() {
        let sock = default_socket_path();
        assert!(sock.to_string_lossy().contains("daemon.sock"));

        let pid = default_pid_path();
        assert!(pid.to_string_lossy().contains("daemon.pid"));
    }

    /// Exercise the real CLI connection, not just a raw socket helper. macOS
    /// otherwise rejects an ordinary prompt above its 2048-byte send default.
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_client_sends_request_near_packet_limit() {
        use crate::ipc::packet::{RequestPacket, ResponsePacket};
        let dir = tempfile::Builder::new()
            .prefix("ipc-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("server.sock");
        let server = UnixDatagram::bind(&path).unwrap();
        crate::ipc::server::bump_recv_buffer(&server).unwrap();
        let serve = tokio::spawn(async move {
            let mut buf = vec![0; 65536];
            let (len, peer) = server.recv_from(&mut buf).await.unwrap();
            assert!(matches!(
                RequestPacket::from_bytes(&buf[..len]).unwrap(),
                RequestPacket::Ping { .. }
            ));
            let pong = ResponsePacket::Pong {
                request_id: 0,
                uptime_secs: 0,
                version: "test".into(),
            }
            .to_bytes()
            .unwrap();
            server
                .send_to(&pong, peer.as_pathname().unwrap())
                .await
                .unwrap();
            let (len, _) = server.recv_from(&mut buf).await.unwrap();
            RequestPacket::from_bytes(&buf[..len]).unwrap()
        });
        let connection = ConnectionManager::connect_unix_with_timeout(
            path.to_str().unwrap(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let message = "x".repeat(58_000);
        let packet = RequestPacket::PrincipalSendStream {
            request_id: 1,
            name: "test-principal".into(),
            message: message.clone(),
            override_model: None,
            attachments: Vec::new(),
            send_as: None,
            answers: None,
            reply_to: None,
        };
        let bytes = packet.to_bytes().unwrap();
        assert!(bytes.len() > 2048 && bytes.len() <= peko_protocol::ipc::MAX_PACKET_SIZE);
        let send_result = connection.send(&bytes).await;
        if send_result.is_err() {
            serve.abort();
        }
        send_result.unwrap();
        let received = tokio::time::timeout(Duration::from_secs(5), serve)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(received, RequestPacket::PrincipalSendStream { message: actual, .. } if actual == message)
        );
        drop(connection);
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("cli-"))
            .collect();
        assert!(left.is_empty(), "reply socket left behind: {left:?}");
    }

    /// A socket left by a command that is gone is removed when the next
    /// one binds; a socket still held is kept.
    #[cfg(unix)]
    #[test]
    fn dead_reply_sockets_are_removed_live_ones_kept() {
        let dir = tempfile::Builder::new()
            .prefix("ipc-")
            .tempdir_in("/tmp")
            .unwrap();
        let dead = dir.path().join("cli-1-dead.sock");
        drop(std::os::unix::net::UnixDatagram::bind(&dead).unwrap());
        assert!(dead.exists(), "the file outlives its socket");
        let live_path = dir.path().join("cli-2-live.sock");
        let _live = std::os::unix::net::UnixDatagram::bind(&live_path).unwrap();
        let other = dir.path().join("daemon.pid");
        std::fs::write(&other, "1").unwrap();

        remove_dead_reply_sockets(dir.path());

        assert!(!dead.exists());
        assert!(live_path.exists());
        assert!(other.exists());
    }

    /// The daemon answers the path a request came from. In the server
    /// test above that path is checked to be beside the daemon's socket,
    /// where a CLI in another container sharing the volume can be reached.
    #[cfg(unix)]
    #[test]
    fn the_reply_socket_sits_beside_the_daemons() {
        let dir = tempfile::Builder::new()
            .prefix("ipc-")
            .tempdir_in("/tmp")
            .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let (socket, file) = bind_reply_socket(&dir.path().join("daemon.sock")).unwrap();
        let bound = socket.local_addr().unwrap();
        let bound = bound.as_pathname().unwrap();
        assert_eq!(bound.parent().unwrap(), dir.path());
        assert!(bound
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("cli-"));
        drop(socket);
        drop(file);
        assert!(!bound.exists());
    }

    /// A directory whose socket path would be too long falls back to the
    /// temporary directory instead of failing.
    #[cfg(unix)]
    #[test]
    fn a_long_run_directory_falls_back_to_tmp() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let long = std::path::PathBuf::from("/")
            .join("x".repeat(120))
            .join("daemon.sock");
        let (socket, _file) = bind_reply_socket(&long).unwrap();
        let bound = socket.local_addr().unwrap();
        assert!(bound
            .as_pathname()
            .unwrap()
            .starts_with(std::env::temp_dir()));
    }
}

#[cfg(all(test, windows))]
mod pipe_tests {
    use super::ConnectionManager;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

    /// `DaemonClient` keeps a receiver task waiting in `recv` on one handle
    /// while it sends requests on another. A send must not wait for that
    /// read: every command but `daemon status` hung after its ping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sends_while_a_receiver_waits_on_the_same_pipe() {
        let name = format!(r"\\.\pipe\peko-conn-test-{}", uuid::Uuid::new_v4());
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .pipe_mode(PipeMode::Message)
            .create(&name)
            .unwrap();
        let daemon = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut buf = vec![0u8; 65536];
            server.read(&mut buf).await.unwrap(); // the connect ping
            let pong = crate::ipc::packet::ResponsePacket::Pong {
                request_id: 0,
                uptime_secs: 0,
                version: "test".into(),
            };
            server.write_all(&pong.to_bytes().unwrap()).await.unwrap();
            let n = server.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"request");
            server.write_all(b"reply").await.unwrap();
        });

        let conn = ConnectionManager::connect_pipe_with_timeout(name, Duration::from_secs(2))
            .await
            .expect("connect");
        let receiver_conn = conn.try_clone().await.unwrap();
        let receiver = tokio::spawn(async move {
            let mut buf = vec![0u8; 64];
            let n = receiver_conn.recv(&mut buf).await.unwrap();
            buf.truncate(n);
            buf
        });
        tokio::task::yield_now().await;

        tokio::time::timeout(Duration::from_secs(2), conn.send(b"request"))
            .await
            .expect("send must not wait for the pending read")
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), receiver)
            .await
            .expect("reply")
            .unwrap();
        assert_eq!(reply, b"reply");
        daemon.await.unwrap();
    }

    /// A streamed reply is many packets. Each read must return exactly
    /// one, even when the daemon wrote several before the client read.
    /// (Tokio keeps one read pending, which takes the first packet alone;
    /// the read after it found the other two queued and, in byte mode,
    /// returned them as one.)
    #[tokio::test]
    async fn each_read_returns_one_packet() {
        let name = format!(r"\\.\pipe\peko-conn-test-{}", uuid::Uuid::new_v4());
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .pipe_mode(PipeMode::Message)
            .create(&name)
            .unwrap();
        let daemon = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut buf = vec![0u8; 65536];
            server.read(&mut buf).await.unwrap(); // the connect ping
            let pong = crate::ipc::packet::ResponsePacket::Pong {
                request_id: 0,
                uptime_secs: 0,
                version: "test".into(),
            };
            server.write_all(&pong.to_bytes().unwrap()).await.unwrap();
            for packet in [&b"first"[..], b"second", b"third"] {
                server.write_all(packet).await.unwrap();
            }
            server
        });

        let conn = ConnectionManager::connect_pipe_with_timeout(name, Duration::from_secs(2))
            .await
            .expect("connect");
        // All three are in the pipe before the first read.
        let _server = daemon.await.unwrap();
        let mut buf = vec![0u8; 64];
        for expected in [&b"first"[..], b"second", b"third"] {
            let n = conn.recv(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], expected);
        }
    }
}
