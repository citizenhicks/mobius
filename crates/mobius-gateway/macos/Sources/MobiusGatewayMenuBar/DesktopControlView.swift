import SwiftUI

struct DesktopControlView: View {
    @Bindable var runtime: DesktopRuntime
    let isConnected: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Toggle("Allow Mac control", isOn: $runtime.enabled)
                .disabled(!isConnected || !runtime.hasPermissions)
            if !runtime.hasPermissions {
                Button("Grant Accessibility and Screen Recording access…") {
                    runtime.requestPermissions()
                }
            }
            if runtime.enabled {
                HStack {
                    Text(runtime.isActive ? "Bot is controlling this Mac" : "Ready for Mac control")
                        .font(.caption)
                    Spacer()
                    Button("Stop", role: .destructive) { runtime.stop() }
                }
            }
            if let message = runtime.message {
                Text(message).font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding()
        .frame(width: 360)
        .onAppear { runtime.refreshPermissions() }
    }
}
