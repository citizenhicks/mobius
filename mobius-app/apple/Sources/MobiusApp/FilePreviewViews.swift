import Foundation
import SwiftUI
import HighlightSwift
import UIKit

struct InspectorLoadingView: View {
    let title: MobiusText

    init(title: LocalizedStringResource) {
        self.title = .localized(title)
    }

    init(verbatim title: String) {
        self.title = .verbatim(title)
    }

    var body: some View {
        ProgressView { title.text }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .accessibilityLabel(title.text)
    }
}

struct TextFilePreviewView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let preview: TextFilePreview

    var body: some View {
        NavigationStack {
            Group {
                if isWorkspaceFile {
                    workspaceEditor
                } else {
                    NumberedSourceText(
                        preview.contents,
                        language: preview.name.sourceHighlightLanguage
                    )
                }
            }
                .navigationTitle(navigationTitle)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: isWorkspaceFile ? .cancellationAction : .confirmationAction) {
                    Button(isWorkspaceFile ? "Cancel" : "Done", action: dismiss.callAsFunction)
                        .disabled(model.isSavingWorkspaceFile)
                }
                if isWorkspaceFile {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Save", action: save)
                            .disabled(!canSave)
                    }
                }
            }
        }
        .mobiusSheet(detents: [.large])
        .interactiveDismissDisabled(model.isSavingWorkspaceFile)
    }

    private var isWorkspaceFile: Bool {
        preview.workspaceSessionID != nil && preview.workspacePath != nil
    }

    private var isNewFile: Bool {
        draft.originalWorkspacePath?.isEmpty == true
    }

    private var navigationTitle: String {
        guard isNewFile, !draftPath.isEmpty else { return preview.name }
        return URL(fileURLWithPath: draftPath).lastPathComponent
    }

    private var canSave: Bool {
        guard isWorkspaceFile,
              model.canModifySelectedSession,
              !model.isSavingWorkspaceFile,
              !draftPath.isEmpty,
              draftPath.utf8.count <= 4_096,
              draftContents.utf8.count <= maximumWorkspaceTextFileBytes
        else { return false }
        return isNewFile || draftContents != draft.originalContents
    }

    private var workspaceEditor: some View {
        VStack(spacing: 0) {
            if isNewFile {
                TextField("Relative file path", text: pathBinding)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(MobiusStyle.bodyFont.monospaced())
                    .padding(MobiusSpace.m)
                Divider()
            }
            HighlightedSourceEditor(
                source: contentsBinding,
                language: draftPath.sourceHighlightLanguage
            )
                .font(MobiusStyle.bodyFont.monospaced())
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .padding(MobiusSpace.s)
                .privacySensitive()
        }
    }

    private func save() {
        guard let sessionID = preview.workspaceSessionID else { return }
        model.saveWorkspaceFile(
            sessionID: sessionID,
            path: draftPath,
            content: draftContents
        )
    }

    private var draft: TextFilePreview {
        guard let draft = model.textFilePreview, draft.id == preview.id else { return preview }
        return draft
    }

    private var draftPath: String { draft.workspacePath ?? "" }

    private var draftContents: String { draft.contents }

    private var pathBinding: Binding<String> {
        Binding(
            get: { draftPath },
            set: { model.updateWorkspaceFileDraft(id: preview.id, path: $0) }
        )
    }

    private var contentsBinding: Binding<String> {
        Binding(
            get: { draftContents },
            set: { model.updateWorkspaceFileDraft(id: preview.id, contents: $0) }
        )
    }
}

private struct HighlightedSourceEditor: View {
    @Environment(\.colorScheme) private var colorScheme
    @Binding var source: String
    let language: HighlightLanguage?
    @State private var attributedSource: AttributedString
    @State private var selection = AttributedTextSelection()

    init(source: Binding<String>, language: HighlightLanguage?) {
        _source = source
        self.language = language
        _attributedSource = State(initialValue: AttributedString(source.wrappedValue))
    }

    var body: some View {
        TextEditor(text: attributedBinding, selection: $selection)
            .scrollContentBackground(.hidden)
            .task(id: renderRequest) {
                await render(renderRequest)
            }
    }

    private var attributedBinding: Binding<AttributedString> {
        Binding(
            get: { attributedSource },
            set: { value in
                attributedSource = value
                let updatedSource = String(value.characters)
                guard updatedSource != source else { return }
                source = updatedSource
            }
        )
    }

