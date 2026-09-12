import Foundation
import SwiftUI

struct ComposerView: View {
    @Environment(AppModel.self) private var model
    let showBotSettings: () -> Void

    var body: some View {
        VStack(spacing: MobiusSpace.s) {
            ForEach(model.composerWidgets(in: .composerHeader)) { widget in
                FrontendWidgetView(widget: widget)
            }
            if let approval = model.chat.pendingApproval {
                ApprovalView(approval: approval)
            }
            if let picker = model.chat.pendingPicker {
                FrontendPickerView(picker: picker)
            }
            ComposerStack(showBotSettings: showBotSettings)
        }
        .frame(maxWidth: MobiusStyle.transcriptWidth)
        .frame(maxWidth: .infinity)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.bottom, MobiusSpace.m)
    }
}

private struct ComposerStack: View {
    @Environment(AppModel.self) private var model
    let showBotSettings: () -> Void

    var body: some View {
        VStack(spacing: MobiusSpace.xs) {
            if model.chat.selectedSessionID == nil, let path = model.chat.pendingNewChatWorkspace {
                VStack(alignment: .leading, spacing: 0) {
                    NewChatFolderPicker(path: path)
                    NewChatBotPicker()
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .menuIndicator(.hidden)
                .buttonStyle(.mobiusPlain)
                .disabled(!model.canCreateSession)
            } else {
                ComposerActivityView(showBotSettings: showBotSettings)
            }
            if model.chat.realtimeVoiceCall != nil {
                RealtimeVoiceComposer()
            } else {
                SessionComposerSurface()
            }
        }
    }
}

private struct NewChatFolderPicker: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let path: String

    var body: some View {
        Menu {
            Picker("Folder", selection: Binding(get: { path }, set: { model.chooseWorkspace($0) }))
            {
                ForEach(model.newChatWorkspacePaths, id: \.self) { folder in
                    MobiusLabel(verbatim: name(folder), glyph: .folder).tag(folder)
                }
            }
            .pickerStyle(.inline)
            Divider()
            Button("Add new folder", glyph: .plus) { model.openWorkspaceBrowser() }
        } label: {
            MobiusMenuLabel(verbatim: name(path), glyph: .folder, glyphColor: palette.muted)
                .frame(minHeight: MobiusStyle.iconButtonSize)
        }
        .accessibilityLabel("Folder")
        .accessibilityValue(path == "." ? name(path) : path)
        .accessibilityHint("Choose a project folder or add a new folder")
    }

    private func name(_ path: String) -> String {
        if path == "." {
            return model.localizedString("Default folder")
        }
        return WorkspaceSessions.workspaceName(path)
    }
}

private struct NewChatBotPicker: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        Menu {
            Picker(
                "Bot",
                selection: Binding(
                    get: { model.chat.pendingNewChatBotID },
                    set: { id in
                        if let bot = model.bots.first(where: { $0.id == id }) {
                            model.selectBotForNewChat(bot)
                        }
                    }
                )
            ) {
                ForEach(model.bots) { bot in
                    Label {
                        Text(verbatim: bot.name)
                    } icon: {
                        MobiusGlyph.aiScan.menuImage(bot.tint.color)
                    }
                    .tag(Optional(bot.id))
                }
            }
            .pickerStyle(.inline)
        } label: {
            MobiusMenuLabel(
                text: model.selectedBot.map { .verbatim($0.name) } ?? .localized("Choose Bot"),
                glyph: .aiScan,
                glyphColor: model.selectedBot?.tint.color ?? palette.muted
            )
            .frame(minHeight: MobiusStyle.iconButtonSize)
        }
        .accessibilityLabel("Bot")
        .accessibilityValue(model.selectedBot?.name ?? model.localizedString("Choose Bot"))
        .accessibilityHint("Choose the Bot for this chat")
        .sensoryFeedback(.selection, trigger: model.chat.pendingNewChatBotID)
    }
}

