import Foundation
import SwiftUI
@preconcurrency import AVFoundation

@MainActor
private final class MessageSpeaker {
    private let synthesizer = AVSpeechSynthesizer()
    private var speechTask: Task<Void, Never>?

    init() {
        synthesizer.usesApplicationAudioSession = false
    }

    func speak(_ markdown: String) {
        speechTask?.cancel()
        _ = synthesizer.stopSpeaking(at: .immediate)
        speechTask = Task { [weak self] in
            let text = await markdown.markdownToPlainText()
            guard let self,
                  !Task.isCancelled,
                  !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            else { return }
            synthesizer.speak(AVSpeechUtterance(string: text))
        }
    }

    func stop() {
        speechTask?.cancel()
        speechTask = nil
        _ = synthesizer.stopSpeaking(at: .immediate)
    }
}

/// The transcript body shared by the full chat and read-only agent previews.
/// Navigation, pagination, and composing controls stay with their owning surface.
///
/// The projection decides what a row is, what it is called, how it is sized and who holds the
/// waiting line. This view only draws it.
struct TranscriptRowsView: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var visibleRowIDs = Set<TranscriptPresentationID>()
    @State private var hasAppeared = false
    @State private var speaker = MessageSpeaker()
    let projection: TranscriptProjection
    let fileSessionID: String?
    var activeStepID: TranscriptPresentationID?
    var rowSpacing: CGFloat = 12
    var turnDiff: (TranscriptEntry) -> String = { _ in "" }
    var onExpandActivityGroup: () -> Void = {}

    var body: some View {
        ForEach(projection.rows.enumerated(), id: \.element.id) { index, row in
            let isVisible = !hasAppeared || visibleRowIDs.contains(row.id)
            VStack(alignment: .leading, spacing: 0) { self.row(row) }
                .id(row.id)
                .padding(.top, index == 0 ? 0 : rowSpacing)
                // The scroll view owns movement; rows only fade in.
                .opacity(isVisible ? 1 : 0)
        }
        .onAppear {
            visibleRowIDs = Set(projection.rows.map(\.id))
            hasAppeared = true
        }
        .onChange(of: projection.rows.map(\.id)) { _, rowIDs in
            withAnimation(reduceMotion ? nil : .easeOut(duration: 0.5)) {
                visibleRowIDs = Set(rowIDs)
            }
        }
        .onDisappear {
            speaker.stop()
        }
    }

    @ViewBuilder
    private func row(_ row: TranscriptPresentationRow) -> some View {
        switch row.kind {
        case .activityGroup:
            EventGroupView(
                entries: row.records,
                fileSessionID: fileSessionID,
                isActive: row.records.contains { $0.presentationID == activeStepID },
                waiting: projection.waiting.phrase(forRow: row.id),
                onExpand: onExpandActivityGroup
            )
        case .workedGroup:
            WorkedForGroupView(
                entries: row.records,
                fileSessionID: fileSessionID,
                elapsedMs: row.elapsedMs,
                onExpand: onExpandActivityGroup
            )
        case .user, .peer, .narrative:
            if let entry = row.records.first {
                TranscriptRow(
                    entry: entry,
                    isUser: row.kind == .user,
                    isPeer: row.kind == .peer,
                    speaker: speaker,
                    fileSessionID: fileSessionID,
                    turnDiff: turnDiff(entry)
                )
            }
        }
    }
}

struct TranscriptPaginationButton: View {
    @Environment(\.mobiusPalette) private var palette
    let isLoading: Bool
    let isEnabled: Bool
    let action: () -> Void

    var body: some View {
        HStack {
            Spacer()
            Button(action: action) {
                MobiusLabel(
                    title: isLoading ? "Loading earlier messages" : "Load earlier messages",
                    glyph: .arrowUp,
                    iconColor: palette.accent
                )
                .frame(minHeight: MobiusStyle.iconButtonSize)
            }
            .buttonStyle(.mobiusPlain)
            .foregroundStyle(isEnabled ? palette.accent : palette.muted)
            .tint(palette.accent)
            .disabled(!isEnabled)
            .accessibilityLabel(
                isLoading ? "Loading earlier messages" : "Load earlier messages"
            )
            Spacer()
        }
    }
}

/// Who is allowed to move the transcript, and why.
///
/// The bottom anchor and an explicit `scrollTo` both correct the same offset, so leaving both
/// live means the visible result is whichever ran last. Exactly one is active per mode.
private let transcriptOrbSize: CGFloat = 144

private enum TranscriptScrollMode {
    /// Parked at the end. The bottom anchor follows content growth; nothing else scrolls.
    case followingTail
    /// The reader is somewhere else. Nothing follows, and structure lands without animation.
    case freeScrolling
    /// A page of history was prepended. The previously visible turn is restored, not the tail.
    case restoringHistory

