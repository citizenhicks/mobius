import Foundation
import SwiftUI

/// The transcript body shared by the full chat and read-only agent previews.
/// Navigation, pagination, and composing controls stay with their owning surface.
///
/// The projection decides what a row is, what it is called, how it is sized and who holds the
/// waiting line. This view only draws it.
struct TranscriptRowsView: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var visibleRowIDs = Set<TranscriptPresentationID>()
    @State private var hasAppeared = false
    @Environment(AppModel.self) private var model
    private var speaker: MessageSpeaker { model.messageSpeaker }
    let projection: TranscriptProjection
    let fileSessionID: String?
    var activeStepID: TranscriptPresentationID?
    var rowSpacing: CGFloat = 12
    var allowsMessageActions = false
    var revealMessageTarget: MessageTarget?
    var turnDiff: (TranscriptEntry) -> String = { _ in "" }
    var onExpandActivityGroup: () -> Void = {}
    var onRevealMessage: (MessageTarget, TranscriptPresentationID) -> Void = { _, _ in }

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
                allowsMessageActions: allowsMessageActions,
                revealMessageTarget: revealMessageTarget,
                onExpand: onExpandActivityGroup,
                onRevealMessage: onRevealMessage
            )
        case .user, .peer, .narrative:
            if let entry = row.records.first {
                TranscriptRow(
                    entry: entry,
                    isUser: row.kind == .user,
                    isPeer: row.kind == .peer,
                    fileSessionID: fileSessionID,
                    allowsMessageActions: allowsMessageActions,
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
private enum TranscriptScrollMode {
    /// Parked at the end. The bottom anchor follows content growth; nothing else scrolls.
    case followingTail
    /// The reader is somewhere else. Nothing follows, and structure lands without animation.
    case freeScrolling
    /// A page of history was prepended. The previously visible turn is restored, not the tail.
    case restoringHistory

    var followsContentGrowth: Bool { self == .followingTail }
}

/// The scroll state shared by every full transcript surface.
struct TranscriptScrollState {
    fileprivate var position = ScrollPosition(edge: .bottom)
    fileprivate var historyAnchorID: TranscriptPresentationID?
    fileprivate var mode = TranscriptScrollMode.followingTail
    fileprivate(set) var historyBoundaryID: TranscriptPresentationID?

    mutating func beginHistoryRestore(
        projection: TranscriptProjection,
        boundaryID: TranscriptPresentationID?
    ) {
        historyAnchorID = projection.rows.first?.id
        historyBoundaryID = boundaryID
        mode = .restoringHistory
    }

    mutating func stopFollowingTail() {
        mode = .freeScrolling
    }

    fileprivate mutating func restoreHistoryAnchor(in projection: TranscriptProjection) {
        guard mode == .restoringHistory else { return }
        let anchorRow = projection.rows.first { row in
            row.id == historyAnchorID
                || row.records.contains { $0.presentationID == historyBoundaryID }
        }
        if let anchorRow {
            position.scrollTo(id: anchorRow.id, anchor: .top)
        }
        mode = .freeScrolling
    }

    fileprivate mutating func reset() {
        position = ScrollPosition(edge: .bottom)
        historyAnchorID = nil
        historyBoundaryID = nil
        mode = .followingTail
    }
}

private struct TranscriptScrollBehavior: ViewModifier {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Binding var scroll: TranscriptScrollState
    let projection: TranscriptProjection
    let historyLoadCompletionRevision: Int
    let conversationID: String?
    let scrollToBottomRequest: Int
    let isAtBottom: Binding<Bool>?
    let loadEarlierHistory: () -> Void

    func body(content: Content) -> some View {
        content
            // The keyboard insets this scroll view, so the backdrop must belong to the
            // transcript rather than ending at the keyboard's top edge.
            .background(MobiusBackdrop())
            .scrollPosition($scroll.position)
            .defaultScrollAnchor(.bottom, for: .initialOffset)
            .defaultScrollAnchor(
                scroll.mode.followsContentGrowth ? .bottom : nil,
                for: .sizeChanges
            )
            .animation(
                reduceMotion || !scroll.mode.followsContentGrowth
                    ? nil
                    : .easeOut(duration: 0.5),
                value: projection.structuralRevision
            )
            .scrollIndicators(.hidden)
            .scrollDismissesKeyboard(.interactively)
            .refreshable { loadEarlierHistory() }
            .onAppear {
                scroll.mode = .followingTail
                scroll.position.scrollTo(edge: .bottom)
            }
            .onScrollGeometryChange(for: Bool.self) { Self.atBottom($0) } action: {
                _, atBottom in
                isAtBottom?.wrappedValue = atBottom
            }
            .onScrollPhaseChange { _, phase, context in
                guard scroll.mode != .restoringHistory, phase != .animating else { return }
                scroll.mode = phase == .idle && Self.atBottom(context.geometry)
                    ? .followingTail
                    : .freeScrolling
            }
            .onChange(of: scrollToBottomRequest) {
                withAnimation(.easeOut(duration: 0.2)) {
                    scroll.position.scrollTo(edge: .bottom)
                }
            }
            .onChange(of: historyLoadCompletionRevision) {
                scroll.restoreHistoryAnchor(in: projection)
            }
            .onChange(of: conversationID) { scroll.reset() }
    }

    private static func atBottom(_ geometry: ScrollGeometry) -> Bool {
        // The visible rect includes the bottom inset and is the only measure that reaches
        // the furthest available offset at rest.
        geometry.visibleRect.maxY >= geometry.contentSize.height - 24
    }
}

extension View {
    func transcriptScrollBehavior(
        _ scroll: Binding<TranscriptScrollState>,
        projection: TranscriptProjection,
        historyLoadCompletionRevision: Int,
        conversationID: String?,
        scrollToBottomRequest: Int = 0,
        isAtBottom: Binding<Bool>? = nil,
        loadEarlierHistory: @escaping () -> Void
    ) -> some View {
        modifier(TranscriptScrollBehavior(
            scroll: scroll,
            projection: projection,
            historyLoadCompletionRevision: historyLoadCompletionRevision,
            conversationID: conversationID,
            scrollToBottomRequest: scrollToBottomRequest,
            isAtBottom: isAtBottom,
            loadEarlierHistory: loadEarlierHistory
        ))
    }
}

struct TranscriptView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let bottomInset: CGFloat
    @Binding var isAtBottom: Bool
    let scrollToBottomRequest: Int
    // A restored transcript can land after the scroll view exists. The bottom-edge position
    // survives that late fill, while ChatView supplies a fresh identity for each presentation.
    @State private var scroll = TranscriptScrollState()
    @State private var waiting = TranscriptWaitingHold()
    @State private var pendingMessageTarget: MessageTarget?
    @State private var groupedMessageTarget: MessageTarget?
    @State private var messageNavigationProgress: MessageNavigationProgress?
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
        ScrollViewReader { proxy in
            transcript(proxy: proxy)
        }
    }

    private func transcript(proxy: ScrollViewProxy) -> some View {
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
                    allowsMessageActions: true,
                    revealMessageTarget: groupedMessageTarget,
                    turnDiff: { model.turnDiff(for: $0) },
                    onExpandActivityGroup: { scroll.stopFollowingTail() },
                    onRevealMessage: { target, rowID in
                        revealGroupedMessage(target, rowID: rowID, proxy: proxy)
                    }
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
        .transcriptScrollBehavior(
            $scroll,
            projection: projection,
            historyLoadCompletionRevision: model.historyLoadCompletionRevision,
            conversationID: model.selectedSessionID,
            scrollToBottomRequest: scrollToBottomRequest,
            isAtBottom: $isAtBottom,
            loadEarlierHistory: loadEarlierHistory
        )
        .overlay {
            if model.displayedTranscript.isEmpty {
                emptyState
            }
        }
        .onChange(of: model.isWaitingForModel, initial: true) { _, isWaiting in
            rescheduleWaitingPhrase(isWaiting)
        }
        .onChange(of: model.messageNavigationRequest) { _, request in
            pendingMessageTarget = request?.target
            groupedMessageTarget = nil
            messageNavigationProgress = nil
            seekMessageTarget(proxy: proxy)
        }
        .onChange(of: model.historyLoadSuccessRevision) { _, _ in
            seekMessageTarget(proxy: proxy)
        }
        .onChange(of: model.historyLoadFailureRevision) { _, _ in
            pendingMessageTarget = nil
            groupedMessageTarget = nil
            messageNavigationProgress = nil
        }
        .onChange(of: model.selectedSessionID) { _, _ in
            pendingMessageTarget = nil
            groupedMessageTarget = nil
            messageNavigationProgress = nil
        }
        .onDisappear { rescheduleWaitingPhrase(false) }
    }

    private var projection: TranscriptProjection {
        model.transcriptProjection(
            breakBefore: scroll.historyBoundaryID,
            waitingPhrase: waitingPhrase
        )
    }

    private var waitingPhrase: TranscriptWaitingPhrase? { waiting.phrase }

    private func rescheduleWaitingPhrase(_ isWaiting: Bool) {
        waiting.update(isWaiting: isWaiting)
    }

    private func loadEarlierHistory() {
        guard model.canLoadEarlierHistory else { return }
        scroll.beginHistoryRestore(
            projection: projection,
            boundaryID: model.displayedTranscript.first?.presentationID
        )
        model.loadEarlierHistory()
    }

    private func seekMessageTarget(proxy: ScrollViewProxy) {
        guard let target = pendingMessageTarget else { return }
        if let row = projection.rows.first(where: { row in
            row.records.contains { $0.messageTarget == target }
        }) {
            if row.kind == .workedGroup {
                groupedMessageTarget = target
                scroll.stopFollowingTail()
                return
            }
            scroll.stopFollowingTail()
            withAnimation(reduceMotion ? nil : .easeOut(duration: 0.2)) {
                proxy.scrollTo(row.id, anchor: .center)
            }
            pendingMessageTarget = nil
            groupedMessageTarget = nil
            messageNavigationProgress = nil
            return
        }
        guard !model.isLoadingEarlierHistory else { return }
        let progress = MessageNavigationProgress(
            firstID: model.displayedTranscript.first?.id,
            visibleCount: model.displayedTranscript.count,
            beforeSequence: model.nextHistoryBeforeSequence
        )
        guard progress != messageNavigationProgress, model.canLoadEarlierHistory else {
            pendingMessageTarget = nil
            groupedMessageTarget = nil
            messageNavigationProgress = nil
            model.showToast("Original message is unavailable.", tone: .warning)
            return
        }
        messageNavigationProgress = progress
        scroll.stopFollowingTail()
        model.loadEarlierHistory()
    }

    private func revealGroupedMessage(
        _ target: MessageTarget,
        rowID: TranscriptPresentationID,
        proxy: ScrollViewProxy
    ) {
        guard pendingMessageTarget == target else { return }
        scroll.stopFollowingTail()
        withAnimation(reduceMotion ? nil : .easeOut(duration: 0.2)) {
            proxy.scrollTo(rowID, anchor: .center)
        }
        pendingMessageTarget = nil
        groupedMessageTarget = nil
        messageNavigationProgress = nil
    }

    private var emptyState: some View {
        MobiusComposingOrb()
            .frame(width: MobiusStyle.transcriptOrbSize, height: MobiusStyle.transcriptOrbSize)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .padding(.bottom, bottomInset)
            .accessibilityHidden(true)
    }
}