private struct RealtimeVoiceComposer: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let voice = model.chat.realtimeVoice
        VStack(spacing: MobiusSpace.s) {
            AudioLevelEqualizer(
                amplitude: sqrt(voice.audioLevels.displayLevel),
                flare: voice.levelFlare,
                playbackColor: voice.audioLevels.isPlaybackActive
                    ? model.selectedBot?.tint.color ?? palette.accent : nil
            )
            .frame(height: 104)
            .frame(maxWidth: .infinity)
            .padding(.horizontal, MobiusSpace.l)
            .padding(.top, MobiusSpace.m)
            .animation(
                reduceMotion ? nil : .smooth(duration: 0.09),
                value: [voice.audioLevels.displayLevel, voice.levelFlare]
            )
            .accessibilityHidden(true)

            HStack {
                Button {
                    voice.isMuted.toggle()
                } label: {
                    MobiusLabel(
                        title: voice.isMuted ? "Unmute" : "Mute",
                        glyph: voice.isMuted ? .micOff01 : .mic01
                    )
                }
                .buttonStyle(MobiusIconButtonStyle(prominent: voice.isMuted, bare: true))
                .accessibilityValue(voice.isMuted ? "Microphone muted" : "Microphone on")
                Spacer(minLength: MobiusSpace.s)
                if !voice.isConnected {
                    Text("Connecting voice")
                        .font(MobiusStyle.badgeFont)
                        .foregroundStyle(palette.muted)
                }
                Spacer(minLength: MobiusSpace.s)
                Button("End voice chat", glyph: .stopFill) { model.chat.stopRealtimeVoice() }
                    .buttonStyle(MobiusIconButtonStyle(bare: true))
            }
            .labelStyle(.iconOnly)
            .padding(.horizontal, MobiusStyle.iconRowPadding)
            .padding(.bottom, MobiusStyle.iconRowPadding)
        }
        .mobiusGlass(in: MobiusStyle.cardShape, interactive: true)
        .shadow(color: palette.shadow.opacity(0.18), radius: 12, y: 6)
    }

}

private struct SessionComposerSurface: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var chat = model.chat
        ComposerSurface(
            text: $chat.composer,
            hasContext: chat.hasComposerContext,
            compactLeadingInset: model.attachmentsEnabled
                ? MobiusStyle.iconRowPadding + MobiusStyle.iconButtonSize : MobiusSpace.l,
            compactTrailingInset: MobiusStyle.iconRowPadding
                + (model.selectedRouteSupportsRealtimeVoice && !model.composerUsesPrimaryVoice
                    ? 2 : 1)
                    * MobiusStyle.iconButtonSize,
            focusRequest: chat.composerFocusRequest,
            blurRequest: chat.composerBlurRequest,
            referenceRevision: chat.contributionsRevision + model.workspaceFilesRevision,
            suggestions: referenceSuggestions,
            send: { model.sendMessage() },
            context: { close in
                if let reply = chat.composerReply {
                    ReplyQuoteView(
                        reply: reply,
                        open: {
                            close(); chat.openMessageReply(reply)
                        },
                        dismiss: { chat.composerReply = nil }
                    )
                    .padding(.horizontal, MobiusSpace.m)
                    .padding(.top, MobiusSpace.m)
                }
                if !chat.composerAttachments.isEmpty {
                    ComposerAttachmentsView()
                        .padding(.horizontal, MobiusSpace.m)
                        .padding(.top, MobiusSpace.m)
                }
            },
            controls: { compact, didSend in
                ComposerOptionsView(
                    send: { delivery in
                        if model.sendMessage(delivery: delivery) { didSend() }
                    },
                    isCompact: compact
                )
            }
        )
    }

    private func referenceSuggestions(text: String, cursorOffset: Int) async
        -> ReferenceSuggestions?
    {
        if let commands = model.commandSuggestions(in: text, cursorOffset: cursorOffset) {
            return commands
        }
        let references = model.capabilityReferences
        let files = model.workspaceFiles
        return await Task.detached(priority: .userInitiated) {
            AppModel.referenceSuggestions(
                in: text, cursorOffset: cursorOffset,
                capabilityReferences: references, workspaceFiles: files)
        }.value
    }
}

