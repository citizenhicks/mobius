import Foundation
import SwiftUI

private struct CollapsibleTextEndAttribute: TextAttribute {}

struct CollapsibleText: View {
    private static let defaultCollapsedLineLimit = 21
    // Bound the text SwiftUI must shape while collapsed. Four thousand characters still
    // exceed 21 lines at the transcript's widest supported layout, including on iPad.
    private static let collapsedCharacterLimit = 4_096

    @Environment(\.mobiusPalette) private var palette
    @State private var isExpanded = false
    @State private var isTruncated = false
    @State private var hasMeasured = false
    let text: String
    var rendersMarkdown = false
    var streaming = false
    /// A transcript row can afford 21 lines before it collapses; a board post packed among
    /// its neighbours cannot, so the surface owns the threshold.
    var collapsedLineLimit = Self.defaultCollapsedLineLimit

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            renderedText
            if isTruncated {
                Button(isExpanded ? "Show less" : "Read more") {
                    isExpanded.toggle()
                }
                .font(MobiusStyle.captionFont.weight(.semibold))
                .foregroundStyle(palette.accent)
                .buttonStyle(.mobiusPlain)
                .frame(minHeight: MobiusStyle.iconButtonSize, alignment: .leading)
                .accessibilityHint(
                    isExpanded ? "Collapses the message" : "Expands the full message"
                )
            }
        }
        .onChange(of: text) { _, _ in
            guard !isExpanded else { return }
            hasMeasured = false
            isTruncated = false
        }
    }

    @ViewBuilder
    private var renderedText: some View {
        if rendersMarkdown && (isExpanded || (hasMeasured && !isTruncated)) {
            MobiusMarkdownText(text, streaming: streaming)
                .equatable()
                .background {
                    if !isExpanded {
                        measuredText
                            .lineLimit(collapsedLineLimit)
                            .truncationMode(.tail)
                            .hidden()
                            .onPreferenceChange(Text.LayoutKey.self, perform: measureTruncation)
                    }
                }
        } else {
            measuredText
                .lineLimit(isExpanded ? nil : collapsedLineLimit)
                .truncationMode(.tail)
                .textSelection(.enabled)
                .onPreferenceChange(Text.LayoutKey.self, perform: measureTruncation)
        }
    }

    private func measureTruncation(_ layouts: Text.LayoutKey.Value) {
        guard !isExpanded, !layouts.isEmpty else { return }
        let reachedEnd = !hasVisibleEnd || layouts.contains { proxy in
            proxy.layout.contains { line in
                line.contains { run in
                    run[CollapsibleTextEndAttribute.self] != nil
                }
            }
        }
        isTruncated = hidesBoundedSuffix
            || layouts.contains { $0.layout.isTruncated }
            || !reachedEnd
        hasMeasured = true
    }

    private var measuredText: Text {
        let content = rendersMarkdown
            ? inlineMarkdownPreview(displayedText)
            : AttributedString(displayedText)
        guard let end = content.characters.lastIndex(where: { !$0.isNewline }) else {
            return Text(content)
        }
        let afterEnd = content.characters.index(after: end)
        let prefix = Text(AttributedString(content[..<end]))
        let markedEnd = Text(AttributedString(content[end..<afterEnd]))
            .customAttribute(CollapsibleTextEndAttribute())
        let trailing = Text(AttributedString(content[afterEnd...]))
        return Text("\(prefix)\(markedEnd)\(trailing)")
    }

    private var displayedText: String {
        guard !isExpanded else { return text }
        let prefix = text.prefix(Self.collapsedCharacterLimit)
        guard prefix.endIndex != text.endIndex else { return text }
        return "\(prefix)…"
    }

    private var hidesBoundedSuffix: Bool {
        text.prefix(Self.collapsedCharacterLimit).endIndex != text.endIndex
    }

    private var hasVisibleEnd: Bool {
        displayedText.contains { !$0.isNewline }
    }
}

struct TranscriptFileCards: View {
    let files: [SessionFileReference]
    let sessionID: String?

    var body: some View {
        ForEach(files) { file in
            SessionFileCard(file: file, sessionID: sessionID)
        }
    }
}

