import Foundation
import SwiftUI
import UIKit
@preconcurrency import AVFoundation

struct ComposerView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(spacing: MobiusSpace.s) {
            ForEach(model.composerHeaderWidgets) { widget in
                FrontendWidgetView(widget: widget)
            }
            if let approval = model.pendingApproval {
                ApprovalView(approval: approval)
            }
            if let picker = model.pendingPicker {
                FrontendPickerView(picker: picker)
            }
            ComposerStack()
        }
        .frame(maxWidth: MobiusStyle.transcriptWidth)
        .frame(maxWidth: .infinity)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.bottom, MobiusSpace.m)
    }
}

private struct ComposerStack: View {
    var body: some View {
        VStack(spacing: MobiusSpace.xs) {
            ComposerActivityView()
            ComposerSurface()
        }
    }
}

private struct ComposerSurface: View {
    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    @State private var dictation = ComposerDictation()
    @State private var selection: TextSelection?
    @FocusState private var isComposerFocused: Bool
    @State private var referenceSuggestions: ReferenceSuggestions?
    @State private var composerHeight: CGFloat = 0
    @State private var showsExpandedComposer = false

    var body: some View {
        @Bindable var model = model
        VStack(spacing: 0) {
            if !showsExpandedComposer {
                if !model.composerAttachments.isEmpty {
                    ComposerAttachmentsView()
                        .padding(.horizontal, MobiusSpace.m)
                        .padding(.top, MobiusSpace.m)
                }
                TextField(
                    "You can just do things",
                    text: $model.composer,
                    selection: $selection,
                    axis: .vertical
                )
                .textFieldStyle(.plain)
                .focused($isComposerFocused)
                .lineLimit(1...8)
                .scrollDismissesKeyboard(.interactively)
                .font(MobiusStyle.bodyFont)
                .accessibilityLabel("Message")
                .disabled(dictation.isActive)
                .onSubmit { _ = submit() }
                .onKeyPress(.return, phases: .down) { keyPress in
                    if keyPress.modifiers.contains(.shift) {
                        insertLineBreak()
                    } else {
                        _ = submit()
                    }
                    return .handled
                }
                .onGeometryChange(for: CGFloat.self) { geometry in
                    geometry.size.height
                } action: { height in
                    composerHeight = height
                }
                .padding(.horizontal, MobiusSpace.l)
                .padding(.top, MobiusSpace.m)
                .padding(.bottom, MobiusSpace.xs)
                .padding(.trailing, showsExpansionControl ? MobiusStyle.iconButtonSize : 0)
                .overlay(alignment: .topTrailing) {
                    if showsExpansionControl {
                        ComposerSizeButton(expanded: false) {
                            showsExpandedComposer = true
                        }
                    }
                }
                ComposerOptionsView(
                    dictation: dictation,
                    selection: $selection,
                    send: { _ = submit() }
                )
                .padding(.horizontal, MobiusStyle.iconRowPadding)
                .padding(.bottom, MobiusStyle.iconRowPadding)
            }
        }
        .mobiusGlass(in: MobiusStyle.cardShape, interactive: true)
        .shadow(color: .black.opacity(0.18), radius: 12, y: 6)
        .overlay(alignment: .top) {
            if !showsExpandedComposer, let suggestions = referenceSuggestions {
                ReferenceSuggestionsPopup(suggestions: suggestions) {
                    complete($0, suggestions: suggestions)
                }
                .padding(.horizontal, MobiusSpace.s)
                .zIndex(2)
            }
        }
        .sheet(isPresented: $showsExpandedComposer) {
            ExpandedComposerSheet(
                dictation: dictation,
                selection: $selection,
                suggestions: referenceSuggestions,
                completeReference: complete,
                submit: submit,
                insertLineBreak: insertLineBreak
            )
        }
        .task(id: referenceSuggestionRequest) {
            let request = referenceSuggestionRequest
            referenceSuggestions = nil
            guard !request.isDisabled else { return }
            try? await Task.sleep(for: .milliseconds(80))
            guard !Task.isCancelled else { return }
            let references = model.capabilityReferences
            let files = model.workspaceFiles
            let searchTask = Task.detached(priority: .userInitiated) {
                AppModel.referenceSuggestions(
                    in: request.text,
                    cursorOffset: request.cursorOffset,
                    capabilityReferences: references,
                    workspaceFiles: files
                )
            }
            let result = await searchTask.value
            guard !Task.isCancelled else { return }
            referenceSuggestions = result
        }
        .onChange(of: model.composerFocusRequest) { _, _ in
            isComposerFocused = true
        }
        .onChange(of: model.composerBlurRequest) { _, _ in
            isComposerFocused = false
        }
        .onChange(of: scenePhase) { _, phase in
            guard phase == .background else { return }
            Task { await dictation.cancel() }
        }
        .onChange(of: model.selectedSessionID) { _, _ in
            Task { await dictation.cancel() }
        }
        .onChange(of: model.connectionState.isReady) { _, isReady in
            guard !isReady else { return }
            Task { await dictation.cancel() }
        }
        .onReceive(
            NotificationCenter.default.publisher(for: AVAudioSession.interruptionNotification)
        ) { notification in
            guard let rawValue = notification.userInfo?[AVAudioSessionInterruptionTypeKey]
                as? UInt,
                  AVAudioSession.InterruptionType(rawValue: rawValue) == .began
            else { return }
            Task { await dictation.cancel() }
        }
        .onDisappear {
            Task { await dictation.cancel() }
        }
    }

