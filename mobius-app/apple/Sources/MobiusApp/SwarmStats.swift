import Foundation
import SwiftUI

/// What a board post turned out to be, derived from its text.
///
/// The wire carries no kind on `SwarmMessageRecord`, so this is read back out of the body the
/// same way the gateway reads it on the way in. Keep `swarmMentionedHandles` in step with the
/// gateway's `mentioned_handles`: a post the gateway accepted as directed has to read as
/// directed here, or the counts disagree with the delivery that actually happened.
enum SwarmMessageKind: Sendable {
    case directed
    case broadcast
}

/// Handles mentioned by one post, mirroring the gateway's parser: an `@` that does not follow
/// a mention byte, then the run of mention bytes after it.
func swarmMentionedHandles(in body: String) -> Set<String> {
    let characters = Array(body.unicodeScalars)
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

/// Emphasises roster mentions in place, so an address reads inside the sentence it was written
/// in rather than as a chip parked beside it.
///
/// Code is left exactly as typed: an `@handle` inside a span or a fence is source, not an
/// address, and bolding it would corrupt what the agent posted.
func swarmHighlightedBody(_ body: String, roster: Set<String>) -> String {
    guard !roster.isEmpty, body.contains("@") else { return body }
    var lines: [String] = []
    var fence: SwarmCodeFence?
    var codeDelimiter: Int?

    for line in body.components(separatedBy: "\n") {
        if let currentFence = fence {
            lines.append(line)
            if currentFence.closes(line) { fence = nil }
        } else if codeDelimiter == nil, let openingFence = SwarmCodeFence.opening(line) {
            fence = openingFence
            lines.append(line)
        } else {
            lines.append(highlightMentions(
                in: line,
                roster: roster,
                codeDelimiter: &codeDelimiter
            ))
        }
    }
    return lines.joined(separator: "\n")
}

private func highlightMentions(
    in line: String,
    roster: Set<String>,
    codeDelimiter: inout Int?
) -> String {
    let scalars = Array(line.unicodeScalars)
    var output = ""
    var index = 0

    while index < scalars.count {
        let scalar = scalars[index]
        if scalar == "`" {
            var end = index + 1
            while end < scalars.count, scalars[end] == "`" { end += 1 }
            let delimiter = end - index
            if codeDelimiter == nil {
                codeDelimiter = delimiter
            } else if codeDelimiter == delimiter {
                codeDelimiter = nil
            }
            output.unicodeScalars.append(contentsOf: scalars[index..<end])
            index = end
            continue
        }
        guard codeDelimiter == nil,
              scalar == "@",
              index == 0 || !isSwarmMentionScalar(scalars[index - 1])
        else {
            output.unicodeScalars.append(scalar)
            index += 1
            continue
        }
        var end = index + 1
        while end < scalars.count, isSwarmMentionScalar(scalars[end]) {
            end += 1
        }
        let handle = String(String.UnicodeScalarView(scalars[(index + 1)..<end]))
        if end > index + 1, roster.contains(handle) {
            output += "**@\(handle)**"
            index = end
        } else {
            output.unicodeScalars.append(scalar)
            index += 1
        }
    }
    return output
}

private struct SwarmCodeFence {
    let marker: Unicode.Scalar
    let length: Int

    static func opening(_ line: String) -> Self? {
        let scalars = Array(line.drop(while: { $0 == " " || $0 == "\t" }).unicodeScalars)
        guard let marker = scalars.first, marker == "`" || marker == "~" else { return nil }
        let length = scalars.prefix(while: { $0 == marker }).count
        return length >= 3 ? Self(marker: marker, length: length) : nil
    }

    func closes(_ line: String) -> Bool {
        let scalars = Array(line.drop(while: { $0 == " " || $0 == "\t" }).unicodeScalars)
        let markerCount = scalars.prefix(while: { $0 == marker }).count
        guard markerCount >= length else { return false }
        return scalars.dropFirst(markerCount).allSatisfy { $0 == " " || $0 == "\t" }
    }
}

struct SwarmStats: Equatable {
    let total: Int
    let counts: [SwarmMessageKind: Int]
    let mentionEdges: Int

    func count(_ kind: SwarmMessageKind) -> Int { counts[kind] ?? 0 }

    /// Every stored post already passed gateway mention validation. Count the handles preserved
    /// in the post rather than intersecting the current roster, because leaving a swarm does not
    /// rewrite its board history.
    static func make(messages: [SwarmMessageRecord]) -> SwarmStats {
        var counts: [SwarmMessageKind: Int] = [:]
        var edges = 0

        for message in messages {
            let mentioned = swarmMentionedHandles(in: message.body)
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

/// The one section heading on the swarm page, so Activity, Roster, and the board cannot drift
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
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            SwarmSectionHeading(title: "Activity", isExpanded: $isExpanded)
            if isExpanded {
                LazyVGrid(
                    columns: [GridItem(.adaptive(minimum: 132), spacing: MobiusSpace.s)],
                    spacing: MobiusSpace.s
                ) {
                    SwarmStatTile(title: "Board posts", value: stats.total)
                    SwarmStatTile(title: "Directed", value: stats.count(.directed))
                    SwarmStatTile(title: "Broadcast", value: stats.count(.broadcast))
                    SwarmStatTile(title: "Mentions", value: stats.mentionEdges)
                }
            }
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
