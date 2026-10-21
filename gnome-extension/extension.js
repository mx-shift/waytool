// waytool gnome-shell extension.
//
// Hosts a D-Bus interface (com.mxshift.Waytool) inside gnome-shell
// so the waytool daemon can enumerate windows, focus them, and
// type text the keyboard layout lacks.  GNOME's own org.gnome.Shell.Introspect API
// does the same thing but gates access by hardcoded caller allowlist
// (orca + a few others); regular apps see AccessDenied.  An extension
// runs inside gnome-shell itself, so it can read the same state
// directly.
//
// The object is exported at /com/mxshift/Waytool on gnome-shell's
// own session-bus connection, so callers address it as
// `org.gnome.Shell` — a name only gnome-shell can own, unlike a name of
// our own that any client could claim first and answer in our place.
//
// Window titles and focus control are exactly what GNOME restricts, so
// this answers only the waytool daemon: the caller has to be a
// process running a binary named `waytool`, outside any Flatpak
// sandbox.  That keeps sandboxed apps with session-bus access out; it
// is not a boundary against unsandboxed code running as you, which can
// rename a binary, or edit this extension, just as well.  Note that
// CommitText and ForwardKey type without the screen-sharing consent the
// daemon's own input needs.
//
// Methods (see IFACE_XML below):
//
//   GetWindows()              -> aa{sv}
//     One dict per top-level window (normal windows and dialogs, not
//     menus or tooltips).  Fields are the metadata the daemon needs for
//     click targeting and focus control: id, title, wm_class, app_id,
//     pid, monitor (its connector name, as `monitors` reports it),
//     frame_x/y/w/h (with decorations) and
//     client_x/y/w/h (the buffer rect), focused/minimized/maximized/
//     fullscreen/on_all_workspaces booleans, the workspace index (-1 on
//     all workspaces) and whether it is on the active one, and is_x11
//     (XWayland clients identify themselves here so the daemon knows
//     whether app_id will be set).
//
//   GetWindow(t id)           -> a{sv}
//     Same dict for a single window.  Errors if id isn't currently
//     mapped.
//
//   FocusWindow(t id)         -> ()
//     Activates the window the way the shell does (Main.activateWindow):
//     switches to its workspace, leaves the overview, raises it and
//     gives it keyboard focus.  Activation is asynchronous; read the
//     window back to see whether focus landed.
//
//   CanCommitText()           -> b
//     Whether the focused window takes text input from the input
//     method (most GTK, Qt and browser windows do; X11 apps don't).
//
//   CommitText(s text)        -> b
//     Types `text` into the focused window through gnome-shell's input
//     method, the way GNOME's on-screen keyboard types emoji: for
//     characters that have no key on the keyboard layout, which can't
//     be sent as key presses.  Returns false, typing nothing, when the
//     focused window doesn't take text input.  An error while an input
//     method is composing text, which a commit would garble.
//
//   ForwardKey(u keysym, u keycode) -> b
//     Presses and releases a key (evdev `keycode`, producing `keysym`)
//     in the focused window through the input method, in order with
//     CommitText: both go through the same queue (see
//     clutter_input_method_forward_key), whereas a key from the
//     daemon's own keyboard device first waits on the input method's
//     engine.  Returns false when the focused window doesn't take text
//     input.
//
// All ids are Meta.Window.get_id() — a stable uint64 valid for the
// window's lifetime.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

Gio._promisify(Gio.DBusConnection.prototype, 'call_with_unix_fd_list');

const OBJECT_PATH = '/com/mxshift/Waytool';
const DAEMON_BINARY = 'waytool';

const IFACE_XML = `
<node>
  <interface name="com.mxshift.Waytool">
    <method name="GetWindows">
      <arg type="aa{sv}" direction="out" name="windows"/>
    </method>
    <method name="GetWindow">
      <arg type="t" direction="in" name="id"/>
      <arg type="a{sv}" direction="out" name="window"/>
    </method>
    <method name="FocusWindow">
      <arg type="t" direction="in" name="id"/>
    </method>
    <method name="CanCommitText">
      <arg type="b" direction="out" name="can_commit"/>
    </method>
    <method name="CommitText">
      <arg type="s" direction="in" name="text"/>
      <arg type="b" direction="out" name="committed"/>
    </method>
    <method name="ForwardKey">
      <arg type="u" direction="in" name="keysym"/>
      <arg type="u" direction="in" name="keycode"/>
      <arg type="b" direction="out" name="forwarded"/>
    </method>
  </interface>
</node>`;

/* ---------- Window dict builder ---------- */

