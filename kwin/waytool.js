// waytool's KWin script: lists windows and focuses one, for the
// waytool daemon on KDE Plasma, where windows are known only to
// KWin. The daemon loads it through KWin's scripting interface
// (org.kde.kwin.Scripting.loadScript) for each request, and unloads it
// after.
//
// A loaded script takes no arguments, so it asks the daemon what to do
// (Request), does it, and sends the result back (Reply), both as JSON.
// The daemon owns the bus name below and answers only KWin.

const SERVICE = 'com.mxshift.Waytool.KWin';
const PATH = '/com/mxshift/Waytool/KWin';
const IFACE = 'com.mxshift.Waytool.KWin';

// A window's id: the first 64 bits of its internal UUID, as hex (a
// JSON number couldn't hold them exactly).
function idOf(w) {
    return w.internalId.toString().replace(/[{}-]/g, '').slice(0, 16);
}

function rect(r) {
    return [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)];
}

function describe(w) {
    return {
        id: idOf(w),
        title: w.caption,
        app_id: w.desktopFileName || w.resourceClass,
        wm_class: w.resourceClass,
        pid: w.pid,
        monitor: w.output ? w.output.name : '',
        frame: rect(w.frameGeometry),
        client: rect(w.clientGeometry),
        focused: w.active,
        minimized: w.minimized,
        // MaximizeFull
        maximized: w.maximizeMode === 3,
        fullscreen: w.fullScreen,
        on_all_workspaces: w.onAllDesktops,
        workspace: w.desktops.length > 0 ? w.desktops[0].name : '',
        on_active_workspace: w.onAllDesktops || w.desktops.includes(workspace.currentDesktop),
    };
}

callDBus(SERVICE, PATH, IFACE, 'Request', function (request) {
    let reply;
    try {
        const req = JSON.parse(request);
        const windows = workspace.windowList().filter(w => w.normalWindow || w.dialog);
        if (req.op === 'list') {
            reply = {windows: windows.map(describe)};
        } else if (req.op === 'focus') {
            const w = windows.find(w => idOf(w) === req.id);
            if (!w)
                throw new Error(`no window with id ${req.id}`);
            if (w.minimized)
                w.minimized = false;
            workspace.activeWindow = w;
            reply = {ok: true};
        } else {
            throw new Error(`unknown request ${req.op}`);
        }
    } catch (e) {
        reply = {error: String(e)};
    }
    callDBus(SERVICE, PATH, IFACE, 'Reply', JSON.stringify(reply));
});