private struct MessageNavigationProgress: Equatable {
    let firstID: String?
    let visibleCount: Int
    let beforeSequence: UInt64?
}

private struct TranscriptLoadingView: View {
    let bottomInset: CGFloat

    var body: some View {
        ZStack {
            MobiusBackdrop()
            MobiusComposingOrb()
                .frame(width: MobiusStyle.transcriptOrbSize, height: MobiusStyle.transcriptOrbSize)
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
    let fileSessionID: String?
    let allowsMessageActions: Bool
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
        if isInput {
            HStack {
                Spacer(minLength: 42)
                VStack(alignment: .trailing, spacing: MobiusSpace.s) {
                    if let reply = entry.reply {
                        ReplyQuoteView(
                            reply: reply,
                            open: allowsMessageActions
                                ? { model.openMessageReply(reply) }
                                : nil
                        )
                    }
                    TranscriptFileCards(
                        files: entry.files,
                        sessionID: fileSessionID,
                        alignsTrailing: true
                    )
                    if !entry.text.isEmpty {
                        if isUser {
                            CollapsibleText(text: entry.text)
                                .padding(.horizontal, MobiusSpace.l)
                                .padding(.vertical, MobiusSpace.m)
                                .background(palette.accentSoft, in: MobiusStyle.cardShape)
                                .contentShape(MobiusStyle.cardShape)
                        } else {
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
                    }
                    messageMetadata
                }
                .contentShape(Rectangle())
                .contextMenu { inputActions }
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
            MessageMetadata(
                author: metadata.author,
                delivery: metadata.delivery,
                bot: isPeer ? displayedBot : nil
            )
        }
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
            if allowsMessageActions, let target = entry.messageTarget {
                ForEach(model.messageActionWidgets) { widget in
                    MessageActionButton(
                        verbatim: widget.widget.text,
                        glyph: messageActionGlyph(widget)
                    ) {
                        model.submitMessageAction(widget, target: target)
                    }
                    .disabled(!model.canModifySelectedSession)
                }
                MessageActionButton(title: "Reply", glyph: .re) {
                    model.beginReplying(to: entry)
                }
                .disabled(!model.canBeginReply)
            }
            if !entry.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                MessageActionButton(title: "Speak", glyph: .volumeHigh) {
                    model.speakMessage(entry.text)
                }
            }
            if !entry.pending, let bot = displayedBot {
                Group {
                    Text(verbatim: "•")
                        .accessibilityHidden(true)
                        .padding(.horizontal, MobiusSpace.xs)
                    MobiusIcon(
                        .aiScan,
                        size: MobiusStyle.glyphMark,
                        foreground: bot.tint.color,
                        gutter: false
                    )
                    .accessibilityHidden(true)
                    .padding(.trailing, MobiusSpace.xs)
                    Text(verbatim: bot.name)
                }
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
            }
        }
        .contextMenu { assistantActions }
    }

    private var displayedBot: BotRecord? {
        if let handle = entry.messageMetadata?.author.peerFields?.handle {
            return model.bots.first { $0.handle == handle }
        }
        return model.bot(forSessionID: fileSessionID)
    }

    @ViewBuilder
    private var inputActions: some View {
        if !entry.text.isEmpty {
            Button("Copy", glyph: .copy) { copyToPasteboard(entry.text) }
        }
        if allowsMessageActions, let target = entry.messageTarget {
            ForEach(model.messageActionWidgets) { widget in
                Button(verbatim: widget.widget.text, glyph: messageActionGlyph(widget)) {
                    model.submitMessageAction(widget, target: target)
                }
                .disabled(!model.canModifySelectedSession)
            }
            Button("Reply", glyph: .re) { model.beginReplying(to: entry) }
                .disabled(!model.canBeginReply)
        }
        informationMenuItems
    }

    @ViewBuilder
    private var assistantActions: some View {
        Button("Copy", glyph: .copy) { copyToPasteboard(entry.text) }
        if allowsMessageActions, let target = entry.messageTarget {
            ForEach(model.messageActionWidgets) { widget in
                Button(verbatim: widget.widget.text, glyph: messageActionGlyph(widget)) {
                    model.submitMessageAction(widget, target: target)
                }
                .disabled(!model.canModifySelectedSession)
            }
        }
        if !entry.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            Button("Speak", glyph: .volumeHigh) { model.speakMessage(entry.text) }
        }
        if allowsMessageActions, entry.messageTarget != nil {
            Button("Reply", glyph: .re) { model.beginReplying(to: entry) }
                .disabled(!model.canBeginReply)
        }
        informationMenuItems
    }

