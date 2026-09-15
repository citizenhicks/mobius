import AppKit
import SwiftUI

struct DesktopControlView: View {
    @Bindable var runtime: DesktopRuntime
    let isConnected: Bool

    var body: some View {
        Menu("Mac control") {
            Toggle("Allow Mac control", isOn: $runtime.enabled)
                .disabled(!isConnected)
            Divider()
            Button(action: runtime.requestAccessibility) {
                Label(
                    "Accessibility",
                    systemImage: runtime.hasAccessibility ? "checkmark.circle" : "circle")
            }
            .disabled(runtime.hasAccessibility)
            Button(action: runtime.requestScreenRecording) {
                Label(
                    "Screen Recording",
                    systemImage: runtime.hasScreenRecording ? "checkmark.circle" : "circle")
            }
            .disabled(runtime.hasScreenRecording)
            if runtime.enabled {
                Divider()
                if !runtime.hasPermissions {
                    Text("Grant Accessibility and Screen Recording to allow control.")
                } else if !runtime.isRegistered {
                    Text("Registering Mac control…")
                } else {
                    Text(runtime.isActive ? "Bot is controlling this Mac" : "Ready for Mac control")
                }
                Button("Stop Mac control", role: .destructive, action: runtime.stop)
            }
            if let message = runtime.message {
                Text(message)
            }
        }
        .onAppear { runtime.refreshPermissions() }
        .onReceive(
            NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)
        ) { _ in
            runtime.refreshPermissions()
        }
    }
}