    var followsContentGrowth: Bool { self == .followingTail }
}

struct TranscriptView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let bottomInset: CGFloat
    @Binding var isAtBottom: Bool
    let scrollToBottomRequest: Int
    // A restored transcript can land after the scroll view exists. The bottom-edge position
    // survives that late fill, while ChatView supplies a fresh identity for each presentation.
    @State private var position = ScrollPosition(edge: .bottom)
    @State private var historyBoundaryID: TranscriptPresentationID?
    @State private var historyAnchorID: TranscriptPresentationID?
    @State private var scrollMode = TranscriptScrollMode.followingTail
    @State private var waiting = TranscriptWaitingHold()
    private let rowSpacing = MobiusStyle.transcriptRowSpacing
    private let contentPadding = MobiusStyle.transcriptPadding

    @ViewBuilder
    var body: some View {
        if model.isLoadingTranscript {
            TranscriptLoadingView(bottomInset: bottomInset)
        } else {
            transcript
        }
    }

    private var transcript: some View {
        ScrollView {
            // ponytail: chat rows have wildly different heights, so exact layout avoids the
            // blank gaps produced by LazyVStack estimates. Paginate before making this lazy again.
            VStack(alignment: .leading, spacing: 0) {
                if model.hasEarlierHistory {
                    TranscriptPaginationButton(
                        isLoading: model.isLoadingEarlierHistory,
                        isEnabled: model.canLoadEarlierHistory,
                        action: loadEarlierHistory
                    )
                    .padding(.bottom, rowSpacing)
                }
                TranscriptRowsView(
                    projection: projection,
                    fileSessionID: model.selectedSessionID,
                    activeStepID: model.activeTranscriptStepID,
                    rowSpacing: rowSpacing,
                    turnDiff: { model.turnDiff(for: $0) },
                    onExpandActivityGroup: { scrollMode = .freeScrolling }
                )
                TranscriptTailView(slot: projection.waiting, topSpacing: rowSpacing)
                ForEach(model.transcriptTailWidgets) { widget in
                    QueuedMessageView(widget: widget)
                        .geometryGroup()
                        .padding(.top, rowSpacing)
                }
                Color.clear.frame(height: max(1, bottomInset))
            }
            .scrollTargetLayout()
            .frame(maxWidth: MobiusStyle.transcriptWidth)
            .frame(maxWidth: .infinity)
            .padding(contentPadding)
        }
        // The keyboard insets this scroll view, so a plain canvas background stops at the
        // keyboard's top edge and the rounded corners expose black. Every other page paints
        // its backdrop the same way, which is why only the chat showed the cut.
        .background(MobiusBackdrop())
        .scrollPosition($position)
        .defaultScrollAnchor(.bottom, for: .initialOffset)
        // The one automatic mechanism, and only while parked at the end. Off, growth lands
        // below the fold and the reader keeps their place.
        .defaultScrollAnchor(
            scrollMode.followsContentGrowth ? .bottom : nil,
            for: .sizeChanges
        )
        // Match the row fade so the bottom-anchor correction no longer lands a frame first.
        .animation(
            reduceMotion || !scrollMode.followsContentGrowth ? nil : .easeOut(duration: 0.5),
            value: projection.structuralRevision
        )
        .scrollIndicators(.hidden)
        .scrollDismissesKeyboard(.interactively)
        .refreshable { loadEarlierHistory() }
        .onAppear {
            scrollMode = .followingTail
            position.scrollTo(edge: .bottom)
        }
        .overlay {
            if model.displayedTranscript.isEmpty {
                emptyState
            }
        }
        // Measured against the furthest reachable offset, including the bottom inset:
        // comparing the visible rect to the content height never reads as "at bottom".
        .onScrollGeometryChange(for: Bool.self) { Self.atBottom($0) } action: { _, atBottom in
            isAtBottom = atBottom
        }
        // The reader's own intent, and the only thing that takes the transcript out of
        // following. A drag ends the follow; coming to rest at the end restores it.
        .onScrollPhaseChange { _, phase, context in
            guard scrollMode != .restoringHistory, phase != .animating else { return }
            scrollMode = phase == .idle && Self.atBottom(context.geometry)
                ? .followingTail
                : .freeScrolling
        }
        .onChange(of: scrollToBottomRequest) {
            withAnimation(.easeOut(duration: 0.2)) { position.scrollTo(edge: .bottom) }
        }
        .onChange(of: model.historyLoadCompletionRevision) { restoreHistoryAnchor() }
        .onChange(of: model.selectedSessionID) {
            historyBoundaryID = nil
            historyAnchorID = nil
            scrollMode = .followingTail
            position = ScrollPosition(edge: .bottom)
        }
        .onChange(of: model.isWaitingForModel, initial: true) { _, isWaiting in
            rescheduleWaitingPhrase(isWaiting)
        }
        .onDisappear { rescheduleWaitingPhrase(false) }
    }

    private var projection: TranscriptProjection {
        model.transcriptProjection(
            breakBefore: historyBoundaryID,
            waitingPhrase: waitingPhrase
        )
    }

    private var waitingPhrase: TranscriptWaitingPhrase? { waiting.phrase }

    private func rescheduleWaitingPhrase(_ isWaiting: Bool) {
        waiting.update(isWaiting: isWaiting)
    }

    private func loadEarlierHistory() {
        guard model.canLoadEarlierHistory else { return }
        historyAnchorID = projection.rows.first?.id
        historyBoundaryID = model.displayedTranscript.first?.presentationID
        scrollMode = .restoringHistory
        model.loadEarlierHistory()
    }

    private func restoreHistoryAnchor() {
        guard scrollMode == .restoringHistory else { return }
        let anchorRow = projection.rows.first { row in
            row.id == historyAnchorID
                || row.records.contains { $0.presentationID == historyBoundaryID }
        }
        if let anchorRow {
            position.scrollTo(id: anchorRow.id, anchor: .top)
        }
        scrollMode = .freeScrolling
    }

    private static func atBottom(_ geometry: ScrollGeometry) -> Bool {
        // The visible rect covers the toolbar inset that `containerSize` leaves out, so it is
        // the only measure that reaches the content height at rest.
        geometry.visibleRect.maxY >= geometry.contentSize.height - 24
    }

    private var emptyState: some View {
        MobiusComposingOrb()
            .frame(width: transcriptOrbSize, height: transcriptOrbSize)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .padding(.bottom, bottomInset)
            .accessibilityHidden(true)
    }
}