    private var showsExpansionControl: Bool {
        composerHeight > UIFont.preferredFont(forTextStyle: .body).lineHeight * 2.5
    }

    private func submit() -> Bool {
        guard !dictation.isActive, model.sendMessage() else { return false }
        selection = nil
        return true
    }

    private var referenceSuggestionRequest: ReferenceSuggestionRequest {
        let isDisabled = dictation.isActive
        let text = model.composer
        let cursor: String.Index
        if let selection,
           case .selection(let range) = selection.indices,
           range.isEmpty,
           text.indices.contains(range.lowerBound) || range.lowerBound == text.endIndex
        {
            cursor = range.lowerBound
        } else {
            cursor = text.endIndex
        }
        return ReferenceSuggestionRequest(
            text: text,
            cursorOffset: text.distance(from: text.startIndex, to: cursor),
            capabilityRevision: model.contributionsRevision,
            workspaceFileRevision: model.workspaceFilesRevision,
            isDisabled: isDisabled
        )
    }

    private func complete(_ mounted: MountedReference, suggestions: ReferenceSuggestions) {
        guard model.composer == suggestions.source else { return }
        var text = suggestions.source
        let offset = text.distance(from: text.startIndex, to: suggestions.range.lowerBound)
        text.replaceSubrange(suggestions.range, with: mounted.replacement)
        model.composer = text
        selection = TextSelection(insertionPoint: text.index(
            text.startIndex,
            offsetBy: offset + mounted.replacement.count
        ))
    }

    private func insertLineBreak() {
        var text = model.composer
        let range: Range<String.Index>
        if let selection, case .selection(let selectedRange) = selection.indices {
            range = selectedRange
        } else {
            range = text.endIndex..<text.endIndex
        }
        let offset = text.distance(from: text.startIndex, to: range.lowerBound)
        text.replaceSubrange(range, with: "\n")
        model.composer = text
        self.selection = TextSelection(
            insertionPoint: text.index(text.startIndex, offsetBy: offset + 1)
        )
    }
}

