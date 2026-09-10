import SwiftUI

struct VoiceShortcutSettings: View {
    @Bindable var shortcuts: VoiceShortcuts

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.l) {
            Text("Voice keyboard shortcuts").font(.headline)
            Text(
                "Control voice from any app, including Mini mode. Click a shortcut to record it; include Command or Control. Press Escape to cancel."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
            ForEach(VoiceShortcutAction.allCases) { action in
                HStack {
                    Text(action.title)
                    Spacer()
                    Button {
                        shortcuts.beginRecording(action)
                    } label: {
                        Text(
                            shortcuts.recording == action
                                ? "Press shortcut…"
                                : shortcuts.shortcut(for: action)?.label ?? "Record shortcut"
                        )
                        .frame(width: 150)
                    }
                    .accessibilityLabel("Record \(action.title) shortcut")
                    .accessibilityValue(
                        shortcuts.recording == action
                            ? "Recording" : shortcuts.shortcut(for: action)?.label ?? "Not assigned"
                    )
                    Button("Clear") {
                        do { try shortcuts.set(nil, for: action) } catch {
                            shortcuts.error = error.localizedDescription
                        }
                    }
                    .disabled(shortcuts.shortcut(for: action) == nil)
                    .accessibilityLabel("Clear \(action.title) shortcut")
                }
            }
            if let error = shortcuts.error {
                Text(verbatim: error)
                    .font(.callout)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(24)
        .frame(width: 520)
        .onDisappear { shortcuts.cancelRecording() }
    }
}
