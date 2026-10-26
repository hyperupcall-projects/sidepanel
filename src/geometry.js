// Loaded only for the lifetime of a browser helper. Never matches other apps.
(function () {
    const service = "__SERVICE__";
    function watch(window) {
        if (window.pid !== __PID__ || !window.normalWindow) return;
        let restoring = true;
        function record() {
            if (restoring || window.deleted) return;
            const g = window.frameGeometry;
            callDBus(service, "/Geometry", "org.sidepanel.Geometry", "Record",
                JSON.stringify({x: g.x, y: g.y, width: g.width, height: g.height}));
        }
        window.frameGeometryChanged.connect(record);
        callDBus(service, "/Geometry", "org.sidepanel.Geometry", "Read", function (value) {
            if (window.deleted) return;
            const saved = JSON.parse(value);
            if (saved) {
                // If a monitor disappeared, bring the window onto a current output.
                const visible = workspace.screenOrder.some(function (output) {
                    const r = output.geometry;
                    return saved.x < r.x + r.width && saved.x + saved.width > r.x
                        && saved.y < r.y + r.height && saved.y + saved.height > r.y;
                });
                if (!visible) {
                    const area = workspace.clientArea(KWin.WorkArea, window);
                    saved.x = area.x;
                    saved.y = area.y;
                    saved.width = Math.min(saved.width, area.width);
                    saved.height = Math.min(saved.height, area.height);
                }
                window.frameGeometry = saved;
            }
            restoring = false;
            record();
        });
    }
    workspace.windowList().forEach(watch);
    workspace.windowAdded.connect(watch);
})();
