//! waytool: command-line keyboard and pointer input, screenshots
//! and window control on Wayland, through the freedesktop RemoteDesktop
//! and ScreenCast portals, with window information from the desktop's
//! own source (see `windows`).
//!
//! Architecture: one persistent daemon owns the session; every other
//! subcommand (`key`, `type`, `click`, `screenshot`, ...) is a thin
//! client that connects to `$XDG_RUNTIME_DIR/waytool.sock` and
//! makes one D-Bus call (see `protocol`).
//!
//! Run `waytool daemon` (in a terminal or as a systemd user unit).
//! Each start shows the screen-sharing consent dialog — accept it.
//! Commands reuse that session until the daemon exits.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

mod atspi;
mod capture;
mod daemon;
mod display;
mod eis;
mod extension;
mod keymap;
mod keys;
mod portal;
mod protocol;
mod wayland;
mod windows;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    about = "Keyboard and pointer input, screenshots and window control on Wayland.",
    long_about = "Keyboard and pointer input, screenshots and window control on Wayland.\n\
                  \n\
                  Input and screenshots go through the RemoteDesktop and ScreenCast portals, \
                  which need libei (GNOME 45+, KDE Plasma 6.1+, COSMIC); the daemon asks for \
                  screen-sharing consent when it starts.\n\
                  \n\
                  [windows]  Needs the desktop's window information: on GNOME, the waytool \
                  gnome-shell extension. Without it these commands fail with an error saying \
                  so.\n\
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
    /// Each start asks for screen-sharing consent, and so does a display
    /// change that ends a shared monitor's stream. Other commands reuse
    /// the session until the daemon exits. Stop it with Ctrl-C, or from
    /// the desktop's screen-sharing indicator.
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
    /// say) types too on GNOME, as text through the gnome-shell extension,
    /// which reaches windows that take text input (GTK, Qt 6, Firefox) but
    /// not X11 apps or emulators; there the whole string goes that way, to
    /// keep its order. A string that can't be typed is rejected before
    /// anything is typed.
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

    /// Move the pointer to a global logical coordinate.
    ///
    /// The space `windows` and `monitors` report rects in.
    MoveGlobal {
        /// Global logical x.
        x: f64,
        /// Global logical y.
        y: f64,
    },

    /// Move the pointer by a relative delta, for apps that lock the pointer.
    ///
    /// DX and DY are in logical pixels. When the focused app holds a
    /// pointer lock (86Box with the guest mouse captured, MartyPC, games)
    /// the compositor pins the pointer and passes the app relative motion,
    /// so `move` and `move-global` do nothing there. Guest mice are
    /// relative too, so steer by deltas and check with a `screenshot`.
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

    /// Move to a global logical coordinate and click.
    ///
    /// Feed it a point from `windows` or `find-window`, such as one inside
    /// the frame (`frame_x`…`frame_h`) or client rect.
    ClickGlobal {
        /// Global logical x.
        x: f64,
        /// Global logical y.
        y: f64,
        /// left, right, middle, back or forward.
        #[arg(long, default_value = "left")]
        button: String,
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
    /// Useful for turning a window position (from `find-window`) into the
    /// stream to screenshot or click on.
    StreamAt {
        /// Global logical x.
        x: f64,
        /// Global logical y.
        y: f64,
    },

    /* ----- windows and monitors ----- */
    /// [windows] List top-level windows (normal windows and dialogs, minimized ones included).
    ///
    /// Fields: id, title, wm_class, app_id, pid, monitor, is_x11, the frame
    /// rect (`frame_x`, `frame_y`, `frame_w`, `frame_h`, with decorations)
    /// and client rect (`client_x`…`client_h`) in global logical
    /// coordinates, focused, minimized, maximized, fullscreen,
    /// on_all_workspaces, workspace and on_active_workspace.
    Windows,

    /// [windows] Print one window's fields by id (from `windows`).
    Window {
        /// Window id.
        id: u64,
    },

    /// [windows] Focus a window by id: switch to its workspace, raise it, and give it keyboard focus.
    Focus {
        /// Window id.
        id: u64,
    },

    /// List monitors: index, connector name (e.g. DP-1), description, rect in global logical coordinates, scale.
    ///
    /// Read from the compositor's Wayland outputs (wl_output and
    /// xdg-output), so it works on every desktop. The scale is physical
    /// pixels per logical pixel: a screenshot of the monitor is its size
    /// times this.
    Monitors,

    /// [windows] Find the window whose wm_class, app_id or title contains a pattern.
    ///
    /// The match ignores case. One match prints its fields; several are an
    /// error listing each candidate, so you can refine the pattern or use
    /// an id, unless `--all` asks for every match.
    FindWindow {
        /// Text to look for.
        pattern: String,
        /// Print every matching window instead of failing when there are
        /// several.
        #[arg(long)]
        all: bool,
    },

    /// [windows] Focus a window and click at a point relative to it.
    ///
    /// X and Y are measured from the window's frame (its visible top-left
    /// corner) in global logical units, the units `windows` uses: divide
    /// screenshot pixels by the monitor's scale. Points outside the window
    /// are rejected, and so is a window that doesn't take focus (a modal
    /// dialog has it, say); the error names the window that has it. A
    /// following `key` or `type` goes to the same window.
    ClickIn {
        /// Window id, or a pattern matching one window's wm_class, app_id
        /// or title.
        window: String,
        /// Offset from the window's left edge.
        x: f64,
        /// Offset from the window's top edge.
        y: f64,
        /// left, right, middle, back or forward.
        #[arg(long, default_value = "left")]
        button: String,
        /// Measure X and Y from the client area (without decorations)
        /// instead of the frame.
        #[arg(long)]
        client: bool,
    },

    /// [windows] Focus a window and type a UTF-8 string into it.
    ///
    /// WINDOW resolves as for `click-in`, and typing works as for `type`.
    /// Fails, typing nothing, if the window doesn't take focus.
    TypeIn {
        /// Window id, or a pattern matching one window's wm_class, app_id
        /// or title.
        window: String,
        /// The text to type.
        text: String,
        /// Milliseconds to wait after each character (at most 10000);
        /// omit for full speed. About 25 suits DOSBox.
        #[arg(long)]
        delay: Option<u32>,
        /// Milliseconds each key stays down (at most 10000); omit to release
        /// it at once.
        #[arg(long)]
        hold: Option<u32>,
    },

    /* ----- accessibility (AT-SPI2) introspection ----- */
    /// [windows] Print a window's accessibility (AT-SPI2) tree.
    ///
    /// One line per element, indented by depth: role, name and rect, so you
    /// can target buttons, menus and fields by name rather than by pixels.
    /// Only apps that expose accessibility info have a tree (GTK, Qt,
    /// Firefox, Chromium; not SDL apps or emulators). When the app's screen
    /// position can be checked (X11 and XWayland apps), rects are relative
    /// to the window's frame and feed straight into `click-in`.
    UiTree {
        /// Window id, or a pattern matching one window's wm_class, app_id
        /// or title.
        window: String,
        /// How many levels to descend.
        #[arg(long, default_value_t = 40)]
        depth: u32,
        /// Include elements that aren't showing (closed menus, hidden
        /// panes).
        #[arg(long)]
        all: bool,
    },

    /// [windows] Find elements in a window's accessibility tree by name.
    ///
    /// Matches names containing PATTERN, ignoring case, hidden elements
    /// included, and prints each with a center point for `click-in`.
    UiFind {
        /// Window id, or a pattern matching one window's wm_class, app_id
        /// or title.
        window: String,
        /// Text to look for in element names.
        pattern: String,
        /// Role name to require, ignoring case, e.g. "push button" or "menu
        /// item".
        #[arg(long)]
        role: Option<String>,
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
        Cmd::MoveGlobal { x, y } => d.move_global(x, y).await.map_err(protocol::describe)?,
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
        Cmd::ClickGlobal { x, y, button } => d
            .click_global(x, y, &button)
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
        Cmd::Windows => {
            let windows = d.windows().await.map_err(protocol::describe)?;
            println!("{}", fmt_dicts("window", &windows));
        }
        Cmd::Window { id } => {
            let window = d.window(id).await.map_err(protocol::describe)?;
            println!("{}", fmt_dicts("window", &[window]));
        }
        Cmd::Focus { id } => d.focus(id).await.map_err(protocol::describe)?,
        Cmd::Monitors => {
            let monitors = d.monitors().await.map_err(protocol::describe)?;
            println!("{}", fmt_dicts("monitor", &monitors));
        }
        Cmd::FindWindow { pattern, all } => {
            let matches = d
                .find_window(&pattern, all)
                .await
                .map_err(protocol::describe)?;
            println!("{}", fmt_dicts("match", &matches));
        }
        Cmd::ClickIn {
            window,
            x,
            y,
            button,
            client,
        } => {
            let (id, gx, gy) = d
                .click_in(&window, x, y, &button, client)
                .await
                .map_err(protocol::describe)?;
            let label = if client { "client area" } else { "frame" };
            println!(
                "clicked {button} at ({x:.0}, {y:.0}) relative to window {id}'s {label} \
                 (global {gx:.0}, {gy:.0})"
            );
        }
        Cmd::TypeIn {
            window,
            text,
            delay,
            hold,
        } => {
            let id = d
                .type_in(&window, &text, delay.unwrap_or(0), hold.unwrap_or(0))
                .await
                .map_err(protocol::describe)?;
            let nchars = text.chars().count();
            println!("typed {nchars} character(s) into window {id}");
        }
        Cmd::UiTree { window, depth, all } => {
            let tree = d
                .ui_tree(&window, depth, all)
                .await
                .map_err(protocol::describe)?;
            println!("{}", ui_coord_header(&tree));
            for n in &tree.nodes {
                let mut line = format!("{}{}", "  ".repeat(n.depth as usize), n.role);
                if !n.name.is_empty() {
                    line.push_str(&format!(" {:?}", n.name));
                }
                if !n.showing {
                    line.push_str(" [hidden]");
                } else if let Some((x, y, w, h)) = n.rect {
                    line.push_str(&format!(" ({x},{y} {w}x{h})"));
                }
                println!("{line}");
            }
            if tree.truncated {
                println!("... truncated at the element budget — use --depth or ui-find");
            }
        }
        Cmd::UiFind {
            window,
            pattern,
            role,
        } => {
            let hits = d
                .ui_find(&window, &pattern, role.as_deref().unwrap_or(""))
                .await
                .map_err(protocol::describe)?;
            println!("{}", ui_coord_header(&hits));
            for n in &hits.nodes {
                let mut line = format!("{} {:?}", n.role, n.name);
                if !n.showing {
                    line.push_str(" [hidden — open its menu/pane first, then re-run]");
                } else if let Some((x, y, w, h)) = n.rect {
                    line.push_str(&format!(
                        " center=({},{}) rect=({x},{y} {w}x{h})",
                        x + w / 2,
                        y + h / 2
                    ));
                }
                println!("{line}");
            }
            if hits.truncated {
                println!("... walk truncated — matches beyond the budget were not seen");
            }
        }
        Cmd::Daemon => unreachable!("the daemon is not a client command"),
    }
    Ok(())
}

