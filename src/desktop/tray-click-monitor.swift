import AppKit

// A separate, windowless process observes mouse clicks in other apps, including Electron.
// No keyboard events, accessibility permission, or event suppression are involved.
let application = NSApplication.shared
application.setActivationPolicy(.prohibited)
guard let monitor = NSEvent.addGlobalMonitorForEvents(
    matching: [.leftMouseDown, .rightMouseDown, .otherMouseDown],
    handler: { event in
        guard let point = event.cgEvent?.location else { return }
        print("\(point.x) \(point.y)")
        fflush(stdout)
    }
) else { exit(1) }

// Closing Electron's pipe also ends the helper if the parent crashes.
FileHandle.standardInput.readabilityHandler = { input in
    if input.availableData.isEmpty { exit(0) }
}
print("ready")
fflush(stdout)
application.run()
