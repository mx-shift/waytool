//! Daemon mode for waytool.
//!
//! Architecture:
//! - `waytool daemon` establishes one RemoteDesktop + ScreenCast
//!   portal session, connects its input through libei (`eis`), finds
//!   the desktop's window source (`windows`), then serves the D-Bus
//!   interface in `protocol` peer-to-peer on a unix socket.
//! - All other subcommands (`key`, `type`, `click`, `screenshot`, ...)
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
//! - A display change that ends a shared monitor's stream needs a new
//!   session, and so new consent (see `spawn_display_watcher`).
//! - Nothing is kept on disk between runs.

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, MutexGuard, Notify, OwnedSemaphorePermit, Semaphore, watch};

use crate::portal::Portal;
use crate::protocol::{Error, Frame, StreamDesc, WindowDict};
use crate::windows::{Windows, dict_i64, summarize_windows, window_matches};

/// Tell systemd about our state when run as a `Type=notify` unit
/// (`READY=1`, `STATUS=...`). A no-op outside systemd. Readiness is
/// only signalled once the portal session is up and the socket is
/// listening, so the unit stays "activating" while the consent dialog
/// is pending — which is what the gnome-shell toggle shows as
/// "waiting for consent".
fn sd_notify(msg: &str) {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let path = path.to_string_lossy().into_owned();
    let addr = match path.strip_prefix('@') {
        Some(name) => SocketAddr::from_abstract_name(name.as_bytes()),
        None => SocketAddr::from_pathname(&path),
    };
    let sent = addr.and_then(|addr| UnixDatagram::unbound()?.send_to_addr(msg.as_bytes(), &addr));
    if let Err(e) = sent {
        eprintln!("waytool: sd_notify({msg:?}) failed: {e}");
    }
}

/// State shared by every client connection.
pub(crate) struct Daemon {
    /// The current portal session, swapped wholesale on re-establish.
    /// Callers take a snapshot (`portal()`) and use that for the whole
    /// request, so nothing holds this lock across an await.
    portal: std::sync::Mutex<Arc<Portal>>,
    /// Serializes input requests, so one client's keystrokes and clicks
    /// never interleave with another's. Read-only requests and captures
    /// don't take it, so a long `type` doesn't block them.
    input: Mutex<()>,
    /// Serializes frame captures, so overlapping screenshots queue
    /// instead of each standing up its own set of streams on the same
    /// nodes.
    capture: Mutex<()>,
    /// Set once shutdown starts, so the session watcher doesn't race it.
    stopping: AtomicBool,
    /// Set while a replacement session is being established, which can
    /// sit on a consent dialog indefinitely.
    reestablishing: AtomicBool,
    /// Asks the main loop to shut down, the same way SIGTERM does.
    shutdown: Notify,
    /// The session bus, for the extension.
    bus: zbus::Connection,
    /// Where window information comes from on this desktop.
    windows: Windows,
    /// The monitor layout, from the daemon's own Wayland connection.
    outputs: Arc<crate::wayland::Wayland>,
}

impl Daemon {
    fn portal(&self) -> Arc<Portal> {
        self.portal.lock().unwrap().clone()
    }

    /// Fail while a re-establish is waiting on consent: the session in
    /// place is about to be replaced, and its streams may already be
    /// gone.
    fn check_ready(&self) -> Result<()> {
        if self.reestablishing.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "the display changed; waiting for screen-sharing consent for the new layout"
            ));
        }
        Ok(())
    }

    /// Take the input lock and the portal to send input through.
    async fn input(&self) -> Result<(MutexGuard<'_, ()>, Arc<Portal>)> {
        self.check_ready()?;
        let guard = self.input.lock().await;
        Ok((guard, self.portal()))
    }
}

/// Watch a portal session and shut the daemon down when it ends: the
/// portal closed it (the user stopped sharing, or revoked consent), or
/// its libei connection was lost, so input can no longer be sent. Stops
/// watching, and lets go of the session, when a re-establish retires it.
///
/// A Closed signal never answers our own Close call (portal Session
/// docs), so closing a retired session doesn't trip this.
fn spawn_session_watcher(portal: &Arc<Portal>, daemon: Arc<Daemon>) {
    let portal = portal.clone();
    tokio::spawn(async move {
        let mut closed = match portal.session.receive_closed().await {
            Ok(closed) => closed,
            Err(e) => {
                eprintln!(
                    "waytool: could not watch session Closed signal ({e:#}); \
                     daemon will not auto-exit when capture stops"
                );
                return;
            }
        };
        let why = tokio::select! {
            // Retirement first: closing a retired session also ends its
            // libei connection, and that mustn't read as a loss.
            biased;
            _ = portal.retired.notified() => return,
            _ = closed.next() => "portal session closed (screen sharing stopped or consent revoked)",
            _ = portal.input_lost() => "lost the libei input connection",
        };
        if daemon.stopping.load(Ordering::SeqCst) {
            return;
        }
        eprintln!("waytool: {why}; shutting down");
        daemon.shutdown.notify_one();
    });
}

