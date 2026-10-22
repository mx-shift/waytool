# waytool

waytool types, clicks, takes screenshots, and finds and focuses windows on a Wayland desktop,
from the command line: xdotool's job, which XTEST no longer does under Wayland. A daemon holds
the input and capture session and each command is a short-lived client, so a desktop that asks
for screen-sharing consent asks once per daemon start. Pointer positions can be given in
screenshot pixels, so a script can click on what it sees, scaled monitors included.

## Where it works

| Desktop | Input and screenshots | Windows | Text not on the keyboard layout | Tested on |
|---|---|---|---|---|
| GNOME 45+ | portal, libei | gnome-shell extension | gnome-shell extension | GNOME 50 |
| KDE Plasma 6.1+ | portal, libei | KWin script | libei text (Plasma 6.8+) | Plasma 6.7.5 |
| COSMIC 1.7+ | portal, libei | COSMIC toplevel protocols | libei text | COSMIC 1.10 |
| sway | Wayland protocols | sway IPC | yes | sway 1.12 (CI) |
| Other wlroots compositors | Wayland protocols | wlr-foreign-toplevel, no positions | yes | labwc 0.9 (CI) |
| Hyprland | Wayland protocols | Hyprland IPC | yes | Hyprland 0.56 |
| niri | Wayland protocols | niri IPC, positions for floating windows only | yes | niri 26.04 (CI) |

"portal" is the RemoteDesktop and ScreenCast portals: the daemon asks for consent when it
starts, and you select every monitor to capture or click on. "Wayland protocols" is the
virtual keyboard and pointer and the screen capture protocols, which any program running as
you can use, so there's no dialog. `WAYTOOL_BACKEND=portal` or `wayland` overrides the choice.

## Install

Each [release](https://github.com/mx-shift/waytool/releases) has, for x86-64 and aarch64:

| For | File |
|---|---|
| Fedora | `waytool-<version>-1.fc44.<arch>.rpm` |
| Ubuntu 24.04 and newer | `waytool_<version>-1_<arch>.deb` |
| Other Linux (glibc 2.34 or newer) | `waytool-<version>-<arch>-linux.tar.gz`; needs `libpipewire-0.3` and `libxkbcommon` |

The packages install `waytool`, a systemd user unit for the daemon, the KWin script, the GNOME
extension (not enabled), and an autostart entry for `waytool tray` (not on GNOME). The tarball
has the binary, without the unit, the KWin script or the extension.

To check a download (GitHub CLI):

```
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify <file> --repo mx-shift/waytool
```

Or build from source, with Rust 1.88 or later:

```
sudo dnf install pipewire-devel libxkbcommon-devel clang      # Fedora
sudo apt install libpipewire-0.3-dev libxkbcommon-dev clang   # Debian, Ubuntu
cargo build --release
```

### GNOME

GNOME needs the extension for windows and for text the layout lacks; it also adds a Waytool
toggle to Quick Settings. gnome-shell picks up a new extension only at login.

- From a package: log out and back in, then `gnome-extensions enable waytool@mxshift.com`.
- From a source checkout: `gnome-extension/install.sh`, then log out and back in. It also
  installs the systemd unit, pointing at the checkout's release binary.

## Quick start

```
waytool daemon                            # or switch it on from Quick Settings or the tray
waytool screenshot shot.png               # shot-0.png, shot-1.png, ... for several monitors
waytool click-at 400 300 --stream 1       # pixels of stream 1's screenshot
waytool type-in dosbox "DIR" --delay 25
waytool key Return
waytool click-in firefox 40 60            # relative to the window's frame
waytool ui-find calculator 7 --role button
```

`waytool --help` lists the commands and `--help` on one gives the details. Commands marked
[windows] need the desktop's window information (see the table).

With a package installed, the daemon runs as a systemd user unit (`systemctl --user start
waytool`). The GNOME toggle starts and stops it; elsewhere `waytool tray`, started at login,
puts an icon in the system tray that does. From a source checkout outside GNOME:

```
systemd/make-unit.sh "$PWD/target/release/waytool" > ~/.config/systemd/user/waytool.service
systemctl --user daemon-reload
target/release/waytool tray &
```

## Notes

- Once the daemon runs, any process running as you can make it type, click and capture, with
  no dialog of its own. Other users and Flatpak apps can't reach its socket. On GNOME, the
  extension answers any binary named `waytool`, so while it's enabled such a process can type
  through it even with the daemon stopped.
- `move`, `click-at` and `screenshot` use a stream's pixels; `monitors`, `windows`,
  `click-in`, `move-global` and `click-global` use global logical coordinates. On a scaled
  monitor they differ by the scale.
- Input is refused while logind says the screen is locked. GNOME ends the session when the
  screen locks; the daemon sets it up again after unlocking, with a new consent dialog. A
  display change that moves, rescales, adds or removes a monitor does the same. Lockers that
  don't set LockedHint (swaylock) aren't detected.
- On GNOME, text the layout lacks goes through gnome-shell's input method: GTK, Qt 6 and
  Firefox take it; X11 apps, emulators and Chromium without `--enable-wayland-ime` don't.
- For emulators that drop fast typing, `--delay MS` waits after each character (25 suits
  DOSBox) and `--hold MS` keeps each key down (VICE).
- An app that has locked the pointer ignores absolute moves; use `move-rel`.
- On niri, absolute moves on a rotated monitor land mirrored.
- `ui-tree` and `ui-find` need the app on the AT-SPI2 bus. GTK and Qt apps are; Firefox and
  Chromium join only when they detect assistive technology; SDL apps and emulators don't.

## Testing

`cargo test` runs the unit tests. `tests/integration/` has end-to-end tests against real
compositors: sway, niri and labwc in a container (CI runs these), and `plasma.sh` inside a
Plasma session.

## Licence

[Apache License 2.0](LICENSE).