private struct ExpandedComposerSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let dictation: ComposerDictation
    @Binding var selection: TextSelection?
    let suggestions: ReferenceSuggestions?
    let completeReference: (MountedReference, ReferenceSuggestions) -> Void
    let submit: () -> Bool
    let insertLineBreak: () -> Void
    @FocusState private var isComposerFocused: Bool

    var body: some View {
        @Bindable var model = model
        VStack(spacing: 0) {
            if !model.composerAttachments.isEmpty {
                ComposerAttachmentsView()
                    .padding(.horizontal, MobiusSpace.m)
                    .padding(.top, MobiusSpace.m)
            }
            TextEditor(text: $model.composer, selection: $selection)
                .scrollContentBackground(.hidden)
                .scrollDismissesKeyboard(.interactively)
                .focused($isComposerFocused)
                .font(MobiusStyle.bodyFont)
                .accessibilityLabel("Message")
                .disabled(dictation.isActive)
                .onKeyPress(.return, phases: .down) { keyPress in
                    if keyPress.modifiers.contains(.shift) {
                        insertLineBreak()
                    } else {
                        submitAndDismiss()
                    }
                    return .handled
                }
                .padding(.horizontal, MobiusSpace.l)
                .padding(.top, MobiusSpace.m)
                .padding(.bottom, MobiusSpace.xs)
                .padding(.trailing, MobiusStyle.iconButtonSize)
                .overlay(alignment: .topTrailing) {
                    ComposerSizeButton(expanded: true, action: dismiss.callAsFunction)
                }
                .frame(maxHeight: .infinity)
            if let suggestions {
                ReferenceSuggestionsPopup(suggestions: suggestions, floatsAbove: false) {
                    completeReference($0, suggestions)
                }
                .padding(.horizontal, MobiusSpace.s)
            }
            ComposerOptionsView(
                dictation: dictation,
                selection: $selection,
                send: submitAndDismiss
            )
                .padding(.horizontal, MobiusStyle.iconRowPadding)
                .padding(.bottom, MobiusStyle.iconRowPadding)
        }
        .frame(maxWidth: MobiusStyle.transcriptWidth, maxHeight: .infinity)
        .mobiusGlass(in: MobiusStyle.cardShape, interactive: true)
        .shadow(color: .black.opacity(0.18), radius: 12, y: 6)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.top, MobiusSpace.xl)
        .padding(.bottom, MobiusSpace.m)
        .defaultFocus($isComposerFocused, true)
        .onChange(of: model.composerFocusRequest) { _, _ in
            isComposerFocused = true
        }
        .onChange(of: model.composerBlurRequest) { _, _ in
            isComposerFocused = false
        }
        .mobiusSheet(detents: [.fraction(0.75)])
    }

    private func submitAndDismiss() {
        if submit() { dismiss() }
    }
}

private struct ComposerSizeButton: View {
    let expanded: Bool
    let action: () -> Void

    var body: some View {
        let title = expanded ? "Collapse composer" : "Expand composer"
        Button(action: action) {
            MobiusLabel(
                title: title,
                glyph: expanded ? .collapse : .expand,
                iconSize: MobiusStyle.glyphMark
            )
        }
        .labelStyle(.iconOnly)
        .buttonStyle(MobiusIconButtonStyle(bare: true))
        .help(title)
    }
}

private struct ReferenceSuggestionRequest: Equatable, Sendable {
    let text: String
    let cursorOffset: Int
    let capabilityRevision: Int
    let workspaceFileRevision: Int
    let isDisabled: Bool
}

private struct ReferenceSuggestionsPopup: View {
    @Environment(\.mobiusPalette) private var palette
    let suggestions: ReferenceSuggestions
    var floatsAbove = true
    let select: (MountedReference) -> Void

    private var height: CGFloat {
        min(CGFloat(suggestions.matches.count) * 48 + 12, 252)
    }

    var body: some View {
        ScrollView {
            VStack(spacing: 0) {
                ForEach(suggestions.matches) { mounted in
                    Button { select(mounted) } label: {
                        HStack(spacing: MobiusSpace.m) {
                            Text(String(mounted.reference.trigger))
                                .font(MobiusStyle.controlFont.monospaced().weight(.semibold))
                                .foregroundStyle(palette.accent)
                                .frame(width: 18, alignment: .center)
                            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                                Text(mounted.reference.value)
                                    .font(MobiusStyle.controlFont)
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                                Text(mounted.reference.description)
                                    .font(MobiusStyle.metadataFont)
                                    .foregroundStyle(palette.muted)
                                    .lineLimit(1)
                            }
                            Spacer(minLength: 0)
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, MobiusSpace.m)
                        .frame(height: 48)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.mobiusPlain)
                    .help(mounted.reference.description)
                    .accessibilityLabel(mounted.label)
                    .accessibilityHint(mounted.reference.description)
                }
            }
            .padding(.vertical, MobiusSpace.s)
        }
        .scrollIndicators(.hidden)
        .frame(height: height)
        .background(palette.panel, in: MobiusStyle.tileShape)
        .mobiusGlass(in: MobiusStyle.tileShape)
        .shadow(color: .black.opacity(0.2), radius: 16, y: 8)
        .offset(y: floatsAbove ? -height - 8 : 0)
    }
}

