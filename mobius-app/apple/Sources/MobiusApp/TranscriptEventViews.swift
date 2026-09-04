import Foundation
import SwiftUI

/// A completed turn becomes one disclosure without changing how the same rows render live.
struct WorkedForGroupView: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ScaledMetric(relativeTo: .body) private var summaryHeight = MobiusStyle.rowRegular
    @State private var isExpanded = false
    let entries: [TranscriptEntry]
    let fileSessionID: String?
    let elapsedMs: UInt64?
    var allowsMessageActions = false
    var revealMessageTarget: MessageTarget?
    var onExpand: () -> Void = {}
    var onRevealMessage: (MessageTarget, TranscriptPresentationID) -> Void = { _, _ in }

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            Button {
                if !isExpanded { onExpand() }
                withAnimation(reduceMotion ? nil : .easeOut(duration: 0.16)) {
                    isExpanded.toggle()
                }
            } label: {
                HStack(spacing: MobiusSpace.s) {
                    MobiusIcon(.combine, size: MobiusStyle.glyphInline, foreground: palette.muted)
                    Text(title)
                        .font(MobiusStyle.bodyFont)
                        .foregroundStyle(palette.muted)
                        .lineLimit(1)
                    Spacer(minLength: MobiusSpace.s)
                    MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
                        .rotationEffect(.degrees(isExpanded ? 90 : 0))
                        .animation(
                            reduceMotion ? nil : .snappy(duration: 0.18),
                            value: isExpanded
                        )
                }
                .frame(minHeight: summaryHeight)
                .contentShape(Rectangle())
            }
            .buttonStyle(.mobiusPlain)
            .accessibilityLabel(Text(title))
            .accessibilityValue(isExpanded ? Text("Expanded") : Text("Collapsed"))
            .accessibilityHint(
                isExpanded
                    ? Text("Collapses the completed work")
                    : Text("Shows the completed work")
            )

            if isExpanded {
                TranscriptRowsView(
                    projection: TranscriptProjection(entries: entries),
                    fileSessionID: fileSessionID,
                    rowSpacing: MobiusSpace.s,
                    allowsMessageActions: allowsMessageActions,
                    onExpandActivityGroup: onExpand
                )
            }
        }
        .task(id: revealMessageTarget) {
            guard let target = revealMessageTarget,
                  entries.contains(where: { $0.messageTarget == target }),
                  let row = TranscriptProjection(entries: entries).rows.first(where: { row in
                      row.records.contains { $0.messageTarget == target }
                  })
            else { return }
            if !isExpanded {
                onExpand()
                withAnimation(reduceMotion ? nil : .easeOut(duration: 0.16)) {
                    isExpanded = true
                }
            }
            await Task.yield()
            guard !Task.isCancelled else { return }
            onRevealMessage(target, row.id)
        }
    }

    private var title: LocalizedStringResource {
        let elapsed = TimeInterval(elapsedMs ?? 0) / 1_000
        return "Worked for \(formatDuration(elapsed))"
    }
}

