import Foundation
import SwiftUI

/// What a chat post turned out to be, derived from its text.
///
/// The wire carries no kind on `SwarmMessageRecord`, so this is read back out of the text the
/// same way the gateway reads it on the way in. Keep `swarmMentionedHandles` in step with the
/// gateway's `mentioned_handles`: a post the gateway accepted as directed has to read as
/// directed here, or the counts disagree with the delivery that actually happened.
enum SwarmMessageKind: Sendable {
    case directed
    case broadcast
}

/// Handles mentioned by one post, mirroring the gateway's parser: an `@` that does not follow
/// a mention byte, then the run of mention bytes after it.
func swarmMentionedHandles(in text: String) -> Set<String> {
    let characters = Array(text.unicodeScalars)
    var handles = Set<String>()
    var index = 0
    while index < characters.count {
        guard characters[index] == "@",
              index == 0 || !isSwarmMentionScalar(characters[index - 1])
        else {
            index += 1
            continue
        }
        var end = index + 1
        while end < characters.count, isSwarmMentionScalar(characters[end]) {
            end += 1
        }
        if end > index + 1 {
            handles.insert(String(String.UnicodeScalarView(characters[(index + 1)..<end])))
        }
        index = max(end, index + 1)
    }
    return handles
}

private func isSwarmMentionScalar(_ scalar: Unicode.Scalar) -> Bool {
    CharacterSet.alphanumerics.contains(scalar) && scalar.isASCII || scalar == "_"
}

struct SwarmStats: Equatable {
    let total: Int
    let counts: [SwarmMessageKind: Int]
    let mentionEdges: Int

    func count(_ kind: SwarmMessageKind) -> Int { counts[kind] ?? 0 }

    /// Every stored post already passed gateway mention validation. Count the handles preserved
    /// in the post rather than intersecting the current roster, because leaving a swarm does not
    /// rewrite its chat history.
    static func make(messages: [SwarmMessageRecord]) -> SwarmStats {
        var counts: [SwarmMessageKind: Int] = [:]
        var edges = 0

        for message in messages {
            let mentioned = swarmMentionedHandles(in: message.text)
            counts[mentioned.isEmpty ? .broadcast : .directed, default: 0] += 1
            edges += mentioned.count
        }

        return SwarmStats(
            total: messages.count,
            counts: counts,
            mentionEdges: edges
        )
    }
}

/// The one section heading on the swarm page, so Activity, Roster, and scratchpad cannot drift
/// apart. Everything drawn above the first of these is page header, not a section.
struct SwarmSectionHeading: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let title: String
    var trailing: String?
    @Binding var isExpanded: Bool

    var body: some View {
        Button {
            withAnimation(reduceMotion ? nil : .snappy(duration: 0.22)) {
                isExpanded.toggle()
            }
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: MobiusSpace.s) {
                // The caret centres on the title rather than sharing its baseline: a glyph is
                // an image with no baseline of its own, so `.firstTextBaseline` would hang it
                // off the bottom edge. The outer stack still baselines the trailing count.
                HStack(spacing: MobiusSpace.xs) {
                    Text(title)
                        .font(.title3.weight(.semibold))
                    // The same sweep the chat list uses on a project folder, so a collapsible
                    // heading behaves the one way everywhere in the app.
                    MobiusIcon(.caretRight, size: 12, foreground: palette.muted)
                        .rotationEffect(.degrees(isExpanded ? 90 : 0))
                }
                Spacer(minLength: 0)
                if let trailing {
                    Text(trailing)
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.muted)
                        .monospacedDigit()
                }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .accessibilityAddTraits(.isHeader)
        .accessibilityValue(isExpanded ? "Expanded" : "Collapsed")
    }
}

struct SwarmStatsSection: View {
    let stats: SwarmStats
    @Binding var isExpanded: Bool

    var body: some View {
        Section {
            if isExpanded {
                LazyVGrid(
                    columns: [GridItem(.adaptive(minimum: 132), spacing: MobiusSpace.s)],
                    spacing: MobiusSpace.s
                ) {
                    SwarmStatTile(title: "Chat posts", value: stats.total)
                    SwarmStatTile(title: "Directed", value: stats.count(.directed))
                    SwarmStatTile(title: "Broadcast", value: stats.count(.broadcast))
                    SwarmStatTile(title: "Mentions", value: stats.mentionEdges)
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
            }
        } header: {
            SwarmSectionHeading(title: "Activity", isExpanded: $isExpanded)
                .textCase(nil)
        }
    }
}

private struct SwarmStatTile: View {
    @Environment(\.mobiusPalette) private var palette
    let title: String
    let value: Int

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
            Text(value.formatted())
                .font(.title2.weight(.semibold))
                .contentTransition(.numericText())
            Text(title)
                .font(MobiusStyle.captionFont)
                .foregroundStyle(palette.muted)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .swarmCard()
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(title): \(value)")
    }
}

extension View {
    /// The one card treatment on the swarm page, so its surfaces cannot drift apart.
    func swarmCard() -> some View {
        modifier(SwarmCardModifier())
    }
}

private struct SwarmCardModifier: ViewModifier {
    @Environment(\.mobiusPalette) private var palette

    func body(content: Content) -> some View {
        content
            .padding(MobiusSpace.l)
            .background(palette.panel, in: MobiusStyle.cardShape)
            .overlay {
                MobiusStyle.cardShape.stroke(palette.line, lineWidth: MobiusStyle.borderWidth)
            }
    }
}