private struct ComposerActivityView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var totals = DiffLineTotals()

    var body: some View {
        GlassEffectContainer(spacing: MobiusSpace.s) {
            HStack(spacing: MobiusSpace.s) {
                ForEach(model.composerFooterWidgets) { widget in
                    FrontendWidgetView(widget: widget)
                }
                if totals.added > 0 || totals.removed > 0 {
                    Button { model.showFiles(.unstaged) } label: {
                        HStack(spacing: MobiusSpace.s) {
                            Text("+\(totals.added)").foregroundStyle(palette.signal)
                            Text("−\(totals.removed)").foregroundStyle(palette.danger)
                        }
                        .font(MobiusStyle.badgeFont)
                        .padding(.horizontal, MobiusSpace.m)
                        .frame(height: MobiusStyle.badgeHeight)
                        .mobiusGlass(in: Capsule(), interactive: true)
                        .frame(
                            minWidth: MobiusStyle.iconButtonSize,
                            minHeight: MobiusStyle.iconButtonSize
                        )
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.mobiusPlain)
                    .accessibilityLabel("Code changes")
                    .accessibilityValue("\(totals.added) additions, \(totals.removed) deletions")
                    .accessibilityHint("Opens modified files")
                }

                SessionStatsBadge()
            }
            .frame(minHeight: MobiusStyle.iconButtonSize)
            .scrollableRow()
        }
        .frame(maxWidth: .infinity)
        .accessibilityElement(children: .contain)
        .task(id: model.gitDiff) {
            let diff = model.gitDiff
            let countTask = Task.detached(priority: .utility) {
                diffTotals(diff)
            }
            let result = await countTask.value
            guard !Task.isCancelled else { return }
            totals = result
        }
    }

}

/// Context fill and elapsed execution time stay visible; deeper run totals live in the popover.
private struct SessionStatsBadge: View {
    @Environment(AppModel.self) private var model
    @State private var showsDetail = false

    var body: some View {
        if model.selectedSessionID != nil {
            TimelineView(.periodic(from: .now, by: 1)) { timeline in
                let elapsed = model.sessionElapsed(at: timeline.date)
                Button { showsDetail = true } label: {
                    MobiusBadge(
                        text: "\(model.contextFillPercent)% · \(formatDuration(elapsed))",
                        progress: model.contextFillFraction,
                        interactive: true
                    )
                    .frame(
                        minWidth: MobiusStyle.iconButtonSize,
                        minHeight: MobiusStyle.iconButtonSize
                    )
                    .contentShape(Rectangle())
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityLabel("Session observability")
                .accessibilityValue(
                    "\(model.contextFillPercent) percent context used, \(formatDuration(elapsed)) elapsed"
                )
                .sensoryFeedback(.selection, trigger: showsDetail)
                .popover(isPresented: $showsDetail, arrowEdge: .bottom) {
                    BadgePopover(title: "Session") {
                        BadgeStat(
                            label: "Context",
                            value: "\(model.contextFillPercent)% used · \(model.contextTokens.formatted()) / \(model.contextLimitTokens?.formatted() ?? "—")"
                        )
                        BadgeStat(
                            label: "Compactions",
                            value: model.sessionCompactionCount.formatted()
                        )
                        BadgeStat(label: "Elapsed", value: formatDuration(elapsed))
                        BadgeStat(label: "Runs", value: model.sessionRunCount.formatted())
                        BadgeStat(label: "Model calls", value: model.sessionModelCalls.formatted())
                        BadgeStat(label: "Tool calls", value: model.sessionToolCalls.formatted())
                        BadgeStat(
                            label: "Tool failures",
                            value: model.sessionFailedToolCalls.formatted()
                        )
                        BadgeStat(
                            label: "Run tokens",
                            value: (
                                model.runStats.usage.totalTokens
                                    + (model.runStats.active?.usage.totalTokens ?? 0)
                            ).formatted()
                        )
                        BadgeStat(label: "Cache hit", value: cacheHit(model.lastUsage))
                    }
                }
            }
        }
    }
}

struct BadgePopover<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            Text(title)
                .font(MobiusStyle.controlFont.weight(.semibold))
            // A full list (every subagent, every file) would otherwise grow the popover
            // past the screen with no way to reach the bottom.
            ScrollView { content }
                .frame(maxHeight: MobiusStyle.rowTouch * 8)
                .scrollBounceBehavior(.basedOnSize)
        }
        .padding(MobiusSpace.l)
        .frame(minWidth: 220, alignment: .leading)
        .presentationCompactAdaptation(.popover)
    }
}

private struct BadgeStat: View {
    @Environment(\.mobiusPalette) private var palette
    let label: String
    let value: String

    var body: some View {
        HStack(spacing: MobiusSpace.m) {
            Text(label)
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
            Spacer(minLength: MobiusSpace.s)
            Text(value)
                .font(MobiusStyle.bodyFont.monospacedDigit())
        }
        .accessibilityElement(children: .combine)
    }
}


