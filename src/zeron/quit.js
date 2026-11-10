// Quit an app by bundle id, through AppKit's normal Quit (so it saves),
// then force-quit it if it hangs. Run with `osascript -l JavaScript`:
// NSRunningApplication needs no Automation permission, unlike Apple Events.
ObjC.import("AppKit");

function run(argv) {
  const id = argv[0];
  const apps = () => ObjC.unwrap($.NSRunningApplication.runningApplicationsWithBundleIdentifier(id));
  const gone = seconds => {
    for (let i = 0; i < seconds * 10; i++) {
      if (!apps().length) return true;
      delay(0.1);
    }
    return false;
  };
  if (!apps().length) return "closed";
  apps().forEach(app => app.terminate);
  if (gone(10)) return "quit";
  apps().forEach(app => app.forceTerminate);
  return gone(5) ? "killed" : "stuck";
}
