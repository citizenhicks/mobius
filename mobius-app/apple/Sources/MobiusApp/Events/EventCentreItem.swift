import Foundation

struct EventCentreItem: Identifiable {
    let id: String
    let revision: String
    let title: String
    let detail: String
    let glyph: MobiusGlyph
    let tone: ToastTone
    var occurredAt: TimeInterval = 0
    var requiresAction = false
    let target: AppNotificationTarget
}