/// Re-establish the portal session after a display reconfiguration. The
/// streams of the old layout may be gone, so stand up a completely new
/// session and swap it in. That shows the consent dialog again: GNOME
/// doesn't restore screencast streams from a token. Requests fail
/// meanwhile instead of using a session that's going away; the swap
/// itself waits for requests in flight. If the new session can't be set
/// up, the daemon shuts down rather than carry on with streams that may
/// no longer exist.
async fn reestablish(daemon: &Arc<Daemon>) {
    daemon.reestablishing.store(true, Ordering::SeqCst);
    let established = Portal::establish(&daemon.bus, &daemon.outputs).await;
    let result = match established {
        Ok(portal) => {
            let portal = Arc::new(portal);
            let _input = daemon.input.lock().await;
            let _capture = daemon.capture.lock().await;
            let old = std::mem::replace(&mut *daemon.portal.lock().unwrap(), portal.clone());
            spawn_session_watcher(&portal, daemon.clone());
            old.retired.notify_one();
            if let Err(e) = old.session.close().await {
                eprintln!("waytool: closing the replaced session failed: {e:#}");
            }
            Ok(())
        }
        Err(e) => Err(e),
    };
    daemon.reestablishing.store(false, Ordering::SeqCst);
    match result {
        Ok(()) => eprintln!("waytool: portal session re-established"),
        Err(e) => {
            eprintln!("waytool: re-establishing the session failed ({e:#}); shutting down");
            daemon.shutdown.notify_one();
        }
    }
}

/// Whether the session still fits the display after a reconfiguration:
/// the layout (monitor names, rects and scales, and GNOME's layout
/// mode) is what it was when the session started, and every stream is
/// still there. A change such as a new refresh rate keeps both, and
/// needs no new consent.
async fn session_survives(daemon: &Daemon) -> Result<bool> {
    let portal = daemon.portal();
    if daemon.outputs.monitors() != portal.monitors
        || crate::display::physical_layout(&daemon.bus)
            .await
            .unwrap_or(false)
            != portal.physical_layout
    {
        return Ok(false);
    }
    portal.streams_alive().await
}

/// Say why a screen-sharing dialog is about to appear. A consent prompt
/// nobody asked for, after a display change, otherwise looks like any
/// other app asking — and teaches people to click "Share" without
/// reading. Best-effort: no notification server, no notification.
async fn notify_reconsent(bus: &zbus::Connection) {
    let hints = std::collections::HashMap::<&str, zvariant::Value>::new();
    let sent = bus
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "waytool",
                0u32,
                "preferences-desktop-remote-desktop-symbolic",
                "Display changed",
                "waytool is asking to share the screen again for the new \
                 monitor layout. Approve the dialog to keep it working.",
                Vec::<&str>::new(),
                hints,
                -1i32,
            ),
        )
        .await;
    if let Err(e) = sent {
        eprintln!("waytool: couldn't post a notification: {e}");
    }
}