/// Header line telling the caller what space ui-tree/ui-find
/// coordinates are in, and how to click them.
fn ui_coord_header(tree: &protocol::UiTree) -> String {
    let id = tree.window_id;
    if tree.frame_relative {
        format!("coords are relative to window {id}'s frame — click with `click-in {id} <x> <y>`")
    } else {
        format!(
            "coords are relative to the app's a11y toplevel (its screen \
             position is unverifiable — typical for Wayland-native apps); \
             for CSD apps this usually equals window {id}'s frame space \
             (`click-in {id} <x> <y>`) — verify against a screenshot"
        )
    }
}

/// Print extension dicts as `key<TAB>value` lines, sorted by key, under
/// a `--- label[i] ---` header per dict.
fn fmt_dicts(label: &str, dicts: &[protocol::WindowDict]) -> String {
    if dicts.is_empty() {
        return format!("no {label}");
    }
    let mut out = String::new();
    for (i, d) in dicts.iter().enumerate() {
        out.push_str(&format!("--- {label}[{i}] ---\n"));
        let mut keys: Vec<&String> = d.keys().collect();
        keys.sort();
        for k in keys {
            out.push_str(&format!("{k}\t{}\n", protocol::render_value(&d[k])));
        }
    }
    out.trim_end().to_string()
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