private struct TranscriptLoadingView: View {
    let bottomInset: CGFloat

    var body: some View {
        ZStack {
            MobiusBackdrop()
            MobiusComposingOrb()
                .frame(width: transcriptOrbSize, height: transcriptOrbSize)
                .offset(y: -bottomInset / 2)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Loading conversation")
    }
}

private struct TranscriptRow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var showsCopyConfirmation = false
    let entry: TranscriptEntry
    /// Activity never reaches this view: a run is a group row, whatever its length. The
    /// projection sends only what the reader wrote and what the agent said back.
    let isUser: Bool
    let isPeer: Bool
    let speaker: MessageSpeaker
    let fileSessionID: String?
    let turnDiff: String

    var body: some View {
        VStack(alignment: isInput ? .trailing : .leading, spacing: 0) {
            content
            if !turnDiff.isEmpty {
                TurnDiffCard(source: turnDiff)
                    .padding(.top, MobiusSpace.m)
            }
            // The reader's own message carries its actions in the context menu; the agent's
            // final answer carries them under the text, where they are always available.
            if entry.kind == .assistant { controls }
        }
        .frame(maxWidth: .infinity, alignment: isInput ? .trailing : .leading)
    }

    private var isInput: Bool { isUser || isPeer }

    @ViewBuilder
    private var content: some View {
        if isUser {
            HStack {
                Spacer(minLength: 42)
                VStack(alignment: .trailing, spacing: MobiusSpace.s) {
                    TranscriptFileCards(
                        files: entry.files,
                        sessionID: fileSessionID,
                        alignsTrailing: true
                    )
                    if !entry.text.isEmpty {
                        CollapsibleText(text: entry.text)
                            .padding(.horizontal, MobiusSpace.l)
                            .padding(.vertical, MobiusSpace.m)
                            .background(palette.accentSoft, in: MobiusStyle.cardShape)
                            .contentShape(MobiusStyle.cardShape)
                            .contextMenu { transcriptActions }
                    }
                    messageMetadata
                }
            }
        } else if isPeer {
            HStack {
                Spacer(minLength: 42)
                VStack(alignment: .trailing, spacing: MobiusSpace.s) {
                    TranscriptFileCards(
                        files: entry.files,
                        sessionID: fileSessionID,
                        alignsTrailing: true
                    )
                    if !entry.text.isEmpty {
                        MobiusMarkdownText(entry.text, streaming: false)
                            .equatable()
                            .multilineTextAlignment(.leading)
                            .padding(MobiusSpace.l)
                            .background(
                                palette.accentSoft.opacity(0.45),
                                in: MobiusStyle.cardShape
                            )
                            .overlay {
                                MobiusStyle.cardShape.stroke(
                                    palette.accent.opacity(0.3),
                                    lineWidth: MobiusStyle.borderWidth
                                )
                            }
                    }
                    if peerApprovalSource != nil {
                        Button("Open approval", glyph: .arrowUpRight01, action: openPeerApproval)
                            .buttonStyle(.mobiusPlain)
                            .font(MobiusStyle.metadataFont)
                            .foregroundStyle(palette.accent)
                            .frame(minHeight: MobiusStyle.iconButtonSize)
                            .disabled(!model.canOpenSession)
                            .accessibilityHint("Opens the Bot conversation awaiting approval")
                    }
                    messageMetadata
                }
            }
        } else {
            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                TranscriptFileCards(files: entry.files, sessionID: fileSessionID)
                if !entry.text.isEmpty {
                    MobiusMarkdownText(entry.text, streaming: entry.pending)
                        .equatable()
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    @ViewBuilder
    private var messageMetadata: some View {
        if let metadata = entry.messageMetadata {
            MessageMetadata(author: metadata.author, delivery: metadata.delivery)
        }
    }

    private var peerApprovalSource: (botID: String, sessionID: String)? {
        guard isSwarmAttentionMessage(entry.text),
              let peer = entry.messageMetadata?.author.peerFields,
              let bot = model.bots.first(where: { $0.handle == peer.handle })
        else { return nil }
        return (bot.id, peer.sessionID)
    }

    private func openPeerApproval() {
        guard let source = peerApprovalSource else { return }
        model.resumeBotSession(botID: source.botID, sessionID: source.sessionID)
    }

    private var controls: some View {
        HStack(spacing: 0) {
            MessageActionButton(
                title: showsCopyConfirmation ? "Copied" : "Copy",
                glyph: showsCopyConfirmation ? .check : .copy
            ) {
                copyToPasteboard(entry.text)
                showsCopyConfirmation = true
            }
            .task(id: showsCopyConfirmation) {
                guard showsCopyConfirmation else { return }
                try? await Task.sleep(for: .seconds(1.5))
                guard !Task.isCancelled else { return }
                showsCopyConfirmation = false
            }
            if let target = entry.messageTarget {
                ForEach(model.messageActionWidgets) { widget in
                    MessageActionButton(
                        verbatim: widget.widget.text,
                        glyph: messageActionGlyph(widget)
                    ) {
                        model.submitMessageAction(widget, target: target)
                    }
                    .disabled(!model.canModifySelectedSession)
                }
            }
            if !entry.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                MessageActionButton(title: "Speak", glyph: .volumeHigh) {
                    speaker.speak(entry.text)
                }
            }
            if !entry.pending, let bot = model.bot(forSessionID: fileSessionID) {
                HStack(spacing: MobiusSpace.xs) {
                    Text(verbatim: "·")
                        .accessibilityHidden(true)
                    MobiusIcon(
                        .aiScan,
                        size: MobiusStyle.glyphMark,
                        foreground: bot.tint.color,
                        gutter: false
                    )
                    .accessibilityHidden(true)
                    Text(verbatim: bot.name)
                }
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
                .padding(.horizontal, MobiusSpace.xs)
            }
        }
    }

    @ViewBuilder
    private var transcriptActions: some View {
        Button("Copy", glyph: .copy) { copyToPasteboard(entry.text) }
        if let target = entry.messageTarget {
            ForEach(model.messageActionWidgets) { widget in
                Button(verbatim: widget.widget.text, glyph: messageActionGlyph(widget)) {
                    model.submitMessageAction(widget, target: target)
                }
                .disabled(!model.canModifySelectedSession)
            }
        }
    }

    private func messageActionGlyph(_ widget: MountedWidget) -> MobiusGlyph {
        widget.widget.symbol.map { MobiusSymbol.glyph(for: $0) } ?? .dotsThree
    }
}

private struct MessageMetadata: View {
    @Environment(\.mobiusPalette) private var palette
    let author: MessageAuthor
    let delivery: MessageDelivery

    var body: some View {
        HStack(spacing: MobiusSpace.xs) {
            MobiusIcon(
                glyph,
                size: MobiusStyle.glyphMark,
                foreground: palette.muted,
                gutter: false
            )
            Text(verbatim: "·")
            if let deliveryLabel {
                Text(deliveryLabel)
                Text(verbatim: "·")
            }
            authorLabel
        }
        .font(MobiusStyle.metadataFont)
        .foregroundStyle(palette.muted)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(accessibilityLabel)
    }

    private var glyph: MobiusGlyph {
        switch delivery {
        case .steer: .workflowSquare03
        case .queue: .queue01
        case .turn: author == .user ? .userFocus : .aiScan
        }
    }

    private var deliveryLabel: LocalizedStringResource? {
        switch delivery {
        case .steer: "steer"
        case .queue: "queued"
        case .turn: nil
        }
    }

    private var authorLabel: Text {
        author.peerFields.map { Text(verbatim: $0.handle) } ?? Text("you")
    }

    private var accessibilityLabel: Text {
        let author = author.peerFields?.handle ?? String(localized: "you")
        guard let deliveryLabel else { return Text(verbatim: author) }
        return Text(verbatim: "\(String(localized: deliveryLabel)), \(author)")
    }
}