/// Watch the monitor layout (wl_output and xdg-output, see `wayland`)
/// and re-establish the portal session when a change leaves it out of
/// date (see `session_survives`), so the daemon survives a resolution,
/// scale or layout change instead of serving stale streams.
fn spawn_display_watcher(daemon: Arc<Daemon>) {
    tokio::spawn(async move {
        let mut changes = daemon.outputs.changes();
        loop {
            // Block until the first change of a burst.
            if changes.changed().await.is_err() {
                return; // the Wayland connection is gone
            }
            // Debounce: one reconfiguration changes several outputs in
            // quick succession, and each re-establish may pop a consent
            // dialog — coalesce until 2s of quiet before acting.
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(2), changes.changed())
                    .await
                {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) => return,
                    Err(_) => break,
                }
            }
            match session_survives(&daemon).await {
                Ok(true) => {
                    eprintln!(
                        "waytool: display reconfigured, but the layout and every \
                         shared monitor's stream are unchanged; keeping the session"
                    );
                    continue;
                }
                Ok(false) => {}
                Err(e) => eprintln!("waytool: couldn't check the session ({e:#})"),
            }
            eprintln!(
                "waytool: display reconfiguration detected; re-establishing \
                 the portal session, which asks for screen-sharing consent again"
            );
            notify_reconsent(&daemon.bus).await;
            reestablish(&daemon).await;
        }
    });
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
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;

    // Bind before asking for consent, so a second daemon fails here
    // instead of after the user has already clicked through a dialog.
    // Clients that connect meanwhile queue until the session is up, or
    // give up after their handshake timeout (see protocol::connect).
    let sock = crate::protocol::socket_path()?;
    let (_socket_file, listener) = SocketFile::bind(&sock).await?;

    let bus = zbus::Connection::session()
        .await
        .context("connect to the session bus")?;
    let outputs = crate::wayland::Wayland::connect()?;
    eprintln!("waytool: establishing portal session (consent prompt may appear)...");
    sd_notify("STATUS=Waiting for screen-sharing consent");
    let portal = tokio::select! {
        portal = Portal::establish(&bus, &outputs) => Arc::new(portal?),
        // Stopping the unit while the consent dialog is up.
        _ = sigterm.recv() => return Ok(()),
        _ = sigint.recv() => return Ok(()),
    };
    eprintln!(
        "waytool: session ready, {} stream(s)",
        portal.streams().len()
    );

    let windows = Windows::detect(&bus, &outputs).await;
    eprintln!("waytool: windows from {}", windows.describe());
    let daemon = Arc::new(Daemon {
        portal: std::sync::Mutex::new(portal.clone()),
        input: Mutex::new(()),
        capture: Mutex::new(()),
        stopping: AtomicBool::new(false),
        reestablishing: AtomicBool::new(false),
        shutdown: Notify::new(),
        windows,
        bus,
        outputs,
    });

    // Exit the daemon if the portal session ends out from under us;
    // re-establish it if the display is reconfigured.
    spawn_session_watcher(&portal, daemon.clone());
    spawn_display_watcher(daemon.clone());

    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    eprintln!("waytool: listening on {}", sock.display());
    sd_notify("READY=1\nSTATUS=Running");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _addr)) => stream,
                    // Out of fds, or a connection aborted before it was
                    // accepted: nothing to do with the session, so carry
                    // on after a pause rather than spin.
                    Err(e) => {
                        eprintln!("waytool: accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let Ok(permit) = connections.clone().try_acquire_owned() else {
                    eprintln!(
                        "waytool: refusing a connection: {MAX_CONNECTIONS} already open"
                    );
                    continue;
                };
                let daemon = daemon.clone();
                tokio::task::spawn(async move {
                    if let Err(e) = accept_client(stream, daemon, permit).await {
                        if gave_up_waiting(&e) {
                            eprintln!(
                                "waytool: a client gave up before the daemon was ready"
                            );
                        } else {
                            eprintln!("waytool: client error: {e:#}");
                        }
                    }
                });
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = daemon.shutdown.notified() => break,
        }
    }

    shutdown(&daemon).await;
    Ok(())
}

