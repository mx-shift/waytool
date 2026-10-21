//! waytool: command-line keyboard input on Wayland, through the
//! freedesktop RemoteDesktop and ScreenCast portals.
//!
//! Architecture: one persistent daemon owns the session; every other
//! subcommand (`key`, `type`, `streams`, ...) is a thin
//! client that connects to `$XDG_RUNTIME_DIR/waytool.sock` and
//! makes one D-Bus call (see `protocol`).
//!
//! Run `waytool daemon` in a terminal.
//! Each start shows the screen-sharing consent dialog — accept it.
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
                  Input goes through the RemoteDesktop and ScreenCast portals, \
                  which need libei (GNOME 45+, KDE Plasma 6.1+, COSMIC); the daemon asks for \
                  screen-sharing consent when it starts.\n\
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
    /// Run the daemon that holds the input and capture session.
    ///
    /// Each start asks for screen-sharing consent. Other commands reuse
    /// the session until the daemon exits. Stop it with Ctrl-C.
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

    /// Type a UTF-8 string, one key press and release per character.
    ///
    /// A newline or tab types Return or Tab, and the modifiers each
    /// character's level needs are pressed with it; use `key-chord` for
    /// shortcuts. A character that isn't on the keyboard layout (an emoji,
    /// say) can't be typed; a string with one is rejected before anything
    /// is typed.
    Type {
        /// The text to type.
        text: String,
        /// Milliseconds to wait after each character (at most 10000);
        /// omit for full speed. About 25 suits DOSBox, whose emulated
        /// keyboard drops characters typed too fast.
        #[arg(long)]
        delay: Option<u32>,
        /// Milliseconds each key stays down (at most 10000); omit to release
        /// it at once. For emulators that scan a keyboard matrix (VICE) and
        /// miss a key pressed and released between two scans.
        #[arg(long)]
        hold: Option<u32>,
    },

    /* ----- capture ----- */
    /// List the capture streams: index, PipeWire node, position and size.
    ///
    /// These are the monitors shared in the consent dialog, each with its
    /// PipeWire node. Position and size are the monitor's rect in global
    /// logical coordinates.
    Streams,

    /// Print which stream covers a global logical point.
    ///
    /// Useful for turning a global point into the stream that shows it.
    StreamAt {
        /// Global logical x.
        x: f64,
        /// Global logical y.
        y: f64,
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
        Cmd::Type { text, delay, hold } => d
            .type_text(&text, delay.unwrap_or(0), hold.unwrap_or(0))
            .await
            .map_err(protocol::describe)?,
        Cmd::Streams => {
            let streams = d.streams().await.map_err(protocol::describe)?;
            if streams.is_empty() {
                println!("no streams");
            } else {
                println!("idx\tnode\tpos\tsize");
            }
            for (i, s) in streams.iter().enumerate() {
                let pos = s
                    .position
                    .map(|(x, y)| format!("{x},{y}"))
                    .unwrap_or_else(|| "?".into());
                let size = s
                    .size
                    .map(|(w, h)| format!("{w}x{h}"))
                    .unwrap_or_else(|| "?".into());
                println!("{i}\t{}\t{pos}\t{size}", s.node_id);
            }
        }
        Cmd::StreamAt { x, y } => {
            println!("{}", d.stream_at(x, y).await.map_err(protocol::describe)?);
        }
        Cmd::Daemon => unreachable!("the daemon is not a client command"),
    }
    Ok(())
}
