//! Daemon mode for waytool.
//!
//! Architecture:
//! - `waytool daemon` establishes one RemoteDesktop + ScreenCast
//!   portal session, connects its input through libei (`eis`), then
//!   serves the D-Bus interface in `protocol` peer-to-peer on a unix
//!   socket.
//! - All other subcommands (`key`, `type`, `streams`, ...)
//!   are thin clients that connect to the socket and make one call.
//!
//! Why a daemon: a portal session costs a consent dialog. The portal's
//! restore tokens are meant to skip it, but GNOME (as of 49) doesn't
//! restore screencast streams from one, so a client per command would
//! ask every time. One long-lived session asks once per daemon start.
//!
//! Lifecycle:
//! - The user runs `waytool daemon` in a terminal or as a systemd
//!   user unit. Each start shows the consent dialog; commands reuse the
//!   session until the daemon exits.
//! - Nothing is kept on disk between runs.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, MutexGuard, OwnedSemaphorePermit, Semaphore, watch};

use crate::portal::Portal;
use crate::protocol::{Error, StreamDesc};

/// State shared by every client connection.
pub(crate) struct Daemon {
    /// The portal session.
    portal: Arc<Portal>,
    /// Serializes input requests, so one client's keystrokes never
    /// interleave with another's. Read-only requests don't take it, so
    /// a long `type` doesn't block them.
    input: Mutex<()>,
}

impl Daemon {
    fn portal(&self) -> Arc<Portal> {
        self.portal.clone()
    }

    /// Take the input lock and the portal to send input through.
    async fn input(&self) -> Result<(MutexGuard<'_, ()>, Arc<Portal>)> {
        let guard = self.input.lock().await;
        Ok((guard, self.portal()))
    }
}

/// Most client connections served at once. Each CLI invocation is one
/// short-lived connection, so this only bounds what a misbehaving
/// client can tie up. A connection counts until its last call finishes,
/// which can be after the client has gone (see `Service`).
const MAX_CONNECTIONS: usize = 64;

/// Most calls one connection may have in flight at once.
const MAX_CALLS_PER_CONNECTION: usize = 8;

/// How long a client gets to finish the D-Bus handshake.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn own_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// The daemon's socket file; removed again when this is dropped, however
/// the daemon exits.
struct SocketFile {
    path: PathBuf,
    /// The socket's device and inode, so drop removes only our own
    /// socket and not one a later daemon bound at the same path.
    id: (u64, u64),
}

impl SocketFile {
    /// Bind the daemon socket, refusing if a daemon already answers on it
    /// and unlinking whatever stale socket a previous crash left behind.
    ///
    /// The socket's directory has to be private to us (XDG_RUNTIME_DIR is
    /// 0700): that, not the socket's own mode, is what keeps other users
    /// out, and it closes the gap between bind() and chmod().
    async fn bind(sock: &Path) -> Result<(SocketFile, UnixListener)> {
        let dir = sock.parent().context("socket path has no directory")?;
        let meta = std::fs::metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
        if meta.uid() != own_uid() || meta.mode() & 0o077 != 0 {
            return Err(anyhow!(
                "{} must be owned by you and private (mode 0700) to hold the daemon socket",
                dir.display()
            ));
        }
        if sock.exists() {
            if UnixStream::connect(sock).await.is_ok() {
                return Err(anyhow!(
                    "another waytool daemon is already running at {}",
                    sock.display()
                ));
            }
            let _ = std::fs::remove_file(sock);
        }
        let listener = UnixListener::bind(sock).context("bind unix socket")?;
        let meta = std::fs::symlink_metadata(sock).context("stat socket")?;
        let file = SocketFile {
            path: sock.to_path_buf(),
            id: (meta.dev(), meta.ino()),
        };
        std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))
            .context("chmod socket")?;
        Ok((file, listener))
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let ours =
            std::fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.id);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether a client error is the client having hung up during the
/// handshake: one that connected while the consent dialog was up and
/// timed out before the daemon got to it.
fn gave_up_waiting(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
    })
}