struct TurnDiffCard: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var document: UnifiedDiffDocument?
    @State private var isExpanded = false
    @State private var showsDetails = false
    let source: String

    var body: some View {
        VStack(spacing: 0) {
            if let document, !document.files.isEmpty {
                card(document)
            }
        }
        .task(id: source) {
            document = nil
            let parseTask = Task.detached(priority: .userInitiated) {
                UnifiedDiffDocument(source)
            }
            let parsed = await withTaskCancellationHandler {
                await parseTask.value
            } onCancel: {
                parseTask.cancel()
            }
            guard !Task.isCancelled else { return }
            document = parsed
        }
        .sheet(isPresented: $showsDetails) {
            NavigationStack {
                ZStack {
                    WorkspaceDiffView(
                        source: source,
                        revision: 0,
                        isLoading: false,
                        title: "changes from this turn"
                    )
                }
                .navigationTitle("Turn changes")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done") { showsDetails = false }
                    }
                }
            }
            .mobiusSheet()
        }
    }

    private func card(_ document: UnifiedDiffDocument) -> some View {
        let files = document.fileChanges
        return VStack(spacing: 0) {
            Button {
                withAnimation(reduceMotion ? nil : .easeOut(duration: 0.16)) {
                    isExpanded.toggle()
                }
            } label: {
                HStack(spacing: MobiusSpace.s) {
                    Text(changedFileCount(files.count))
                        .foregroundStyle(.primary)
                    Text("+\(document.added)")
                        .foregroundStyle(palette.signal)
                    Text("−\(document.removed)")
                        .foregroundStyle(palette.danger)
                    Spacer(minLength: MobiusSpace.s)
                    MobiusIcon(.caretRight, size: MobiusStyle.glyphInline, foreground: palette.muted)
                        .rotationEffect(.degrees(isExpanded ? 90 : 0))
                }
                .font(MobiusStyle.badgeFont)
                .padding(.horizontal, MobiusSpace.l)
                .frame(minHeight: MobiusStyle.rowTouch)
                .contentShape(Rectangle())
            }
            .buttonStyle(.mobiusPlain)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(turnDiffAccessibilityLabel(document))
            .accessibilityValue(isExpanded ? "Expanded" : "Collapsed")
            .accessibilityHint(isExpanded ? "Collapses the file list" : "Shows the file list")

            if isExpanded {
                ForEach(files.prefix(3)) { file in
                    fileRow(file)
                }
                Button {
                    showsDetails = true
                } label: {
                    HStack(spacing: MobiusSpace.s) {
                        Text(detailsTitle(files.count))
                            .foregroundStyle(palette.muted)
                        Spacer(minLength: MobiusSpace.s)
                        MobiusIcon(
                            .caretRight,
                            size: MobiusStyle.glyphInline,
                            foreground: palette.muted
                        )
                    }
                    .padding(.horizontal, MobiusSpace.l)
                    .frame(minHeight: MobiusStyle.rowTouch)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityHint("Opens the full diff")
            }
        }
        .mobiusGlass(in: MobiusStyle.cardShape)
    }

    private func fileRow(_ file: UnifiedDiffFileChange) -> some View {
        HStack(spacing: MobiusSpace.s) {
            Text(verbatim: file.path)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: MobiusSpace.s)
            Text("+\(file.added)").foregroundStyle(palette.signal)
            Text("−\(file.removed)").foregroundStyle(palette.danger)
        }
        .font(MobiusStyle.metadataFont)
        .padding(.horizontal, MobiusSpace.l)
        .frame(minHeight: MobiusStyle.rowRegular)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            "File \(file.path), \(file.added) additions, \(file.removed) removals"
        )
    }

    private func detailsTitle(_ fileCount: Int) -> LocalizedStringResource {
        let remaining = fileCount - 3
        if remaining == 1 { return "View 1 more file" }
        if remaining > 1 { return "View \(remaining) more files" }
        return "View all changes"
    }
}

private func changedFileCount(_ count: Int) -> LocalizedStringResource {
    count == 1 ? "1 file changed" : "\(count) files changed"
}

private func turnDiffAccessibilityLabel(_ document: UnifiedDiffDocument) -> Text {
    let files = changedFileCount(document.fileChanges.count)
    let additions: LocalizedStringResource = document.added == 1
        ? "1 addition"
        : "\(document.added) additions"
    let removals: LocalizedStringResource = document.removed == 1
        ? "1 removal"
        : "\(document.removed) removals"
    return Text("\(files), \(additions), \(removals)")
}