    private var renderRequest: SourceHighlightRequest {
        SourceHighlightRequest(
            source: source,
            language: language,
            isDark: colorScheme == .dark
        )
    }

    private func render(_ request: SourceHighlightRequest) async {
        if String(attributedSource.characters) != request.source {
            attributedSource.transform(updating: &selection) {
                $0 = AttributedString(request.source)
            }
        }
        guard !request.source.isEmpty else { return }

        try? await Task.sleep(for: .milliseconds(180))
        guard !Task.isCancelled else { return }

        let highlightTask = Task.detached(priority: .userInitiated) {
            await request.highlightedText()
        }
        let highlighted = await withTaskCancellationHandler {
            await highlightTask.value
        } onCancel: {
            highlightTask.cancel()
        }
        guard let highlighted,
              !Task.isCancelled,
              source == request.source,
              String(attributedSource.characters) == request.source,
              attributedSource != highlighted
        else { return }
        attributedSource.transform(updating: &selection) { $0 = highlighted }
    }
}

struct SessionFileShareView: UIViewControllerRepresentable {
    let file: SessionFileShareItem

    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: [file.url], applicationActivities: nil)
    }

    func updateUIViewController(_ viewController: UIActivityViewController, context: Context) {}
}

struct NumberedSourceLine: Identifiable, Sendable {
    let id: Int
    let text: AttributedString
}

private struct SourceHighlightRequest: Equatable, Sendable {
    let source: String
    let language: HighlightLanguage?
    let isDark: Bool
}

private extension SourceHighlightRequest {
    func highlightedText() async -> AttributedString? {
        guard !Task.isCancelled else { return nil }
        let colors: HighlightColors = isDark ? .dark(.xcode) : .light(.xcode)
        let mode = language.map(HighlightMode.language) ?? .automatic
        guard let result = try? await Highlight().request(source, mode: mode, colors: colors),
              !Task.isCancelled
        else { return nil }
        return NumberedSourceText.restoringWhitespace(result.attributedText, in: source)
    }
}

struct NumberedSourceText: View {
    @Environment(\.colorScheme) private var colorScheme
    let source: String
    let language: HighlightLanguage?
    @State private var lines: [NumberedSourceLine] = []

    init(_ source: String, language: HighlightLanguage? = nil) {
        self.source = source
        self.language = language
    }

    var body: some View {
        ScrollView(.vertical) {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(lines) { line in
                    HStack(alignment: .top, spacing: 0) {
                        Text(verbatim: String(line.id))
                            .font(MobiusStyle.metadataFont)
                            .monospacedDigit()
                            .foregroundStyle(.secondary)
                            .frame(width: 44, alignment: .trailing)
                            .padding(.trailing, MobiusSpace.m)
                        Text(line.text.characters.isEmpty ? AttributedString(" ") : line.text)
                            .font(MobiusStyle.metadataFont)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, MobiusSpace.l)
            .padding(.trailing, MobiusSpace.l)
        }
        .textSelection(.enabled)
        .overlay {
            if lines.isEmpty, !source.isEmpty {
                ProgressView("Rendering text")
            }
        }
        .task(id: renderRequest) {
            lines = []

            let request = renderRequest
            let plainTask = Task.detached(priority: .userInitiated) {
                Self.lines(from: AttributedString(request.source))
            }
            let plainLines = await plainTask.value
            guard !Task.isCancelled else { return }
            lines = plainLines

            let highlightTask = Task.detached(priority: .userInitiated) {
                guard let text = await request.highlightedText() else {
                    return Optional<[NumberedSourceLine]>.none
                }
                return Self.lines(from: text)
            }
            let highlightedLines = await withTaskCancellationHandler {
                await highlightTask.value
            } onCancel: {
                highlightTask.cancel()
            }
            guard let highlightedLines, !Task.isCancelled else { return }
            lines = highlightedLines
        }
    }

    private var renderRequest: SourceHighlightRequest {
        SourceHighlightRequest(
            source: source,
            language: language,
            isDark: colorScheme == .dark
        )
    }

    nonisolated static func restoringWhitespace(
        _ highlighted: AttributedString,
        in source: String
    ) -> AttributedString {
        let trimmed = source.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              String(highlighted.characters) == trimmed,
              let range = source.range(of: trimmed)
        else { return AttributedString(source) }
        var result = AttributedString(String(source[..<range.lowerBound]))
        result.append(highlighted)
        result.append(AttributedString(String(source[range.upperBound...])))
        return result
    }

    nonisolated static func lines(from text: AttributedString) -> [NumberedSourceLine] {
        var lines: [AttributedString] = []
        var start = text.startIndex
        while let newline = text.characters[start...].firstIndex(where: \.isNewline) {
            lines.append(AttributedString(text[start..<newline]))
            start = text.characters.index(after: newline)
        }
        lines.append(AttributedString(text[start..<text.endIndex]))
        return lines.enumerated().map { NumberedSourceLine(id: $0.offset + 1, text: $0.element) }
    }
}

struct ReadOnlyTranscriptSheet<Header: View>: View {
    @Environment(\.mobiusPalette) private var palette
    @State private var retainedEntryID: String?
    @State private var selectedDetent: PresentationDetent = .large
    @State private var waiting = TranscriptWaitingHold()
    let header: Header
    let entries: [TranscriptEntry]
    let fileSessionID: String?
    let hasEarlier: Bool
    let isLoading: Bool
    /// The run is still going, so the gap between two steps is the model thinking rather
    /// than the end of the transcript. Drives the same waiting line the chat shows.
    let isRunning: Bool
    let loadEarlier: () -> Void

