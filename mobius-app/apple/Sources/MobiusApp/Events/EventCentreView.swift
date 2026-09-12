import SwiftUI

struct EventCentreView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        let events = model.eventCentreItems
        PageScaffold(
            title: "Event Centre", detail: "Approvals and results.",
            headerAccessory: {
                Button("Mark all read", glyph: .checkCircle) { model.markEventsRead(events) }
                    .mobiusIconButton()
                    .disabled(!events.contains(where: model.isEventUnread))
            }
        ) {
            Section {
                if events.isEmpty {
                    SettingsCaption("No events yet.")
                }
                ForEach(events) { event in
                    SettingsNavigationRow(
                        hint: "Opens the source",
                        open: { model.openEvent(event) },
                        marks: {
                            if model.isEventUnread(event) {
                                Circle().fill(palette.accent).frame(width: 6, height: 6)
                                    .accessibilityHidden(true)
                            }
                        }
                    ) {
                        SettingsRowLabel(
                            title: .verbatim(event.title), detail: .verbatim(event.detail)
                        ) {
                            MobiusIcon(event.glyph, foreground: event.tone.color(in: palette))
                        }
                        .accessibilityValue(
                            model.isEventUnread(event) ? Text("Unread") : Text("Read"))
                    }
                }
            }
        }
        .refreshable { model.refreshRoutines() }
    }
}