struct SessionFileCard: View {
    @Environment(AppModel.self) private var model
    let file: SessionFileReference
    let sessionID: String?

    var body: some View {
        let thumbnail = model.fileThumbnail(for: file, sessionID: sessionID)
        Button {
            model.previewSessionFile(file, sessionID: sessionID)
        } label: {
            SessionFileCardLabel(file: file, thumbnail: thumbnail)
        }
        .buttonStyle(.mobiusPlain)
        .accessibilityLabel("Open file \(file.name)")
        .accessibilityHint("Downloads and opens a preview")
        .frame(
            minWidth: MobiusStyle.iconButtonSize,
            minHeight: MobiusStyle.iconButtonSize
        )
        .contentShape(Rectangle())
        .contextMenu {
            Button("Preview", glyph: file.name.fileGlyph) {
                model.previewSessionFile(file, sessionID: sessionID)
            }
            Button("Share or Save…", glyph: .arrowUpRight01) {
                model.saveOrShareSessionFile(file, sessionID: sessionID)
            }
        }
        .disabled(model.isLoadingFilePresentation)
        .task(id: thumbnailTaskID) {
            model.requestSessionFileThumbnail(file, sessionID: sessionID)
        }
    }

    private var thumbnailTaskID: FileThumbnailKey? {
        guard model.connectionState.isReady,
              let sessionID
        else { return nil }
        return .session(sessionID: sessionID, fileID: file.id)
    }
}

struct QueuedMessageView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let widget: MountedWidget

    var body: some View {
        HStack {
            Spacer(minLength: 42)
            CollapsibleText(text: widget.widget.text)
                .font(MobiusStyle.bodyFont)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.horizontal, MobiusSpace.l)
                .padding(.vertical, MobiusSpace.m)
                .background(palette.accentSoft.opacity(0.24), in: MobiusStyle.cardShape)
                .overlay {
                    MobiusStyle.cardShape.stroke(
                        palette.accent.opacity(0.42),
                        style: StrokeStyle(lineWidth: 1.25, lineCap: .round, dash: [1, 4])
                    )
                }
                .contentShape(MobiusStyle.cardShape)
                .contextMenu {
                    if editAction != nil {
                        Button("Edit", glyph: .pencilSimple) {
                            model.editWidgetInputInComposer(widget)
                        }
                    }
                    Button("Copy", glyph: .copy) { copyToPasteboard(widget.widget.text) }
                }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Queued message")
        .accessibilityValue(editAction == nil ? "Queued" : "Queued, editable until sent")
        .accessibilityActions {
            if editAction != nil {
                Button("Edit queued message") { model.editWidgetInputInComposer(widget) }
            }
            Button("Copy queued message") { copyToPasteboard(widget.widget.text) }
        }
    }

    private var editAction: AgentOperation? {
        guard let action = widget.widget.action, action.capabilityInput != nil else { return nil }
        return action
    }
}

private struct SessionFileCardLabel: View {
    @Environment(\.mobiusPalette) private var palette
    let file: SessionFileReference
    let thumbnail: CGImage?

    var body: some View {
        FileCard(
            name: file.name,
            detail: Text(
                "\(fileKind(name: file.name, mediaType: file.mediaType).text) · \(Text(file.size, format: .byteCount(style: .file)))"
            ),
            detailColor: palette.muted,
            thumbnail: thumbnail,
            size: cardSize
        )
    }

    private var cardSize: CGSize {
        guard let thumbnail else { return CGSize(width: 136, height: 112) }
        let width = CGFloat(thumbnail.width)
        let height = CGFloat(thumbnail.height)
        let scale = min(136 / width, 112 / height)
        return CGSize(width: width * scale, height: height * scale)
    }
}