/// Serve one accepted connection, if it comes from our own user. The
/// socket's directory already keeps other users out; this makes that
/// explicit rather than a property of wherever the socket happens to
/// live.
async fn accept_client(
    stream: UnixStream,
    daemon: Arc<Daemon>,
    connection: OwnedSemaphorePermit,
) -> Result<()> {
    let uid = stream.peer_cred().context("peer credentials")?.uid();
    if uid != own_uid() {
        return Err(anyhow!("refused a connection from uid {uid}"));
    }
    serve_client(stream, |hangup| Service {
        daemon,
        hangup,
        _connection: connection,
        calls: Arc::new(Semaphore::new(MAX_CALLS_PER_CONNECTION)),
    })
    .await
}

pub async fn run_daemon() -> Result<()> {
    // Bind before asking for consent, so a second daemon fails here
    // instead of after the user has already clicked through a dialog.
    // Clients that connect meanwhile queue until the session is up, or
    // give up after their handshake timeout (see protocol::connect).
    let sock = crate::protocol::socket_path()?;
    let (_socket_file, listener) = SocketFile::bind(&sock).await?;

    eprintln!("waytool: establishing portal session (consent prompt may appear)...");
    let portal = Arc::new(Portal::establish().await?);
    eprintln!(
        "waytool: session ready, {} stream(s)",
        portal.streams().len()
    );

    let daemon = Arc::new(Daemon {
        portal: portal.clone(),
        input: Mutex::new(()),
    });

    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    eprintln!("waytool: listening on {}", sock.display());

    loop {
        let stream = match listener.accept().await {
            Ok((stream, _addr)) => stream,
            // Out of fds, or a connection aborted before it was
            // accepted: nothing to do with the session, so carry on
            // after a pause rather than spin.
            Err(e) => {
                eprintln!("waytool: accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            eprintln!("waytool: refusing a connection: {MAX_CONNECTIONS} already open");
            continue;
        };
        let daemon = daemon.clone();
        tokio::task::spawn(async move {
            if let Err(e) = accept_client(stream, daemon, permit).await {
                if gave_up_waiting(&e) {
                    eprintln!("waytool: a client gave up before the daemon was ready");
                } else {
                    eprintln!("waytool: client error: {e:#}");
                }
            }
        });
    }
}

fn keysym(name: &str) -> Result<u32> {
    crate::keys::keysym_from_name(name)
        .ok_or_else(|| anyhow!("unknown key name {name:?} (see `waytool key --help`)"))
}

/// Longest `delay`/`hold` a type request may ask for, in milliseconds.
const MAX_PACING_MS: u32 = 10_000;

/// Fires when the client that made a request hangs up — Ctrl-C on the
/// CLI, say — so long-running requests can stop instead of carrying on
/// for nobody.
#[derive(Clone)]
pub(crate) struct Hangup(watch::Receiver<bool>);

impl Hangup {
    fn happened(&self) -> bool {
        *self.0.borrow()
    }

    /// Sleep for `ms` milliseconds, or until the client hangs up.
    async fn sleep(&mut self, ms: u32) {
        if ms > 0 {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(ms.into())) => {}
                _ = self.0.wait_for(|gone| *gone) => {}
            }
        }
    }
}

/// Type `text` into whatever currently holds keyboard focus, one
/// press+release per character. `delay_ms` is how long to wait after
/// each character — pacing for slow consumers (e.g. DOSBox's emulated
/// keyboard, which drops characters typed faster than it can drain).
/// `hold_ms` is how long each key stays down, for emulators that scan a
/// keyboard matrix (VICE) and miss a key pressed and released between
/// two scans. Stops between characters if the client hangs up; a key
/// that's down is always released first.
///
/// A character that isn't on the keyboard layout can't be sent as a key
/// press at all, so it's refused. Everything is checked before the
/// first keystroke, so a string that can't be typed types nothing.
async fn type_text(
    portal: &Portal,
    text: &str,
    delay_ms: u32,
    hold_ms: u32,
    mut hangup: Hangup,
) -> Result<()> {
    for (name, ms) in [("delay", delay_ms), ("hold", hold_ms)] {
        if ms > MAX_PACING_MS {
            return Err(anyhow!("--{name} is at most {MAX_PACING_MS} ms, got {ms}"));
        }
    }
    let mut syms = Vec::new();
    for ch in text.chars() {
        let sym = crate::keys::keysym_for_char(ch)
            .ok_or_else(|| anyhow!("can't type {ch:?}: it has no keysym (nothing was typed)"))?;
        if !portal.has_keysym(sym)? {
            return Err(anyhow!(
                "{ch:?} isn't on the keyboard layout, so it can't be typed (nothing was typed)"
            ));
        }
        syms.push(sym);
    }
    for sym in syms {
        if hangup.happened() {
            return Err(anyhow!("client hung up; stopped typing"));
        }
        portal.keysym(sym, true)?;
        hangup.sleep(hold_ms).await;
        portal.keysym(sym, false)?;
        hangup.sleep(delay_ms).await;
    }
    Ok(())
}

