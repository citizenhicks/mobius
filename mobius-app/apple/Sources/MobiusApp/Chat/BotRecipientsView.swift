import SwiftUI

struct BotRecipientsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let botIDs: [String]
    let remove: (String) -> Void

    var body: some View {
        ScrollView(.horizontal) {
            HStack(spacing: MobiusSpace.s) {
                ForEach(botIDs, id: \.self) { botID in
                    let bot = model.bots.first { $0.id == botID }
                    let name = bot?.name ?? botID
                    Button {
                        remove(botID)
                    } label: {
                        pill(name: name, color: bot?.tint.color)
                    }
                    .buttonStyle(.mobiusPlain)
                    .frame(minHeight: MobiusStyle.rowTouch)
                    .accessibilityLabel("Remove recipient \(name)")
                }
            }
        }
        .scrollIndicators(.hidden)
        .scrollBounceBehavior(.basedOnSize)
        .defaultScrollAnchor(.leading, for: .alignment)
    }

    private func pill(name: String, color: Color?) -> some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusIcon(
                .aiScan, size: MobiusStyle.glyphInline, foreground: color ?? palette.muted,
                gutter: false)
            Text(verbatim: name).lineLimit(1)
            MobiusIcon(
                .x, size: MobiusStyle.glyphMark, foreground: palette.muted, gutter: false)
        }
        .font(MobiusStyle.badgeFont)
        .padding(.horizontal, MobiusSpace.m)
        .padding(.vertical, MobiusSpace.xs)
        .background(palette.accentSoft, in: Capsule())
        .contentShape(Capsule())
    }
}