/// The shared file tile: raster thumbnails run edge to edge; other files retain their
/// glyph, name, and one line of detail.
struct FileCard<Trailing: View>: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let name: String
    let detail: Text
    let detailColor: Color
    let thumbnail: CGImage?
    let size: CGSize
    let trailing: Trailing

    init(
        name: String,
        detail: Text,
        detailColor: Color,
        thumbnail: CGImage? = nil,
        size: CGSize = CGSize(width: 136, height: 112),
        @ViewBuilder trailing: () -> Trailing
    ) {
        self.name = name
        self.detail = detail
        self.detailColor = detailColor
        self.thumbnail = thumbnail
        self.size = size
        self.trailing = trailing()
    }

    var body: some View {
        content
        .frame(width: size.width, height: size.height)
        .background(palette.raised)
        .compositingGroup()
        .clipShape(MobiusStyle.tileShape)
        .overlay(alignment: .topTrailing) {
            trailing
                .foregroundStyle(thumbnail == nil ? Color.primary : palette.onMedia)
                .shadow(
                    color: thumbnail == nil ? .clear : palette.shadow.opacity(0.85),
                    radius: 1,
                    y: 1
                )
                .padding(MobiusSpace.xs)
        }
        .contentShape(MobiusStyle.tileShape)
        .animation(reduceMotion ? nil : .spring(duration: 0.4, bounce: 0.18), value: thumbnail != nil)
    }

    private var content: some View {
        // The placeholder stays put and the thumbnail dissolves over it, so only one
        // thing fades while the tile springs into the image's aspect ratio.
        ZStack {
            placeholder
            if let thumbnail {
                Image(thumbnail, scale: 1, label: Text(verbatim: name))
                    .resizable()
                    .scaledToFill()
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(palette.raised)
                    .clipped()
                    .accessibilityHidden(true)
                    .transition(.opacity)
            }
        }
    }

    private var placeholder: some View {
        VStack(spacing: 0) {
            MobiusIcon(.fileText, size: 26, foreground: .primary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            Text(verbatim: name)
                .font(MobiusStyle.badgeFont)
                .lineLimit(1)
                .truncationMode(.middle)
            detail
                .font(MobiusStyle.badgeFont)
                .foregroundStyle(detailColor)
                .lineLimit(1)
        }
        .padding(MobiusSpace.m)
    }
}

/// The extension reads faster than a media type, but a name without one still needs a word.
private func fileKind(name: String, mediaType: String) -> MobiusText {
    let ext = URL(fileURLWithPath: name).pathExtension
    if !ext.isEmpty { return .verbatim(ext.uppercased()) }
    if let kind = mediaType.split(separator: "/").last {
        return .verbatim(kind.uppercased())
    }
    return .localized("File")
}

extension FileCard where Trailing == EmptyView {
    init(
        name: String,
        detail: Text,
        detailColor: Color,
        thumbnail: CGImage? = nil,
        size: CGSize = CGSize(width: 136, height: 112)
    ) {
        self.init(
            name: name,
            detail: detail,
            detailColor: detailColor,
            thumbnail: thumbnail,
            size: size
        ) { EmptyView() }
    }
}

struct MessageActionButton: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isHovered = false
    let title: MobiusText
    let glyph: MobiusGlyph
    let action: () -> Void

    init(
        title: LocalizedStringResource,
        glyph: MobiusGlyph,
        action: @escaping () -> Void
    ) {
        self.title = .localized(title)
        self.glyph = glyph
        self.action = action
    }

    init(
        verbatim title: String,
        glyph: MobiusGlyph,
        action: @escaping () -> Void
    ) {
        self.title = .verbatim(title)
        self.glyph = glyph
        self.action = action
    }

    var body: some View {
        // Secondary actions, so a smaller glyph in a smaller box than a standalone icon button:
        // the box is what spaces these apart, and the context menu carries the same actions.
        Button(action: action) {
            ZStack {
                MobiusIcon(
                    glyph,
                    size: 13,
                    foreground: isHovered ? palette.accent : palette.muted
                )
                .id(glyph)
                .transition(.scale(scale: 0.7).combined(with: .opacity))
            }
            .frame(width: 26, height: 26)
            .contentShape(Rectangle())
            .animation(reduceMotion ? nil : .snappy(duration: 0.18), value: glyph)
        }
        .buttonStyle(.mobiusPlain)
        .onHover { isHovered = $0 }
        .animation(.easeOut(duration: 0.12), value: isHovered)
        .accessibilityLabel(title.text)
        .help(title.text)
    }
}