/// Press `syms` in order, then release them in reverse so the first
/// (the modifier) is the last to come up — the held-modifier discipline
/// emulators expect. If a press fails, the keys already down are still
/// released, so an error never leaves a modifier stuck.
async fn key_chord(portal: &Portal, syms: &[u32]) -> Result<()> {
    let mut pressed = 0;
    let mut result = Ok(());
    for sym in syms {
        if let Err(e) = portal.keysym(*sym, true) {
            result = Err(e);
            break;
        }
        pressed += 1;
    }
    for sym in syms[..pressed].iter().rev() {
        let released = portal.keysym(*sym, false);
        if result.is_ok() {
            result = released;
        }
    }
    result
}

/// Reject NaN and infinite coordinates up front: every bounds check
/// compares false against NaN, so it would otherwise reach the portal.
fn finite(coords: &[f64]) -> Result<()> {
    if coords.iter().all(|c| c.is_finite()) {
        Ok(())
    } else {
        Err(anyhow!(
            "coordinates must be finite numbers, got {coords:?}"
        ))
    }
}

/// The daemon's D-Bus interface (see `protocol`). One instance is
/// served on each client connection, all sharing the daemon state.
///
/// zbus runs each call in a task of its own that holds the interface
/// and runs to completion even if the client hangs up (so a key is never
/// left pressed). The connection's slot is therefore held here, freed
/// only when the last call has finished, and `calls` bounds how many a
/// connection can start.
struct Service {
    daemon: Arc<Daemon>,
    hangup: Hangup,
    _connection: OwnedSemaphorePermit,
    calls: Arc<Semaphore>,
}

impl Service {
    /// Admit one call, or refuse it if the connection has too many in
    /// flight.
    fn admit(&self) -> Result<OwnedSemaphorePermit, Error> {
        self.calls.clone().try_acquire_owned().map_err(|_| {
            anyhow!(
                "too many requests in flight on this connection (at most \
                 {MAX_CALLS_PER_CONNECTION})"
            )
            .into()
        })
    }
}

#[zbus::interface(name = "com.mxshift.Waytool.Daemon1")]
impl Service {
    async fn ping(&self) {}

    async fn key(&self, name: String) -> Result<(), Error> {
        let _call = self.admit()?;
        let sym = keysym(&name)?;
        let (_input, portal) = self.daemon.input().await?;
        portal.tap_keysym(sym)?;
        Ok(())
    }

    async fn key_chord(&self, keys: String) -> Result<(), Error> {
        let _call = self.admit()?;
        let syms: Vec<u32> = keys
            .split('+')
            .filter(|p| !p.is_empty())
            .map(|p| {
                crate::keys::keysym_from_name(p).ok_or_else(|| {
                    anyhow!("unknown key name {p:?} in the chord (see `waytool key --help`)")
                })
            })
            .collect::<Result<_>>()?;
        if syms.is_empty() {
            return Err(anyhow!("empty chord").into());
        }
        let (_input, portal) = self.daemon.input().await?;
        key_chord(&portal, &syms).await?;
        Ok(())
    }

    async fn key_code(&self, code: i32) -> Result<(), Error> {
        let _call = self.admit()?;
        let code = u32::try_from(code).map_err(|_| anyhow!("keycode {code} is negative"))?;
        let (_input, portal) = self.daemon.input().await?;
        portal.tap_keycode(code)?;
        Ok(())
    }