/// A run of consecutive events behind one summary line, so a long turn costs one row until
/// the reader asks for more.
struct EventGroupView: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// One line, whatever it says. A count climbing from 1 to 47, an icon swapping, the
    /// waiting phrase taking the summary's place: none of it changes the row's height.
    /// Scaled rather than fixed, because `.body` grows with Dynamic Type and a hard 30pt
    /// clips it at accessibility sizes.
    @ScaledMetric(relativeTo: .body) private var summaryHeight = MobiusStyle.rowRegular
    @State private var isExpanded = false
    let entries: [TranscriptEntry]
    let fileSessionID: String?
    let isActive: Bool
    /// The gap between two steps belongs to this row: rather than growing the transcript by a
    /// line that then has to disappear again, the summary hands its slot to the waiting line.
    var waiting: TranscriptWaitingPhrase?
    var onExpand: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            // Files an event produced are the deliverable, not a detail, so they stay out.
            TranscriptFileCards(files: files, sessionID: fileSessionID)
            // The summary slot belongs to the run, not to its contents: while the run holds
            // the waiting phrase it draws the slot whether or not any step has named itself,
            // so naming one costs a crossfade rather than a row's worth of height.
            if !lines.isEmpty || waiting != nil {
                Button {
                    if !isExpanded { onExpand() }
                    withAnimation(.easeOut(duration: 0.16)) { isExpanded.toggle() }
                } label: {
                    header
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityLabel(
                    waiting == nil ? TranscriptEntry.summary(for: lines) : "Waiting for the model"
                )
                .accessibilityHint(
                    isExpanded ? Text("Collapses the steps") : Text("Expands the steps")
                )
                if isExpanded {
                    VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                        ForEach(lines, id: \.presentationID) { entry in
                            if entry.kind == .reasoning {
                                ReasoningLine(entry: entry, isActive: false)
                            } else {
                                EventLine(entry: entry, isActive: false)
                            }
                        }
                    }
                }
            }
        }
    }

    private var header: some View {
        HStack(spacing: MobiusSpace.s) {
            // The group keeps its own mark whether or not it is running: the summary beside
            // it shimmers while the run is live, so swapping in a spinner said the same
            // thing twice and cost the row its identity while it mattered most.
            MobiusIcon(.group01, size: MobiusStyle.glyphInline, foreground: palette.muted)
            Group {
                if let waiting {
                    TranscriptWaitingPhraseText(phrase: waiting)
                } else {
                    Text(TranscriptEntry.summary(for: lines))
                        .font(MobiusStyle.bodyFont)
                        .foregroundStyle(palette.muted)
                        .lineLimit(1)
                        // The count climbs every time a step joins the run; morphing the digit
                        // reads as the same line counting up rather than a new line replacing it.
                        .contentTransition(.numericText())
                        // The group is one transcript step, so its summary owns the running mark.
                        .mobiusRunningShimmer(active: isActive)
                }
            }
            .transition(.opacity)
            .animation(
                reduceMotion ? nil : .easeInOut(duration: TranscriptWaitingNote.crossfade),
                value: waiting != nil
            )
            Spacer(minLength: MobiusSpace.s)
            MobiusIcon(.caretUpDown, size: MobiusStyle.glyphMark, foreground: palette.muted)
        }
        .frame(minHeight: summaryHeight)
        .contentShape(Rectangle())
    }

    private var lines: [TranscriptEntry] {
        entries.filter(\.hasActivityLineContent)
    }

    /// Two events in a run can carry the same file, and `ForEach` needs the ids unique.
    private var files: [SessionFileReference] {
        var seen = Set<String>()
        return entries.flatMap(\.files).filter { seen.insert($0.id).inserted }
    }
}

/// Reasoning is its own disclosure: the first row is the summary and expands in place.
private struct ReasoningLine: View {
    private static let summaryCharacterLimit = 512

    @Environment(\.mobiusPalette) private var palette
    @State private var isExpanded = false
    let entry: TranscriptEntry
    let isActive: Bool

    var body: some View {
        Button {
            withAnimation(.easeOut(duration: 0.16)) { isExpanded.toggle() }
        } label: {
            // A glyph has no baseline, so `.firstTextBaseline` hung this one by its bottom
            // edge and left it sitting low. Centred while the summary is one line, topped
            // once the reasoning expands into a block.
            HStack(alignment: isExpanded ? .top : .center, spacing: MobiusSpace.s) {
                MobiusIcon(.setup01, size: MobiusStyle.glyphInline, foreground: palette.muted)
                Group {
                    if isExpanded {
                        MobiusMarkdownText(entry.text, streaming: entry.pending)
                            .equatable()
                    } else {
                        Text(summary)
                    }
                }
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
                    .multilineTextAlignment(.leading)
                    .lineLimit(isExpanded ? nil : 1)
                    .truncationMode(.tail)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .allowsHitTesting(false)
                    // The transcript owns which phase is current; an older reasoning stream
                    // can remain pending while a later tool call is already running.
                    .mobiusRunningShimmer(active: isActive && !isExpanded)
            }
            .frame(minHeight: MobiusStyle.rowCompact)
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .accessibilityLabel(Text(verbatim: entry.text))
        .accessibilityHint(
            isExpanded ? Text("Collapses the reasoning") : Text("Expands the reasoning")
        )
    }

    private var summary: AttributedString {
        let lineEnd = entry.text.firstIndex(of: "\n") ?? entry.text.endIndex
        let line = entry.text[..<lineEnd]
        let end = line.index(
            line.startIndex,
            offsetBy: Self.summaryCharacterLimit,
            limitedBy: line.endIndex
        ) ?? line.endIndex
        let source = String(line[..<end])
        var summary = (try? AttributedString(markdown: source)) ?? AttributedString(source)
        if end != line.endIndex || lineEnd != entry.text.endIndex {
            summary.append(AttributedString("…"))
        }
        return summary
    }
}

/// The rotating waiting line, wherever it is shown.
private struct TranscriptWaitingPhraseText: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let phrase: TranscriptWaitingPhrase