    @ViewBuilder
    private var informationMenuItems: some View {
        if timestamp != nil || isInput || displayedBot != nil {
            Divider()
        }
        if let timestamp {
            Button(verbatim: timestamp.combined, glyph: .clock) {}
                .disabled(true)
        }
        if isUser {
            Button("you", glyph: .userFocus) {}
                .disabled(true)
        } else if let bot = displayedBot {
            if let image = MobiusGlyph.aiScan.menuImage(bot.tint.color) {
                Button(action: {}) {
                    Label { Text(verbatim: bot.name) } icon: { image }
                }
                .disabled(true)
            } else {
                Button(verbatim: bot.name, glyph: .aiScan) {}
                    .disabled(true)
            }
        } else if isPeer, let handle = entry.messageMetadata?.author.peerFields?.handle {
            Button(verbatim: handle, glyph: .aiScan) {}
                .disabled(true)
        }
    }

    private var timestamp: MessageTimestamp? {
        entry.recordedAtMs.flatMap { MessageTimestamp(milliseconds: $0) }
    }

    private func messageActionGlyph(_ widget: MountedWidget) -> MobiusGlyph {
        widget.widget.symbol.map { MobiusSymbol.glyph(for: $0) } ?? .dotsThree
    }
}

private struct MessageMetadata: View {
    @Environment(\.mobiusPalette) private var palette
    let author: MessageAuthor
    let delivery: MessageDelivery
    let bot: BotRecord?