// Returns a plain object whose values are GLib.Variant — Gjs's
// DBusExportedObject marshalls this into a{sv} automatically.
function windowToDict(win, names) {
    const fr = win.get_frame_rect();
    const br = win.get_buffer_rect();

    // Best-effort app id resolution.  GTK / portal-aware apps report
    // gtk_application_id; Flatpak'd apps report sandboxed_app_id.
    // X11 apps generally have neither and rely on wm_class.
    const appId = win.get_gtk_application_id() || win.get_sandboxed_app_id() || '';

    const isX11 = (win.get_client_type() === Meta.WindowClientType.X11);

    // The maximized-* properties exist on every supported release;
    // get_maximized() is gone as of GNOME 49.
    const maximized = win.maximized_horizontally && win.maximized_vertically;

    const workspace = win.is_on_all_workspaces() ? -1 : (win.get_workspace()?.index() ?? -1);
    const active = global.workspace_manager.get_active_workspace();

    return {
        id:          GLib.Variant.new_uint64(win.get_id()),
        title:       GLib.Variant.new_string(win.get_title() || ''),
        wm_class:    GLib.Variant.new_string(win.get_wm_class() || ''),
        app_id:      GLib.Variant.new_string(appId),
        pid:         GLib.Variant.new_int32(win.get_pid()),
        monitor:     GLib.Variant.new_string(names.get(win.get_monitor()) ?? ''),
        is_x11:      GLib.Variant.new_boolean(isX11),
        frame_x:     GLib.Variant.new_int32(fr.x),
        frame_y:     GLib.Variant.new_int32(fr.y),
        frame_w:     GLib.Variant.new_int32(fr.width),
        frame_h:     GLib.Variant.new_int32(fr.height),
        client_x:    GLib.Variant.new_int32(br.x),
        client_y:    GLib.Variant.new_int32(br.y),
        client_w:    GLib.Variant.new_int32(br.width),
        client_h:    GLib.Variant.new_int32(br.height),
        focused:     GLib.Variant.new_boolean(win.has_focus()),
        minimized:   GLib.Variant.new_boolean(win.minimized),
        maximized:   GLib.Variant.new_boolean(maximized),
        fullscreen:  GLib.Variant.new_boolean(win.is_fullscreen()),
        on_all_workspaces: GLib.Variant.new_boolean(win.is_on_all_workspaces()),
        workspace:   GLib.Variant.new_int32(workspace),
        on_active_workspace: GLib.Variant.new_boolean(win.located_on_workspace(active)),
    };
}

/* ---------- Monitor names ---------- */

// The connector name (e.g. "DP-1") of each monitor, by the index
// windows report.  Mirrored outputs share a monitor; their connectors
// are joined with "+".
function connectorNames() {
    const names = new Map();
    for (const lm of global.backend.get_monitor_manager().get_logical_monitors()) {
        names.set(lm.get_number(), lm.get_monitors().map(m => m.get_connector()).join('+'));
    }
    return names;
}

/* ---------- Helpers ---------- */

// Window types a person would call a window, as opposed to menus,
// tooltips, docks and the like that also have window actors.
const TOPLEVEL_TYPES = new Set([
    Meta.WindowType.NORMAL,
    Meta.WindowType.DIALOG,
    Meta.WindowType.MODAL_DIALOG,
    Meta.WindowType.UTILITY,
]);

function allWindows() {
    return global.get_window_actors()
        .map(a => a.get_meta_window())
        .filter(w => w !== null && TOPLEVEL_TYPES.has(w.get_window_type()));
}

/* ---------- Caller check ---------- */

// The pid a pidfd refers to, or -1 once that process has exited.
function pidfdPid(fd) {
    const [, bytes] = GLib.file_get_contents(`/proc/self/fdinfo/${fd}`);
    const match = new TextDecoder().decode(bytes).match(/^Pid:\s*(-?\d+)/m);
    return match ? Number(match[1]) : -1;
}

// Is the sender of `invocation` the waytool daemon?  See the
// header for what this does and doesn't protect against.  Anything
// that can't be checked counts as no.
//
// The caller's process comes from the bus (GetConnectionCredentials).
// Where the bus provides a ProcessFD, that pidfd pins the process: if
// it exits while we look, the pidfd reads -1 rather than a reused pid,
// so another process can't pass in its place.
async function callerIsDaemon(invocation) {
    const [reply, fds] = await Gio.DBus.session.call_with_unix_fd_list(
        'org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
        'GetConnectionCredentials',
        new GLib.Variant('(s)', [invocation.get_sender()]),
        new GLib.VariantType('(a{sv})'), Gio.DBusCallFlags.NONE, -1, null, null);
    const [creds] = reply.deepUnpack();
    // Take the fds out of the list so all of them are closed below, not
    // whenever the list is garbage-collected.
    const received = fds?.steal_fds() ?? [];
    let pidfd = -1;
    try {
        if (creds.ProcessFD !== undefined)
            pidfd = received[creds.ProcessFD.unpack()] ?? -1;
        const pid = pidfd >= 0 ? pidfdPid(pidfd) : creds.ProcessID?.unpack();
        if (!(pid > 0))
            return false;
        // A sandboxed process is never the daemon, whatever its binary
        // is called.
        if (GLib.file_test(`/proc/${pid}/root/.flatpak-info`, GLib.FileTest.EXISTS))
            return false;
        // A daemon whose binary was rebuilt under it reads "(deleted)".
        const exe = GLib.file_read_link(`/proc/${pid}/exe`).replace(/ \(deleted\)$/, '');
        if (GLib.path_get_basename(exe) !== DAEMON_BINARY)
            return false;
        return pidfd < 0 || pidfdPid(pidfd) === pid;
    } catch (_) {
        return false;
    } finally {
        received.forEach(fd => GLib.close(fd));
    }
}