    init(
        entries: [TranscriptEntry],
        fileSessionID: String?,
        hasEarlier: Bool,
        isLoading: Bool,
        isRunning: Bool,
        loadEarlier: @escaping () -> Void,
        @ViewBuilder header: () -> Header
    ) {
        self.header = header()
        self.entries = entries
        self.fileSessionID = fileSessionID
        self.hasEarlier = hasEarlier
        self.isLoading = isLoading
        self.isRunning = isRunning
        self.loadEarlier = loadEarlier
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            ZStack {
                ScrollViewReader { proxy in
                    ScrollView {
                        VStack(alignment: .leading, spacing: 0) {
                            if hasEarlier {
                                TranscriptPaginationButton(
                                    isLoading: isLoading,
                                    isEnabled: !isLoading
                                ) { loadEarlierPage() }
                                .padding(.bottom, MobiusStyle.transcriptRowSpacing)
                            }
                            TranscriptRowsView(
                                projection: projection,
                                fileSessionID: fileSessionID,
                                activeStepID: activeStepID(in: entries, isRunning: isRunning),
                                turnDiff: { transcriptTurnDiff(for: $0, in: entries) }
                            )
                            TranscriptTailView(
                                slot: projection.waiting,
                                topSpacing: MobiusStyle.transcriptRowSpacing
                            )
                        }
                        .scrollTargetLayout()
                        .frame(maxWidth: MobiusStyle.transcriptWidth)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(MobiusStyle.transcriptPadding)
                    }
                    .scrollIndicators(.hidden)
                    .refreshable { loadEarlierPage() }
                    .onChange(of: isLoading) { _, loading in
                        guard !loading, let retainedEntryID else { return }
                        let row = projection.rows.first { row in
                            row.id == retainedEntryID
                                || row.records.contains {
                                    $0.presentationID == retainedEntryID
                                }
                        }
                        if let row { proxy.scrollTo(row.id, anchor: .top) }
                        self.retainedEntryID = nil
                    }
                }

                if isLoading {
                    ZStack {
                        palette.canvas.opacity(0.58)
                        MobiusComposingOrb()
                            .frame(width: 112, height: 112)
                    }
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel("Loading earlier agent messages")
                }
            }
        }
        .mobiusSheet(selection: $selectedDetent)
        .onChange(of: isWaitingForModel, initial: true) { _, isWaiting in
            waiting.update(isWaiting: isWaiting)
        }
        .onDisappear { waiting.update(isWaiting: false) }
    }

    /// Mirrors the chat's rule: a live run with nothing pending is the model thinking.
    private var isWaitingForModel: Bool {
        TranscriptWaitingNote.isWaiting(
            hasActiveTurn: isRunning,
            lastEntryIsPending: entries.last?.pending == true,
            connectionIsReady: true,
            hasPendingApproval: false,
            hasPendingPicker: false
        )
    }

    private var projection: TranscriptProjection {
        TranscriptProjection(
            entries: entries,
            breakBefore: retainedEntryID,
            waitingPhrase: waiting.phrase
        )
    }

    private func loadEarlierPage() {
        guard !isLoading else { return }
        retainedEntryID = entries.first?.presentationID
        loadEarlier()
    }
}