    var body: some View {
        // The clock drives the rotation, so a transcript rebuild cannot restart it and the
        // message advances on its own schedule rather than on redraws.
        TimelineView(.periodic(from: phrase.startedAt, by: TranscriptWaitingNote.rotation)) { context in
            let elapsed = reduceMotion ? 0 : context.date.timeIntervalSince(phrase.startedAt)
            Text(TranscriptWaitingNote.message(in: phrase.order, elapsed: elapsed))
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
                .lineLimit(1)
                .truncationMode(.tail)
                .contentTransition(.opacity)
                .animation(.easeInOut(duration: 0.3), value: elapsed)
                .mobiusRunningShimmer(active: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        // One stable label: rotating the joke past VoiceOver every few seconds is noise.
        .accessibilityElement()
        .accessibilityLabel("Waiting for the model")
    }
}

/// The bottom of the transcript, as one view with one state.
///
/// The waiting line used to be a row that appeared and disappeared while a group header
/// separately took the phrase over, which meant two views trading a slot and a row's height
/// moving with them. The projection now says which state the tail is in; this draws it.
struct TranscriptTailView: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// `.body` scales with Dynamic Type, so a hard 30pt clips the line at accessibility
    /// sizes. The summary slot and this line share the metric and stay the same height.
    @ScaledMetric(relativeTo: .body) private var lineHeight = MobiusStyle.rowRegular
    let slot: TranscriptWaitingSlot
    /// Owned rather than applied by the transcript: padding outside the condition reserves
    /// the gap while the line is absent, and the arriving row then lands 12pt low.
    let topSpacing: CGFloat

    var body: some View {
        Group {
            // The other cases belong to a row: its summary is its own, and the phrase, when a
            // row holds it, is drawn inside that row's header.
            if case .standaloneLine(let phrase) = slot {
                HStack(spacing: MobiusSpace.s) {
                    MobiusIcon(
                        .neuralNetwork,
                        size: MobiusStyle.glyphInline,
                        foreground: palette.muted
                    )
                    TranscriptWaitingPhraseText(phrase: phrase)
                }
                .frame(maxWidth: .infinity, minHeight: lineHeight, alignment: .leading)
                .padding(.top, topSpacing)
                .transition(
                    reduceMotion ? .opacity : .opacity.combined(with: .offset(y: 8))
                )
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("Waiting for the model")
            }
        }
    }
}

private struct WebSearchDetail: View {
    @Environment(\.mobiusPalette) private var palette
    let detail: String
    let sources: [WebSearchSource]

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            if !detail.isEmpty {
                Text(verbatim: detail)
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
                    .textSelection(.enabled)
            }
            if !sources.isEmpty {
                VStack(alignment: .leading, spacing: MobiusSpace.s) {
                    Text("Sources")
                        .font(MobiusStyle.captionFont.weight(.semibold))
                        .foregroundStyle(palette.muted)
                    ForEach(sources) { source in
                        Link(destination: source.url) {
                            HStack(alignment: .top, spacing: MobiusSpace.s) {
                                VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                                    Text(verbatim: source.title)
                                        .font(MobiusStyle.bodyFont)
                                        .foregroundStyle(palette.accent)
                                        .lineLimit(2)
                                    Text(verbatim: source.host)
                                        .font(MobiusStyle.captionFont)
                                        .foregroundStyle(palette.muted)
                                        .lineLimit(1)
                                    if let excerpt = source.excerpt {
                                        Text(verbatim: excerpt)
                                            .font(MobiusStyle.captionFont)
                                            .foregroundStyle(palette.muted)
                                            .lineLimit(3)
                                    }
                                }
                                Spacer(minLength: MobiusSpace.s)
                                MobiusIcon(
                                    .link,
                                    size: MobiusStyle.glyphMark,
                                    foreground: palette.accent
                                )
                            }
                            .frame(
                                maxWidth: .infinity,
                                minHeight: MobiusStyle.rowTouch,
                                alignment: .leading
                            )
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.mobiusPlain)
                        .accessibilityLabel("Open \(source.title) from \(source.host)")
                        .accessibilityHint("Opens the source in your browser")
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, MobiusSpace.m)
        .padding(.vertical, MobiusSpace.s)
        .background(palette.panel, in: MobiusStyle.controlShape)
    }
}

/// One typed event on one line: its semantic owner, title, and optional detail.
private struct EventLine: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var isExpanded = false
    let entry: TranscriptEntry
    let isActive: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            if isInteractive {
                Button(action: activate) { line }
                    .buttonStyle(.mobiusPlain)
                    .accessibilityLabel(eventAccessibilityLabel)
                    .accessibilityValue(isExpanded ? Text("Expanded") : Text("Collapsed"))
                    .accessibilityHint(Text(accessibilityHint))
            } else {
                line
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel(eventAccessibilityLabel)
            }
            if isExpanded {
                if entry.format == "unified_diff" {
                    InlineUnifiedDiffView(source: entry.text)
                } else if entry.isWebSearch {
                    WebSearchDetail(detail: detail, sources: entry.webSearchSources)
                } else if !entry.eventDetail.isEmpty {
                    Text(verbatim: detail)
                        .font(
                            entry.role == .tool
                                ? MobiusStyle.bodyFont.monospaced()
                                : MobiusStyle.bodyFont
                        )
                        .foregroundStyle(palette.muted)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, MobiusSpace.m)
                        .padding(.vertical, MobiusSpace.s)
                        .background(palette.panel, in: MobiusStyle.controlShape)
                }
            }
        }
    }

    private func activate() {
        withAnimation(.easeOut(duration: 0.16)) { isExpanded.toggle() }
    }

    private var line: some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusIcon(glyph, size: MobiusStyle.glyphInline, foreground: headlineColor)
            HStack(spacing: MobiusSpace.s) {
                middlewareLabel.text
                    .foregroundStyle(palette.accent)
                Text(verbatim: "•")
                    .foregroundStyle(palette.muted)
                headline.text
                    .foregroundStyle(headlineColor)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .mobiusRunningShimmer(active: isActive)
            Spacer(minLength: MobiusSpace.s)
            // No spinner: the shimmer already says this step is running, and two marks for
            // one fact left the trailing slot flickering between them as steps completed.
            if entry.format == "unified_diff" {
                MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
                    .rotationEffect(.degrees(isExpanded ? 90 : 0))
                    .animation(.snappy(duration: 0.18), value: isExpanded)
            } else if !entry.eventDetail.isEmpty || !entry.webSearchSources.isEmpty {
                MobiusIcon(.caretUpDown, size: MobiusStyle.glyphMark, foreground: palette.muted)
            }
        }
        .font(MobiusStyle.bodyFont)
        .frame(minHeight: MobiusStyle.rowCompact)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
    }

    /// A diff says more as a count of changed lines than as the word "Code change".
    private var headline: MobiusText {
        entry.format == "unified_diff" ? diffSummary(entry.text) : .verbatim(entry.headline)
    }

    private var detail: String {
        entry.eventDetail
    }

    private var glyph: MobiusGlyph {
        if entry.kind == .error || entry.tone == "error" { return .xCircle }
        if entry.format == "unified_diff" { return .fileMagnifyingGlass }
        if let symbol = entry.symbol, let glyph = MobiusSymbol.knownGlyph(for: symbol) {
            return glyph
        }
        return switch entry.role {
        case .webSearch: .globe02
        case .artifact: .fileMagnifyingGlass
        case .approval: .checkCircle
        case .activity, .tool, .notice, nil: .typeCursor
        }
    }

    private var middlewareLabel: MobiusText {
        guard let capability = entry.capability else { return .localized("Event") }
        if let feature = model.middlewareFeatures.first(where: { $0.id == capability }) {
            return .verbatim(feature.label)
        }
        return .verbatim(capability.replacingOccurrences(of: "_", with: " ").capitalized)
    }

    private var headlineColor: Color {
        entry.tone == "neutral" ? .primary : palette.tone(entry.tone)
    }

    private var isInteractive: Bool {
        entry.format == "unified_diff"
            || !entry.eventDetail.isEmpty
            || !entry.webSearchSources.isEmpty
    }

    private var accessibilityHint: LocalizedStringResource {
        if entry.format == "unified_diff" {
            return isExpanded ? "Collapses code changes" : "Shows code changes"
        }
        return isExpanded ? "Collapses details" : "Expands details"
    }

    private var eventAccessibilityLabel: Text {
        switch (middlewareLabel, headline) {
        case (.localized(let middleware), .localized(let headline)):
            Text("\(middleware), \(headline)")
        case (.localized(let middleware), .verbatim(let headline)):
            Text("\(middleware), \(headline)")
        case (.verbatim(let middleware), .localized(let headline)):
            Text("\(middleware), \(headline)")
        case (.verbatim(let middleware), .verbatim(let headline)):
            Text("\(middleware), \(headline)")
        }
    }
}
