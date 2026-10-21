//! waytool: command-line keyboard input on Wayland, through the
//! freedesktop RemoteDesktop portal.
//!
//! Architecture: one persistent daemon owns the session; every other
//! subcommand (`key`, `key-chord`, ...) is a thin
//! client that connects to `$XDG_RUNTIME_DIR/waytool.sock` and
//! makes one D-Bus call (see `protocol`).
//!
//! Run `waytool daemon` in a terminal.
//! Each start shows the portal's consent dialog — accept it.
//! Commands reuse that session until the daemon exits.

mod daemon;
mod eis;
mod keys;
mod portal;
mod protocol;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    about = "Keyboard input on Wayland.",
    long_about = "Keyboard input on Wayland.\n\
                  \n\
                  Input goes through the RemoteDesktop portal, which needs libei (GNOME 45+, \
                  KDE Plasma 6.1+, COSMIC); the daemon asks for consent when it starts.\n\
                  \n\
                  Every command except `daemon` talks to the running daemon, so start it first \
                  with `waytool daemon`. Commands then return almost at once."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /* ----- meta / lifecycle ----- */
    /// Run the daemon that holds the input session.
    ///
    /// Each start asks for consent. Other commands reuse the session
    /// until the daemon exits. Stop it with Ctrl-C.
    Daemon,

    /// Check that the daemon is up; prints `pong`.
    Ping,

    /* ----- keyboard ----- */
    /// Press and release one key.
    ///
    /// The key is looked up on the current keyboard layout, and the
    /// modifiers its level needs (Shift, AltGr) are pressed with it.
    Key {
        /// An XKB keysym name (`Return`, `space`, `F1`, `KP_Enter`,
        /// `period`), an alias (`Enter`, `Esc`, `Ctrl`, `Shift`, `Alt`,
        /// `Super`, `Meta`, `PageUp`, `PageDown`, `Backspace`), or a single
        /// character.
        name: String,
    },

    /// Press keys in order, then release them in reverse.
    ///
    /// The held-modifier pattern emulators expect: `Ctrl+F4` holds Ctrl
    /// while F4 goes down and up. Use it for shortcuts like DOSBox-X's
    /// swap-floppy hotkey, and anything that tells a held modifier from
    /// two separate presses.
    KeyChord {
        /// Key names (as for `key`) joined with `+`, e.g. `Ctrl+F4`.
        keys: String,
    },

    /// Press and release a Linux evdev keycode, as is.
    KeyCode {
        /// The evdev code (linux/input-event-codes.h), 0 to 767.
        code: i32,
    },

    /// Press a key and hold it until a matching `key-up`.
    ///
    /// For apps that tell a held key (pinball plungers, accelerating
    /// cursors) from a tap. Keys pressed with it to reach its level are
    /// released with it.
    KeyDown {
        /// A key name, as for `key`.
        name: String,
    },

    /// Release a key held with `key-down`.
    KeyUp {
        /// A key name, as for `key`.
        name: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon => daemon::run_daemon().await,
        cmd => call(cmd).await,
    }
}

/// Run one command against the daemon over D-Bus and print its result.
async fn call(cmd: Cmd) -> Result<()> {
    let d = protocol::connect().await?;
    match cmd {
        Cmd::Ping => {
            d.ping().await.map_err(protocol::describe)?;
            println!("pong");
        }
        Cmd::Key { name } => d.key(&name).await.map_err(protocol::describe)?,
        Cmd::KeyChord { keys } => d.key_chord(&keys).await.map_err(protocol::describe)?,
        Cmd::KeyCode { code } => d.key_code(code).await.map_err(protocol::describe)?,
        Cmd::KeyDown { name } => d.key_down(&name).await.map_err(protocol::describe)?,
        Cmd::KeyUp { name } => d.key_up(&name).await.map_err(protocol::describe)?,
        Cmd::Daemon => unreachable!("the daemon is not a client command"),
    }
    Ok(())
}