private struct ComposerAttachmentsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            if !model.canSubmitAttachments {
                Text(model.attachmentSubmissionUnavailableMessage)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            // Tiles are too tall to stack: a few files would push the text field off screen.
            ScrollView(.horizontal) {
                HStack(spacing: MobiusSpace.s) {
                    ForEach(model.composerAttachments) { attachment in
                        ComposerAttachmentRow(attachment: attachment)
                    }
                }
            }
            .scrollIndicators(.hidden)
            .scrollBounceBehavior(.basedOnSize)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct ComposerAttachmentRow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let attachment: ComposerAttachment

    var body: some View {
        FileCard(
            name: attachment.name,
            detail: status,
            detailColor: statusColor,
            thumbnail: model.fileThumbnail(for: attachment)
        ) {
            // A tile has no room for a row of controls, so the state sits in the corner and
            // the glyph keeps saying which file this is.
            HStack(spacing: MobiusSpace.xxs) {
                stateControl
                Button("Remove attachment", glyph: .x) {
                    model.removeComposerAttachment(attachment.id)
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.mobiusPlain)
                .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                .disabled(isUploading)
            }
        }
        .accessibilityElement(children: .contain)
    }

    @ViewBuilder
    private var stateControl: some View {
        switch attachment.state {
        case .queued, .uploading:
            MobiusSpinner(size: MobiusStyle.glyphInline)
                .frame(width: MobiusStyle.rowCompact, height: MobiusStyle.rowCompact)
        case .uploaded:
            EmptyView()
        case .failed:
            Button("Retry upload", glyph: .arrowClockwise) {
                model.retryComposerAttachment(attachment.id)
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.mobiusPlain)
            .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
        }
    }

    private var status: Text {
        switch attachment.state {
        case .queued: Text("Waiting to upload")
        case .uploading: Text("Uploading")
        case .uploaded: Text(attachment.size, format: .byteCount(style: .file))
        case .failed(let message): Text(message)
        }
    }

    private var statusColor: Color {
        if case .failed = attachment.state { return palette.danger }
        return palette.muted
    }

    private var isUploading: Bool {
        if case .uploading = attachment.state { return true }
        return false
    }
}

private struct ApprovalView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let approval: PendingApproval

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            MobiusLabel(
                title: "Approval required",
                glyph: .shieldCheck,
                iconColor: palette.warning
            )
                .font(MobiusStyle.titleFont)
                .foregroundStyle(palette.warning)
            Text(approval.reason).font(MobiusStyle.bodyFont)
            ScrollView([.horizontal, .vertical]) {
                LazyVStack(alignment: .leading, spacing: MobiusSpace.s) {
                    ForEach(approval.calls) { call in
                        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                            Text(call.name).font(MobiusStyle.metadataFont.weight(.bold))
                            Text(call.arguments).font(MobiusStyle.metadataFont).textSelection(.enabled)
                        }
                        .padding(MobiusSpace.m)
                        .background(palette.raised, in: MobiusStyle.controlShape)
                        .accessibilityElement(children: .combine)
                        .accessibilityLabel("\(call.name), arguments \(call.arguments)")
                    }
                }
            }
            .frame(maxHeight: 180)
            ViewThatFits(in: .horizontal) {
                HStack(spacing: MobiusSpace.s) { actions }
                VStack(spacing: MobiusSpace.s) { actions }.buttonSizing(.flexible)
            }
            .buttonStyle(.mobiusGlass)
            .buttonBorderShape(.capsule)
            .frame(maxWidth: .infinity, alignment: .trailing)
        }
        .padding(MobiusStyle.cardPadding)
        .background(palette.warning.opacity(0.09), in: MobiusStyle.cardShape)
        .overlay {
            MobiusStyle.cardShape
                .stroke(palette.warning.opacity(0.55), lineWidth: MobiusStyle.borderWidth)
        }
    }

    @ViewBuilder
    private var actions: some View {
        Button("Abort", role: .destructive) { model.resolveApproval(.abort) }
        Button("Deny") { model.resolveApproval(.denied(rejection: "Denied in möbius App")) }
        Button("Approve for session") { model.resolveApproval(.approvedForSession) }
        Button("Approve once") { model.resolveApproval(.approved) }
            .mobiusProminentButton()
    }
}