struct PreviewTranscriptSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let preview: TranscriptPreview

    var body: some View {
        ReadOnlyTranscriptSheet(
            entries: currentPreview.entries,
            fileSessionID: model.selectedSessionID,
            hasEarlier: currentPreview.next != nil,
            isLoading: model.isLoadingPreviewPage,
            isRunning: currentPreview.status == "running",
            loadEarlier: loadEarlierPage,
            header: { header }
        )
    }

    private var header: some View {
        HStack(spacing: MobiusSpace.s) {
            Text(verbatim: agentName)
                .font(MobiusStyle.controlFont.weight(.semibold))
                .lineLimit(1)
                .truncationMode(.middle)
            if let status = currentPreview.status {
                headerSeparator
                Text(verbatim: status)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(status == "errored" ? palette.danger : palette.muted)
                    .lineLimit(1)
            }
            if let choice = modelChoice {
                headerSeparator
                MobiusMenuLabel(
                    verbatim: model.modelLabel(for: choice),
                    glyph: model.providerSymbol(for: choice)
                        .flatMap(MobiusSymbol.knownGlyph(for:)) ?? .aiScan,
                    detail: choice.reasoningEffort?.capitalized,
                    showsDisclosure: false
                )
                .layoutPriority(1)
                .accessibilityLabel("Model and reasoning")
                .accessibilityValue(modelSummary(choice))
            }
            Spacer(minLength: 0)
            if !currentPreview.context.isEmpty {
                SettingsInfoButton(
                    title: .localized("Spawn context: \(currentPreview.context)"),
                    detail: .localized(spawnContextDetail),
                    glyph: spawnContextGlyph,
                    accessibilityHint: .localized("Explains the inherited conversation context")
                )
            }
        }
        .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize, alignment: .leading)
        .padding(.leading, MobiusSpace.l)
        .padding(.trailing, MobiusStyle.iconRowPadding)
        .padding(.vertical, MobiusSpace.s)
        .accessibilityElement(children: .contain)
    }

    private var headerSeparator: some View {
        Text("•")
            .font(MobiusStyle.metadataFont)
            .foregroundStyle(palette.muted)
            .accessibilityHidden(true)
    }

    private func loadEarlierPage() {
        guard let next = currentPreview.next, !model.isLoadingPreviewPage else { return }
        model.loadPreviewPage(next)
    }

    private var currentPreview: TranscriptPreview {
        if model.presentedPreview?.id == preview.id, let presented = model.presentedPreview {
            return presented
        }
        return model.previews.first(where: { $0.id == preview.id }) ?? preview
    }

    private var agentName: String {
        currentPreview.title
    }

    private var modelChoice: ModelChoice? {
        guard let route = currentPreview.model else { return nil }
        return model.modelChoices.first { $0.route == route }
    }

    private func modelSummary(_ choice: ModelChoice) -> String {
        let name = model.modelLabel(for: choice)
        guard let reasoning = choice.reasoningEffort, !reasoning.isEmpty else { return name }
        return "\(name) · \(reasoning.capitalized)"
    }

    private var spawnContextGlyph: MobiusGlyph {
        let context = currentPreview.context.lowercased()
        if context.hasPrefix("no ") || context == "none" { return .circle }
        if context.hasPrefix("full") { return .circleDot }
        return .circleDotDashed
    }

    private var spawnContextDetail: LocalizedStringResource {
        let context = currentPreview.context.lowercased()
        if context.hasPrefix("no ") || context == "none" {
            return "This agent started fresh with only its assigned task. It inherited none of the parent conversation."
        }
        if context.hasPrefix("full") {
            return "This agent inherited the full parent conversation as its starting context."
        }
        return "This agent inherited \(currentPreview.context.lowercased()) from the parent conversation."
    }
}

struct PreviewBlockView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let block: FrontendBlock

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            ForEach(block.files) { file in
                SessionFileCard(file: file, sessionID: model.selectedSessionID)
            }
            if !block.text.isEmpty {
                HStack(alignment: .top, spacing: MobiusSpace.s) {
                    if block.pending { ProgressView().controlSize(.mini) }
                    CollapsibleText(text: block.text)
                        .font(
                            block.format == "unified_diff"
                                ? MobiusStyle.metadataFont
                                : MobiusStyle.bodyFont
                        )
                        .foregroundStyle(
                            block.tone == "neutral" ? Color.primary : palette.tone(block.tone)
                        )
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }
    }
}
