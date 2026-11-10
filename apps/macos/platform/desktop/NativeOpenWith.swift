import AppKit

@MainActor
private final class OpenWithSelection: NSObject {
    var application: URL?

    @objc func choose(_ item: NSMenuItem) {
        application = item.representedObject as? URL
    }
}

/// A native menu stays above Quick Look without hiding the file preview.
@_cdecl("mobius_native_open_with_menu")
public func nativeOpenWithMenu(_ path: UnsafePointer<CChar>) {
    let file = URL(fileURLWithPath: String(cString: path))
    // Exit GPUI’s mutable event context before AppKit enters its menu tracking loop.
    DispatchQueue.main.async {
        let workspace = NSWorkspace.shared
        let preferred = workspace.urlForApplication(toOpen: file)
        let applications = Set(workspace.urlsForApplications(toOpen: file)).sorted {
            if ($0 == preferred) != ($1 == preferred) { return $0 == preferred }
            return $0.lastPathComponent.localizedStandardCompare($1.lastPathComponent)
                == .orderedAscending
        }
        let selection = OpenWithSelection()
        let menu = NSMenu()
        for application in applications {
            var name = FileManager.default.displayName(atPath: application.path)
            if name.hasSuffix(".app") { name.removeLast(4) }
            let title = name + (application == preferred ? " (default)" : "")
            let item = NSMenuItem(
                title: title, action: #selector(OpenWithSelection.choose(_:)), keyEquivalent: "")
            item.target = selection
            item.representedObject = application
            let icon = workspace.icon(forFile: application.path)
            icon.size = NSSize(width: 18, height: 18)
            item.image = icon
            menu.addItem(item)
        }
        if applications.isEmpty {
            let item = NSMenuItem(
                title: "No compatible applications", action: nil, keyEquivalent: "")
            item.isEnabled = false
            menu.addItem(item)
        }
        menu.popUp(positioning: nil, at: NSEvent.mouseLocation, in: nil)
        guard let application = selection.application else { return }
        Task { @MainActor in
            do {
                try await workspace.open(
                    [file], withApplicationAt: application, configuration: .init())
            } catch {
                NativeBridge.shared.fail(error)
            }
        }
    }
}