    var body: some View {
        HStack(spacing: MobiusSpace.xs) {
            MobiusIcon(
                glyph,
                size: MobiusStyle.glyphMark,
                foreground: bot?.tint.color ?? palette.muted,
                gutter: false
            )
            if let deliveryLabel {
                Text(deliveryLabel)
                Text(verbatim: "•")
                    .accessibilityHidden(true)
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
        bot.map { Text(verbatim: $0.name) }
            ?? author.peerFields.map { Text(verbatim: $0.handle) }
            ?? Text("you")
    }

    private var accessibilityLabel: Text {
        let author = bot?.name ?? author.peerFields?.handle ?? String(localized: "you")
        let delivery = deliveryLabel.map { String(localized: $0) }
        return Text(verbatim: [delivery, author].compactMap { $0 }.joined(separator: ", "))
    }
}

private struct MessageTimestamp {
    let combined: String

    init?(milliseconds: Int64) {
        guard milliseconds > 0 else { return nil }
        let value = Date(timeIntervalSince1970: TimeInterval(milliseconds) / 1_000)
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = .autoupdatingCurrent
        let components = calendar.dateComponents(
            [.year, .month, .day, .hour, .minute],
            from: value
        )
        guard let year = components.year,
              let month = components.month,
              let day = components.day,
              let hour = components.hour,
              let minute = components.minute
        else { return nil }
        combined = String(
            format: "%02d:%02d • %02d/%02d/%02d",
            hour,
            minute,
            day,
            month,
            year % 100
        )
    }
}
