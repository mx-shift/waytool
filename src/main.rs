//! waytool: command-line keyboard and pointer input and screenshots
//! on Wayland, through the freedesktop RemoteDesktop and ScreenCast
//! portals.
//!
//! Architecture: one persistent daemon owns the session; every other
//! subcommand (`key`, `type`, `click`, `screenshot`, ...) is a thin
//! client that connects to `$XDG_RUNTIME_DIR/waytool.sock` and
//! makes one D-Bus call (see `protocol`).
//!
//! Run `waytool daemon` in a terminal.
//! Each start shows the screen-sharing consent dialog — accept it.
//! Commands reuse that session until the daemon exits.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

mod capture;
mod daemon;
mod display;
mod eis;
mod keys;
mod portal;
mod protocol;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    about = "Keyboard and pointer input and screenshots on Wayland.",
    long_about = "Keyboard and pointer input and screenshots on Wayland.\n\
                  \n\
                  Input and screenshots go through the RemoteDesktop and ScreenCast portals, \
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

    /* ----- pointer ----- */
    /// Move the pointer to a pixel of a stream's screenshot.
    ///
    /// X and Y are pixels of that stream's `screenshot` (0 to width and
    /// height of its PNG), so they match what you read off it. Every pixel
    /// is reachable, on scaled monitors too.
    Move {
        /// Pixel column in the stream's screenshot.
        x: f64,
        /// Pixel row in the stream's screenshot.
        y: f64,
        /// Stream index, from `streams`.
        #[arg(long, default_value_t = 0)]
        stream: u32,
    },

    /// Move the pointer by a relative delta, for apps that lock the pointer.
    ///
    /// DX and DY are in logical pixels. When the focused app holds a
    /// pointer lock (86Box with the guest mouse captured, MartyPC, games)
    /// the compositor pins the pointer and passes the app relative motion,
    /// so `move` does nothing there. Guest mice are relative too, so steer
    /// by deltas and check with a `screenshot`.
    MoveRel {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },

    /// Press and release a pointer button where the pointer is.
    Click {
        /// left, right, middle, back or forward.
        #[arg(long, default_value = "left")]
        button: String,
    },

    /// Press a pointer button and hold it until a matching `button-up`.
    ///
    /// For drag and drop, and classic Mac menus: `button-down`, `move` the
    /// pointer, then `button-up` over the target.
    ButtonDown {
        /// left, right, middle, back or forward.
        #[arg(long, default_value = "left")]
        button: String,
    },

    /// Release a pointer button held with `button-down`.
    ButtonUp {
        /// left, right, middle, back or forward.
        #[arg(long, default_value = "left")]
        button: String,
    },

    /// Move to a pixel of a stream's screenshot and click.
    ///
    /// Coordinates as for `move`.
    ClickAt {
        /// Pixel column in the stream's screenshot.
        x: f64,
        /// Pixel row in the stream's screenshot.
        y: f64,
        /// left, right, middle, back or forward.
        #[arg(long, default_value = "left")]
        button: String,
        /// Stream index, from `streams`.
        #[arg(long, default_value_t = 0)]
        stream: u32,
    },

    /* ----- capture ----- */
    /// Capture one frame per stream (shared monitor), as PNG.
    ///
    /// With one stream it writes OUT; with several, OUT's name with `-N`
    /// added per stream (shot.png → shot-0.png, shot-1.png). Prints
    /// `wrote PATH` for each file.
    Screenshot {
        /// Where to write; should end in `.png` (the format follows the
        /// extension).
        out: PathBuf,
    },

    /// List the capture streams: index, PipeWire node, position and size.
    ///
    /// These are the monitors shared in the consent dialog, each with its
    /// PipeWire node. Position and size are the monitor's rect in global
    /// logical coordinates.
    Streams,

    /// Print which stream covers a global logical point.
    ///
    /// Useful for turning a global point into the stream to screenshot or
    /// click on.
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
        Cmd::Move { x, y, stream } => d.move_to(x, y, stream).await.map_err(protocol::describe)?,
        Cmd::MoveRel { dx, dy } => d.move_rel(dx, dy).await.map_err(protocol::describe)?,
        Cmd::Click { button } => d.click(&button).await.map_err(protocol::describe)?,
        Cmd::ButtonDown { button } => d.button_down(&button).await.map_err(protocol::describe)?,
        Cmd::ButtonUp { button } => d.button_up(&button).await.map_err(protocol::describe)?,
        Cmd::ClickAt {
            x,
            y,
            button,
            stream,
        } => d
            .click_at(x, y, &button, stream)
            .await
            .map_err(protocol::describe)?,
        Cmd::Screenshot { out } => {
            let frames = d.screenshot().await.map_err(protocol::describe)?;
            let paths = screenshot_paths(&out, frames.len());
            for (frame, path) in frames.into_iter().zip(paths) {
                frame_to_image(frame)?
                    .save(&path)
                    .with_context(|| format!("save {}", path.display()))?;
                println!("wrote {}", path.display());
            }
        }
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

/// Where `screenshot OUT` writes `n` frames: exactly `OUT` for one,
/// `<stem>-N.<ext>` for several.
fn screenshot_paths(out: &Path, n: usize) -> Vec<PathBuf> {
    if n == 1 {
        return vec![out.to_path_buf()];
    }
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("frame");
    let ext = out.extension().and_then(|s| s.to_str()).unwrap_or("png");
    (0..n)
        .map(|i| out.with_file_name(format!("{stem}-{i}.{ext}")))
        .collect()
}

/// Read a frame's pixels out of the memfd the daemon passed.
fn frame_to_image(frame: protocol::Frame) -> Result<image::RgbaImage> {
    use std::os::unix::fs::FileExt;
    let (w, h) = (frame.width, frame.height);
    let len = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("frame size {w}x{h} overflows"))?;
    let file = std::fs::File::from(OwnedFd::from(frame.rgba));
    // Check the claimed size against what the memfd holds before
    // allocating for it.
    let size = file.metadata().context("stat the frame memfd")?.len();
    if size != len as u64 {
        return Err(anyhow!(
            "{w}x{h} frame should be {len} bytes, but the daemon sent {size}"
        ));
    }
    let mut rgba = vec![0; len];
    file.read_exact_at(&mut rgba, 0)
        .with_context(|| format!("read {w}x{h} frame from the daemon"))?;
    image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| anyhow!("bad {w}x{h} frame"))
}