function findWindowById(id) {
    // Meta IDs are uint64; gjs receives them as BigInt over D-Bus.
    // get_id() returns a plain Number unless the value exceeds 2^53;
    // compare as BigInt to be safe.
    const target = BigInt(id);
    for (const w of allWindows()) {
        if (BigInt(w.get_id()) === target) return w;
    }
    return null;
}

/* ---------- D-Bus interface ---------- */

// A D-Bus error with a proper name, rather than the unmapped-GError
// name a thrown GLib.Error would turn into.
class DBusError extends Error {
    constructor(name, message) {
        super(message);
        this.dbusName = name;
    }
}

function noSuchWindow(id) {
    return new DBusError('com.mxshift.Waytool.Error.NoSuchWindow',
        `no window with id ${id}`);
}

// The exported com.mxshift.Waytool object.  Methods use gjs's
// async form (NameAsync(params, invocation)) so each call can check its
// caller before answering.
class AgentDBus {
    constructor() {
        this._impl = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._impl.export(Gio.DBus.session, OBJECT_PATH);
        this._destroyed = false;
    }

    destroy() {
        this._destroyed = true;
        this._impl.unexport();
    }

    // Answer `invocation` with `answer()`, wrapped in `signature`
    // (null for no return value) — if the caller is the daemon.
    async _serve(invocation, signature, answer) {
        try {
            if (!await callerIsDaemon(invocation)) {
                invocation.return_dbus_error('org.freedesktop.DBus.Error.AccessDenied',
                    'only the waytool daemon may call this');
                return;
            }
            // Disabled while the caller check ran — the screen locking,
            // say, which disables extensions.  Don't act any more.
            if (this._destroyed) {
                invocation.return_dbus_error('org.freedesktop.DBus.Error.UnknownObject',
                    'the waytool extension was disabled');
                return;
            }
            const result = answer();
            invocation.return_value(signature ? new GLib.Variant(signature, [result]) : null);
        } catch (e) {
            invocation.return_dbus_error(e.dbusName ?? 'org.freedesktop.DBus.Error.Failed',
                e.message);
        }
    }

    GetWindowsAsync(_params, invocation) {
        this._serve(invocation, '(aa{sv})', () => {
            const names = connectorNames();
            return allWindows().map(w => windowToDict(w, names));
        });
    }

    GetWindowAsync([id], invocation) {
        this._serve(invocation, '(a{sv})', () => {
            const w = findWindowById(id);
            if (!w)
                throw noSuchWindow(id);
            return windowToDict(w, connectorNames());
        });
    }

    FocusWindowAsync([id], invocation) {
        this._serve(invocation, null, () => {
            const w = findWindowById(id);
            if (!w)
                throw noSuchWindow(id);
            // Outside an event the shell's current time is 0, which
            // mutter treats as too old to move focus; ask for the
            // server's current time instead.
            const time = global.display.get_current_time_roundtrip();
            const active = global.workspace_manager.get_active_workspace_index();
            const ws = w.is_on_all_workspaces() ? active : (w.get_workspace()?.index() ?? active);
            Main.activateWindow(w, time, ws);
        });
    }

    CanCommitTextAsync(_params, invocation) {
        this._serve(invocation, '(b)', () => Main.inputMethod.currentFocus !== null);
    }

    CommitTextAsync([text], invocation) {
        this._serve(invocation, '(b)', () => {
            if (Main.inputMethod.currentFocus === null)
                return false;
            if (Main.inputMethod.hasPreedit()) {
                throw new DBusError('com.mxshift.Waytool.Error.Composing',
                    'an input method is composing text in the focused window');
            }
            Main.inputMethod.commit(text);
            return true;
        });
    }

    ForwardKeyAsync([keysym, keycode], invocation) {
        this._serve(invocation, '(b)', () => {
            if (Main.inputMethod.currentFocus === null)
                return false;
            // forward_key takes an XKB keycode: evdev + 8.
            const time = global.display.get_current_time_roundtrip();
            Main.inputMethod.forward_key(keysym, keycode + 8, 0, time, true);
            Main.inputMethod.forward_key(keysym, keycode + 8, 0, time, false);
            return true;
        });
    }
}

/* ---------- Extension class ---------- */

export default class WaytoolExtension extends Extension {
    enable() {
        this._dbus = new AgentDBus();
    }

    disable() {
        this._dbus?.destroy();
        this._dbus = null;
    }
}