/// Close the portal session before exiting (portal Session.Close), so
/// screen sharing ends now rather than whenever the portal notices the
/// daemon has left the bus.
async fn shutdown(daemon: &Daemon) {
    eprintln!("waytool: shutting down; closing portal session");
    sd_notify("STOPPING=1");
    daemon.stopping.store(true, Ordering::SeqCst);
    if let Err(e) = daemon.portal().session.close().await {
        eprintln!("waytool: closing portal session failed: {e:#}");
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

    /// Absolute move to a pixel of stream `stream`'s screenshot.
    #[zbus(name = "Move")]
    async fn move_to(&self, x: f64, y: f64, stream: u32) -> Result<(), Error> {
        let _call = self.admit()?;
        finite(&[x, y])?;
        let (_input, portal) = self.daemon.input().await?;
        portal.move_to_pixel(stream, x, y)?;
        Ok(())
    }

    /// Absolute move to global logical coordinates, the space the
    /// extension reports window and monitor rects in.
    async fn move_global(&self, x: f64, y: f64) -> Result<(), Error> {
        let _call = self.admit()?;
        finite(&[x, y])?;
        let (_input, portal) = self.daemon.input().await?;
        portal.move_to_global(x, y)?;
        Ok(())
    }

    /// Relative pointer motion, dx/dy in logical pixels. Absolute moves
    /// do nothing once an app holds a pointer lock (86Box with the guest
    /// mouse captured, games): the compositor pins the pointer and hands
    /// the app relative motion only.
    async fn move_rel(&self, dx: f64, dy: f64) -> Result<(), Error> {
        let _call = self.admit()?;
        finite(&[dx, dy])?;
        let (_input, portal) = self.daemon.input().await?;
        portal.move_rel(dx, dy)?;
        Ok(())
    }

    async fn click(&self, button: String) -> Result<(), Error> {
        let _call = self.admit()?;
        let code = crate::keys::button_code(&button)?;
        let (_input, portal) = self.daemon.input().await?;
        portal.click(code)?;
        Ok(())
    }

    async fn button_down(&self, button: String) -> Result<(), Error> {
        let _call = self.admit()?;
        let code = crate::keys::button_code(&button)?;
        let (_input, portal) = self.daemon.input().await?;
        portal.button(code, true)?;
        Ok(())
    }

    async fn button_up(&self, button: String) -> Result<(), Error> {
        let _call = self.admit()?;
        let code = crate::keys::button_code(&button)?;
        let (_input, portal) = self.daemon.input().await?;
        portal.button(code, false)?;
        Ok(())
    }

    /// Move + click, in screenshot pixels of stream index `stream`.
    async fn click_at(&self, x: f64, y: f64, button: String, stream: u32) -> Result<(), Error> {
        let _call = self.admit()?;
        finite(&[x, y])?;
        let code = crate::keys::button_code(&button)?;
        let (_input, portal) = self.daemon.input().await?;
        portal.move_to_pixel(stream, x, y)?;
        portal.click(code)?;
        Ok(())
    }

    /// Move (global logical coords) + click.
    async fn click_global(&self, x: f64, y: f64, button: String) -> Result<(), Error> {
        let _call = self.admit()?;
        finite(&[x, y])?;
        let code = crate::keys::button_code(&button)?;
        let (_input, portal) = self.daemon.input().await?;
        portal.move_to_global(x, y)?;
        portal.click(code)?;
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

    /// Capture one frame per stream and hand each back as a memfd of
    /// RGBA pixels. The client decides what to do with them; the daemon
    /// never writes files on a caller's behalf.
    async fn screenshot(&self) -> Result<Vec<Frame>, Error> {
        let _call = self.admit()?;
        self.daemon.check_ready()?;
        let _capture = self.daemon.capture.lock().await;
        let portal = self.daemon.portal();
        let node_ids: Vec<u32> = portal.streams().iter().map(|s| s.node_id).collect();
        let fd = portal.pipewire_fd().await?;
        let frames = tokio::task::spawn_blocking(move || {
            crate::capture::pipewire_capture_frames(fd, &node_ids)
        })
        .await
        .context("screenshot blocking task")??;

        let frames = frames
            .into_iter()
            .map(|f| {
                Ok(Frame {
                    width: f.width,
                    height: f.height,
                    rgba: memfd_with(&f.rgba)?.into(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(frames)
    }

    /* ----- these need the desktop's window provider (see `windows`) ----- */

    /// Top-level windows (minimized ones too), as the extension's dicts.
    async fn windows(&self) -> Result<Vec<WindowDict>, Error> {
        let _call = self.admit()?;
        Ok(self.daemon.windows.list().await?)
    }

    /// One window's dict by id.
    async fn window(&self, id: u64) -> Result<WindowDict, Error> {
        let _call = self.admit()?;
        Ok(self.daemon.windows.get(id).await?)
    }

    /// Activate a window by id: switch to its workspace, raise and focus
    /// it.
    async fn focus(&self, id: u64) -> Result<(), Error> {
        let _call = self.admit()?;
        self.daemon.windows.focus(id).await?;
        Ok(())
    }

    /// Monitors: index, connector name, description, rect in global
    /// logical coordinates, and scale.
    async fn monitors(&self) -> Result<Vec<WindowDict>, Error> {
        let _call = self.admit()?;
        let dict = |entries: Vec<(&str, zvariant::Value)>| -> Result<WindowDict, Error> {
            entries
                .into_iter()
                .map(|(k, v)| Ok((k.to_owned(), v.try_to_owned().map_err(zbus::Error::from)?)))
                .collect()
        };
        self.daemon
            .outputs
            .monitors()
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                dict(vec![
                    ("index", (i as u32).into()),
                    ("name", m.name.into()),
                    ("description", m.description.into()),
                    ("x", m.x.into()),
                    ("y", m.y.into()),
                    ("width", m.width.into()),
                    ("height", m.height.into()),
                    ("scale", m.scale.into()),
                ])
            })
            .collect()
    }

    /// Windows whose wm_class/app_id/title contains `pattern`
    /// (case-insensitive). Exactly one match is required unless `all`
    /// is set; several matches is an error listing the candidates.
    async fn find_window(&self, pattern: String, all: bool) -> Result<Vec<WindowDict>, Error> {
        let _call = self.admit()?;
        let windows: Vec<WindowDict> = self.daemon.windows.list().await?;
        let matches: Vec<WindowDict> = windows
            .into_iter()
            .filter(|w| window_matches(w, &pattern))
            .collect();
        match matches.len() {
            0 => Err(anyhow!("no window matched {pattern:?}").into()),
            1 => Ok(matches),
            _ if all => Ok(matches),
            n => Err(anyhow!(
                "{n} windows match {pattern:?} — refine the pattern, target one \
                 by id (`window <id>` / `click-in <id> ...`), or pass --all to \
                 list every match:\n{}",
                summarize_windows(&matches)
            )
            .into()),
        }
    }

    /// Focus a window, move to coordinates relative to its origin, and
    /// click — the one-shot "drive this window" primitive. `window` is a
    /// numeric id or a find-window pattern that must match exactly one
    /// window. `x`/`y` are global-logical units measured from the
    /// window's frame origin (or client origin when `client` is set).
    /// Returns the window id and the global point clicked.
    async fn click_in(
        &self,
        window: String,
        x: f64,
        y: f64,
        button: String,
        client: bool,
    ) -> Result<(u64, f64, f64), Error> {
        let _call = self.admit()?;
        finite(&[x, y])?;
        let code = crate::keys::button_code(&button)?;
        // The input lock is taken before focusing, so another client's
        // focus can't land between this focus and this click.
        let (_input, portal) = self.daemon.input().await?;
        let win = self.daemon.windows.resolve_and_focus(&window).await?;
        let id = dict_i64(&win, "id")?;

        let (prefix, label) = if client {
            ("client", "client area")
        } else {
            ("frame", "frame")
        };
        let ox = dict_i64(&win, &format!("{prefix}_x"))? as f64;
        let oy = dict_i64(&win, &format!("{prefix}_y"))? as f64;
        let w = dict_i64(&win, &format!("{prefix}_w"))? as f64;
        let h = dict_i64(&win, &format!("{prefix}_h"))? as f64;
        if x < 0.0 || y < 0.0 || x >= w || y >= h {
            return Err(anyhow!(
                "offset ({x:.0}, {y:.0}) is outside window {id}'s {label} \
                 ({w:.0}x{h:.0}). click-in coordinates are RELATIVE to the \
                 window's origin, not global screen coordinates."
            )
            .into());
        }
        let (gx, gy) = (ox + x, oy + y);
        portal.move_to_global(gx, gy)?;
        portal.click(code)?;
        Ok((id as u64, gx, gy))
    }

    /// Focus a window, then type into it — same window resolution and
    /// focus-wait as ClickIn, same typing semantics as TypeText. No
    /// pointer movement: keyboard input follows focus. Returns the
    /// window id.
    async fn type_in(
        &self,
        window: String,
        text: String,
        delay_ms: u32,
        hold_ms: u32,
    ) -> Result<u64, Error> {
        let _call = self.admit()?;
        // As in click_in, the input lock covers the focus too.
        let (_input, portal) = self.daemon.input().await?;
        let win = self.daemon.windows.resolve_and_focus(&window).await?;
        let id = dict_i64(&win, "id")?;
        let hangup = self.hangup.clone();
        type_text(&portal, &text, delay_ms, hold_ms, hangup).await?;
        Ok(id as u64)
    }
}

/// An anonymous in-memory file holding `data`, for passing bulk bytes
/// to a client without touching the filesystem.
pub(crate) fn memfd_with(data: &[u8]) -> Result<OwnedFd> {
    use std::io::Write;
    // SAFETY: memfd_create takes a NUL-terminated name and flags, and
    // returns a new fd we immediately take ownership of.
    let raw = unsafe { libc::memfd_create(c"waytool".as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(anyhow!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
    file.write_all(data).context("fill memfd")?;
    Ok(file.into())
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