private struct ComposerActivityView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var totals = DiffLineTotals()
    let showBotSettings: () -> Void

    var body: some View {
        GlassEffectContainer(spacing: MobiusSpace.s) {
            HStack(spacing: MobiusSpace.s) {
                ForEach(model.composerWidgets(in: .composerFooter)) { widget in
                    FrontendWidgetView(widget: widget)
                }
                if !model.simplifiedChatUI, totals.added > 0 || totals.removed > 0 {
                    Button {
                        model.showFiles(.unstaged)
                    } label: {
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
                    .accessibilityValue(
                        "\(totals.added) additions, \(totals.removed) deletions"
                    )
                    .accessibilityHint("Opens modified files")
                }

                if let bot = model.selectedBot {
                    BotActivityBadge(bot: bot, action: showBotSettings)
                }
                if !model.simplifiedChatUI { SessionStatsBadge() }
            }
            .frame(minHeight: MobiusStyle.iconButtonSize)
            .scrollableRow()
        }
        .frame(maxWidth: .infinity)
        .accessibilityElement(children: .contain)
        .task(id: model.gitDiffs[.unstaged]?.text) {
            let diff = model.gitDiffs[.unstaged]?.text ?? ""
            let countTask = Task.detached(priority: .utility) {
                diffTotals(diff)
            }
            let result = await countTask.value
            guard !Task.isCancelled else { return }
            totals = result
        }
    }

}

private struct BotActivityBadge: View {
    let bot: BotRecord
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            MobiusBadge(
                text: .verbatim(bot.name),
                glyph: .aiScan,
                glyphColor: bot.tint.color,
                interactive: true
            )
            .frame(minWidth: MobiusStyle.iconButtonSize, minHeight: MobiusStyle.iconButtonSize)
        }
        .buttonStyle(.mobiusPlain)
        .accessibilityLabel(Text("Bot \(bot.name)"))
        .accessibilityHint("Opens Bot agent settings")
    }
}

/// Context fill and elapsed execution time stay visible; deeper run totals live in the popover.
private struct SessionStatsBadge: View {
    @Environment(AppModel.self) private var model
    @Environment(\.locale) private var locale
    @State private var showsDetail = false

    var body: some View {
        if model.chat.selectedSessionID != nil {
            TimelineView(.periodic(from: .now, by: 1)) { timeline in
                let elapsed = model.sessionElapsed(at: timeline.date)
                Button {
                    showsDetail = true
                } label: {
                    MobiusBadge(
                        text: .verbatim(
                            "\(model.contextFillPercent)% · \(formatCompactDuration(elapsed, locale: locale))"
                        ),
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
                    "\(model.contextFillPercent) percent context used, \(formatCompactDuration(elapsed, locale: locale)) elapsed"
                )
                .sensoryFeedback(.selection, trigger: showsDetail)
                .popover(isPresented: $showsDetail, arrowEdge: .bottom) {
                    BadgePopover(localizedTitle: "Session") {
                        BadgeStat(
                            label: "Context",
                            value:
                                "\(model.contextFillPercent)% · \(model.chat.contextTokens.formatted()) / \(model.chat.contextLimitTokens?.formatted() ?? "—")"
                        )
                        BadgeStat(
                            label: "Compactions",
                            value: model.chat.sessionCompactionCount.formatted()
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
                            value: (model.chat.runStats.usage.totalTokens
                                + (model.chat.runStats.active?.usage.totalTokens ?? 0)).formatted()
                        )
                        BadgeStat(label: "Cache hit", value: cacheHit(model.chat.lastUsage))
                    }
                }
            }
        }
    }
}

struct BadgePopover<Content: View>: View {
    let title: MobiusText
    @ViewBuilder let content: Content