    async fn key_down(&self, name: String) -> Result<(), Error> {
        let _call = self.admit()?;
        let sym = keysym(&name)?;
        let (_input, portal) = self.daemon.input().await?;
        portal.keysym(sym, true)?;
        Ok(())
    }

    async fn key_up(&self, name: String) -> Result<(), Error> {
        let _call = self.admit()?;
        let sym = keysym(&name)?;
        let (_input, portal) = self.daemon.input().await?;
        portal.keysym(sym, false)?;
        Ok(())
    }

    async fn type_text(&self, text: String, delay_ms: u32, hold_ms: u32) -> Result<(), Error> {
        let _call = self.admit()?;
        let (_input, portal) = self.daemon.input().await?;
        let hangup = self.hangup.clone();
        type_text(&portal, &text, delay_ms, hold_ms, hangup).await?;
        Ok(())
    }

    /// The portal streams, in index order.
    async fn streams(&self) -> Vec<StreamDesc> {
        self.daemon
            .portal()
            .streams()
            .iter()
            .map(|s| StreamDesc {
                node_id: s.node_id,
                position: s.position,
                size: s.size,
            })
            .collect()
    }

    /// Index of the stream whose portal-reported rect covers global
    /// point (x, y).
    async fn stream_at(&self, x: f64, y: f64) -> Result<u32, Error> {
        let _call = self.admit()?;
        finite(&[x, y])?;
        let idx = self
            .daemon
            .portal()
            .streams()
            .iter()
            .position(|s| s.contains(x, y))
            .ok_or_else(|| anyhow!("no stream covers point ({x}, {y})"))?;
        Ok(idx as u32)
    }
}

/// Serve the interface `make_iface` builds (the daemon's `Service`) on
/// one client connection until the client hangs up, then fire the
/// `Hangup` it was given.
pub(crate) async fn serve_client<I: zbus::object_server::Interface>(
    stream: UnixStream,
    make_iface: impl FnOnce(Hangup) -> I,
) -> Result<()> {
    let (hung_up, hangup) = watch::channel(false);
    let handshake = zbus::connection::Builder::unix_stream(stream)
        .server(zbus::Guid::generate())?
        .p2p()
        .serve_at(crate::protocol::OBJECT_PATH, make_iface(Hangup(hangup)))?
        .build();
    let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .context("client didn't complete the D-Bus handshake")?
        .context("D-Bus handshake")?;
    // The connection's message stream ends when its socket closes.
    let mut messages = zbus::MessageStream::from(&conn);
    while messages.next().await.is_some() {}
    let _ = hung_up.send(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    #[zbus::interface(name = "com.mxshift.Waytool.Test")]
    impl Nothing {}

    #[tokio::test]
    async fn socket_file_binds_once_and_cleans_up() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-socket");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("agent.sock");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = SocketFile::bind(&sock).await.err().unwrap().to_string();
        assert!(err.contains("private"), "{err}");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (file, _listener) = SocketFile::bind(&sock).await.unwrap();
        let mode = std::fs::metadata(&sock).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let err = SocketFile::bind(&sock).await.err().unwrap().to_string();
        assert!(err.contains("already running"), "{err}");

        drop(file);
        assert!(!sock.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn hangup_fires_when_the_client_disconnects() {
        let (server, client) = UnixStream::pair().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let served = tokio::spawn(serve_client(server, |hangup| {
            tx.send(hangup).ok().unwrap();
            Nothing
        }));
        let conn = zbus::connection::Builder::unix_stream(client)
            .p2p()
            .build()
            .await
            .unwrap();
        let mut hangup = rx.await.unwrap();
        assert!(!hangup.happened());

        drop(conn);
        tokio::time::timeout(std::time::Duration::from_secs(5), served)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(hangup.happened());
        // And a pending sleep returns at once rather than running out.
        tokio::time::timeout(std::time::Duration::from_secs(1), hangup.sleep(60_000))
            .await
            .unwrap();
    }
}
