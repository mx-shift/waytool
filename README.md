# waytool

waytool types, clicks, takes screenshots, and finds and focuses windows on a Wayland desktop,
from the command line: xdotool's job, which XTEST no longer does under Wayland. A daemon holds
the input and capture session and each command is a short-lived client, so the desktop asks for
screen-sharing consent once per daemon start. Pointer positions can be given in screenshot
pixels, so a script can click on what it sees, scaled monitors included.

## Where it works

| Desktop | Input and screenshots | Windows | Text not on the keyboard layout | Tested on |
|---|---|---|---|---|
| GNOME 45+ | portal, libei | gnome-shell extension | gnome-shell extension | GNOME 50 |
| KDE Plasma 6.1+ | portal, libei | no | no | not yet |
| COSMIC 1.7+ | portal, libei | no | no | not yet |

"portal" is the RemoteDesktop and ScreenCast portals: the daemon asks for consent when it
starts, and you select every monitor to capture or click on.

## Install

Each [release](https://github.com/mx-shift/waytool/releases) has, for x86-64 and aarch64:

| For | File |
|---|---|
| Fedora | `waytool-<version>-1.fc44.<arch>.rpm` |
| Ubuntu 24.04 and newer | `waytool_<version>-1_<arch>.deb` |
| Other Linux (glibc 2.34 or newer) | `waytool-<version>-<arch>-linux.tar.gz`; needs `libpipewire-0.3` and `libxkbcommon` |

The packages install `waytool`, a systemd user unit for the daemon, and the GNOME extension
(not enabled). The tarball has the binary, without the unit or the extension.

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
toggle to Quick Settings, which starts and stops the daemon's systemd user unit. gnome-shell
picks up a new extension only at login.

- From a package: log out and back in, then `gnome-extensions enable waytool@mxshift.com`.
- From a source checkout: `gnome-extension/install.sh`, then log out and back in. It also
  installs the systemd unit, pointing at the checkout's release binary.

## Quick start

```
waytool daemon                            # or switch it on from Quick Settings
waytool screenshot shot.png               # shot-0.png, shot-1.png, ... for several monitors
waytool click-at 400 300 --stream 1       # pixels of stream 1's screenshot
waytool type-in dosbox "DIR" --delay 25
waytool key Return
waytool click-in firefox 40 60            # relative to the window's frame
waytool ui-find calculator 7 --role button
```

`waytool --help` lists the commands and `--help` on one gives the details. Commands marked
[windows] need the GNOME extension.

With a package installed, the daemon can also run as a systemd user unit (`systemctl --user
start waytool`).

## Notes

- Once the daemon runs, any process running as you can make it type, click and capture, with
  no dialog of its own. Other users and Flatpak apps can't reach its socket. On GNOME, the
  extension answers any binary named `waytool`, so while it's enabled such a process can type
  through it even with the daemon stopped.
- `move`, `click-at` and `screenshot` use a stream's pixels; `monitors`, `windows`,
  `click-in`, `move-global` and `click-global` use global logical coordinates. On a scaled
  monitor they differ by the scale.
- Input is refused while logind says the screen is locked. GNOME ends the session when the
  screen locks, which stops the daemon; start it again after unlocking. A display change that
  moves, rescales, adds or removes a monitor needs a new session too, which the daemon sets up
  with a new consent dialog. Lockers that don't set LockedHint aren't detected.
- On GNOME, text the layout lacks goes through gnome-shell's input method: GTK, Qt 6 and
  Firefox take it; X11 apps, emulators and Chromium without `--enable-wayland-ime` don't.
- For emulators that drop fast typing, `--delay MS` waits after each character (25 suits
  DOSBox) and `--hold MS` keeps each key down (VICE).
- An app that has locked the pointer ignores absolute moves; use `move-rel`.
- `ui-tree` and `ui-find` need the app on the AT-SPI2 bus. GTK and Qt apps are; Firefox and
  Chromium join only when they detect assistive technology; SDL apps and emulators don't.

## Testing

`cargo test` runs the unit tests.

## Licence

[Apache License 2.0](LICENSE).