    init(title: String, @ViewBuilder content: () -> Content) {
        self.title = .verbatim(title)
        self.content = content()
    }

    init(localizedTitle title: LocalizedStringResource, @ViewBuilder content: () -> Content) {
        self.title = .localized(title)
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            title.text
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
    let label: LocalizedStringResource
    let value: String

    var body: some View {
        HStack(spacing: MobiusSpace.m) {
            Text(label)
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
            Spacer(minLength: MobiusSpace.s)
            Text(verbatim: value)
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
                    ForEach(model.chat.composerAttachments) { attachment in
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
        let thumbnail = model.chat.fileThumbnail(for: attachment)
        FileCard(
            name: attachment.name,
            detail: status,
            detailColor: statusColor,
            thumbnail: thumbnail
        )
        .blur(radius: isProcessing ? 1.5 : 0)
        .opacity(isProcessing ? 0.5 : 1)
        .overlay {
            if isProcessing {
                MobiusSpinner(size: 32)
            }
        }
        .overlay(alignment: .topTrailing) {
            HStack(spacing: MobiusSpace.xxs) {
                stateControl
                Button("Remove attachment", glyph: .x) {
                    model.removeComposerAttachment(attachment.id)
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.mobiusPlain)
                .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
            }
            .foregroundStyle(thumbnail == nil ? Color.primary : palette.onMedia)
            .shadow(
                color: thumbnail == nil ? .clear : palette.shadow.opacity(0.85),
                radius: 1,
                y: 1
            )
            .padding(MobiusSpace.xs)
        }
        .overlay(alignment: .bottom) {
            if let uploadProgress {
                ProgressView(value: uploadProgress)
                    .progressViewStyle(.linear)
                    .controlSize(.mini)
                    .tint(palette.accent)
                    .padding(.horizontal, MobiusSpace.s)
                    .padding(.bottom, MobiusSpace.xs)
                    .frame(maxWidth: .infinity)
                    .accessibilityLabel("Uploading")
                    .accessibilityValue(Text(uploadProgress, format: .percent))
            }
        }
        .clipShape(MobiusStyle.tileShape)
        .accessibilityElement(children: .contain)
    }

    @ViewBuilder
    private var stateControl: some View {
        switch attachment.state {
        case .preparing, .queued, .uploading, .uploaded:
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
        case .preparing: Text("Uploading")
        case .queued: Text("Waiting to upload")
        case .uploading: Text("Uploading")
        case .uploaded: Text(attachment.size, format: .byteCount(style: .file))
        case .failed(let message): Text(verbatim: message)
        }
    }

    private var statusColor: Color {
        if case .failed = attachment.state { return palette.danger }
        return palette.muted
    }

    private var isProcessing: Bool {
        switch attachment.state {
        case .preparing, .queued, .uploading: true
        case .uploaded, .failed: false
        }
    }

    private var uploadProgress: Double? {
        guard case .uploading(let bytes) = attachment.state, attachment.size > 0 else {
            return nil
        }
        return min(1, max(0, Double(bytes) / Double(attachment.size)))
    }
}

struct ApprovalView: View {
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
            Text(verbatim: approval.reason).font(MobiusStyle.bodyFont)
            ScrollView([.horizontal, .vertical]) {
                LazyVStack(alignment: .leading, spacing: MobiusSpace.s) {
                    ForEach(approval.calls) { call in
                        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                            Text(verbatim: call.name).font(MobiusStyle.metadataFont.weight(.bold))
                            Text(verbatim: call.arguments)
                                .font(MobiusStyle.metadataFont)
                                .textSelection(.enabled)
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
        .background(palette.panel, in: MobiusStyle.cardShape)
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
