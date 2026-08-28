import SwiftUI

private struct RenderedWorkspaceDiff: Sendable {
    let revision: Int
    let document: UnifiedDiffDocument
}

struct WorkspaceDiffView: View {
    @State private var rendered: RenderedWorkspaceDiff?
    @State private var expandedFileIDs: Set<Int> = []
    let source: String
    let revision: Int
    let isLoading: Bool
    let title: LocalizedStringResource

    var body: some View {
        content
            .task(id: revision) {
                rendered = nil
                expandedFileIDs.removeAll()
                guard !source.isEmpty else { return }

                let parseTask = Task.detached(priority: .userInitiated) {
                    UnifiedDiffDocument(source)
                }
                let document = await withTaskCancellationHandler {
                    await parseTask.value
                } onCancel: {
                    parseTask.cancel()
                }
                guard !Task.isCancelled else { return }
                rendered = RenderedWorkspaceDiff(revision: revision, document: document)
            }
    }

    @ViewBuilder
    private var content: some View {
        if isLoading {
            InspectorLoadingView(title: "Loading \(title)")
        } else if source.isEmpty {
            MobiusUnavailable(title: "No \(title)", glyph: .gitBranch)
        } else if let rendered, rendered.revision == revision {
            if rendered.document.files.isEmpty {
                MobiusUnavailable(title: "No displayable changes", glyph: .gitBranch)
            } else {
                UnifiedDiffView(
                    document: rendered.document,
                    expandedFileIDs: $expandedFileIDs
                )
            }
        } else {
            InspectorLoadingView(title: "Preparing \(title)")
        }
    }
}

struct InlineUnifiedDiffView: View {
    @Environment(\.mobiusPalette) private var palette
    let source: String
    @State private var document: UnifiedDiffDocument?
    @State private var expandedFileIDs: Set<Int> = []

    var body: some View {
        content
            .task(id: source) {
                document = nil
                expandedFileIDs.removeAll()
                guard !source.isEmpty else { return }

                let parseTask = Task.detached(priority: .userInitiated) {
                    UnifiedDiffDocument(source)
                }
                let parsed = await withTaskCancellationHandler {
                    await parseTask.value
                } onCancel: {
                    parseTask.cancel()
                }
                guard !Task.isCancelled else { return }
                expandedFileIDs = Set(parsed.files.map(\.id))
                document = parsed
            }
    }

    @ViewBuilder
    private var content: some View {
        if source.isEmpty {
            EmptyView()
        } else if let document {
            if document.files.isEmpty {
                Text(verbatim: source)
                    .font(MobiusStyle.metadataFont.monospaced())
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(MobiusSpace.m)
                    .background(palette.panel, in: MobiusStyle.tileShape)
            } else {
                VStack(alignment: .leading, spacing: MobiusSpace.s) {
                    ForEach(document.files) { file in
                        VStack(spacing: 0) {
                            DiffFileHeader(
                                file: file,
                                isExpanded: expandedFileIDs.contains(file.id),
                                toggle: { toggle(file.id) }
                            )
                            if expandedFileIDs.contains(file.id) {
                                ForEach(file.rows) { row in
                                    DiffRowView(row: row)
                                }
                            }
                        }
                    }

                    if document.isTruncated {
                        DiffTruncationWarning()
                            .padding(.horizontal, MobiusSpace.m)
                            .padding(.vertical, MobiusSpace.s)
                    }
                }
                .accessibilityElement(children: .contain)
                .accessibilityLabel(diffAccessibilityLabel(document))
            }
        } else {
            HStack(spacing: MobiusSpace.s) {
                ProgressView().controlSize(.small)
                Text("Preparing code change")
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(MobiusSpace.m)
            .background(palette.panel, in: MobiusStyle.tileShape)
        }
    }

    private func toggle(_ id: Int) {
        if expandedFileIDs.contains(id) {
            expandedFileIDs.remove(id)
        } else {
            expandedFileIDs.insert(id)
        }
    }
}

private struct UnifiedDiffView: View {
    let document: UnifiedDiffDocument
    @Binding var expandedFileIDs: Set<Int>

    var body: some View {
        List {
            ForEach(document.files) { file in
                DiffFileHeader(
                    file: file,
                    isExpanded: expandedFileIDs.contains(file.id),
                    toggle: { toggle(file.id) }
                )
                .diffListRow(topPadding: 10)

                if expandedFileIDs.contains(file.id) {
                    ForEach(file.rows) { row in
                        DiffRowView(row: row)
                            .diffListRow(bottomPadding: row.id == file.rows.last?.id ? 10 : 0)
                    }
                }
            }

            if document.isTruncated {
                DiffTruncationWarning()
                    .diffListRow(topPadding: 8, bottomPadding: 12)
            }
        }
        .environment(\.defaultMinListRowHeight, 0)
        .listStyle(.plain)
        .listRowSpacing(0)
        .scrollContentBackground(.hidden)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(diffAccessibilityLabel(document))
    }

    private func toggle(_ id: Int) {
        if expandedFileIDs.contains(id) {
            expandedFileIDs.remove(id)
        } else {
            expandedFileIDs.insert(id)
        }
    }
}

private func diffAccessibilityLabel(_ document: UnifiedDiffDocument) -> Text {
    let files: LocalizedStringResource = document.files.count == 1
        ? "1 file"
        : "\(document.files.count) files"
    let additions: LocalizedStringResource = document.added == 1
        ? "1 addition"
        : "\(document.added) additions"
    let removals: LocalizedStringResource = document.removed == 1
        ? "1 removal"
        : "\(document.removed) removals"
    return Text("Code diff, \(files), \(additions) and \(removals)")
}

private struct DiffTruncationWarning: View {
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusIcon(.warning, size: MobiusStyle.glyphInline, foreground: palette.warning)
            Text("Diff truncated at the safe transfer limit")
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
        }
    }
}

private struct DiffFileHeader: View {
    @Environment(\.mobiusPalette) private var palette
    let file: UnifiedDiffFile
    let isExpanded: Bool
    let toggle: () -> Void

    var body: some View {
        Button(action: toggle) {
            HStack(spacing: MobiusSpace.m) {
                MobiusIcon(file.path.fileGlyph, size: MobiusStyle.glyphLead, foreground: palette.accent)
                VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                    Text(verbatim: file.name)
                        .font(MobiusStyle.metadataFont.weight(.semibold))
                        .lineLimit(1)
                        .truncationMode(.middle)
                    if let parentPath = file.parentPath {
                        Text(verbatim: parentPath)
                            .font(MobiusStyle.metadataFont)
                            .foregroundStyle(palette.muted)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                Spacer(minLength: MobiusSpace.s)
                HStack(spacing: MobiusSpace.xs) {
                    Text("+\(file.added)").foregroundStyle(palette.signal)
                    Text("−\(file.removed)").foregroundStyle(palette.danger)
                }
                .font(MobiusStyle.metadataFont.weight(.semibold))
                .fixedSize()
                MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
                    .rotationEffect(.degrees(isExpanded ? 90 : 0))
                    .animation(.snappy(duration: 0.18), value: isExpanded)
            }
            .padding(.horizontal, MobiusSpace.m)
            .frame(minHeight: 54)
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .background(palette.raised, in: headerShape)
        .overlay { headerShape.stroke(palette.line.opacity(0.55), lineWidth: 0.5) }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            "File \(file.path), \(file.added) additions, \(file.removed) removals"
        )
        .accessibilityValue(isExpanded ? Text("Expanded") : Text("Collapsed"))
        .accessibilityHint(
            isExpanded ? Text("Collapses this file") : Text("Shows changed lines")
        )
    }

    private var headerShape: UnevenRoundedRectangle {
        let bottomRadius = isExpanded ? 0 : MobiusStyle.tileRadius
        return UnevenRoundedRectangle(
            cornerRadii: .init(
                topLeading: MobiusStyle.tileRadius,
                bottomLeading: bottomRadius,
                bottomTrailing: bottomRadius,
                topTrailing: MobiusStyle.tileRadius
            ),
            style: .continuous
        )
    }
}

private struct DiffRowView: View {
    @Environment(\.mobiusPalette) private var palette
    let row: UnifiedDiffRow

    @ViewBuilder
    var body: some View {
        switch row.kind {
        case let .hunk(hunk):
            hunkHeader(hunk)
        case .addition, .removal, .context, .metadata:
            codeLine
        }
    }

    private func hunkHeader(_ hunk: UnifiedDiffHunk) -> some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusIcon(.caretDown, size: MobiusStyle.glyphMark, foreground: palette.muted)
            Text(hunk.title)
                .font(MobiusStyle.metadataFont.weight(.semibold))
                .foregroundStyle(palette.muted)
            Spacer(minLength: MobiusSpace.s)
            if hunk.added > 0 {
                Text("+\(hunk.added)").foregroundStyle(palette.signal)
            }
            if hunk.removed > 0 {
                Text("−\(hunk.removed)").foregroundStyle(palette.danger)
            }
        }
        .font(MobiusStyle.metadataFont.weight(.semibold))
        .padding(.horizontal, MobiusSpace.m)
        .padding(.vertical, MobiusSpace.s)
        .frame(maxWidth: .infinity, minHeight: 32, alignment: .leading)
        .background(palette.accentSoft.opacity(0.45))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            "\(hunk.title), \(hunk.added) additions, \(hunk.removed) removals"
        )
    }

    private var codeLine: some View {
        HStack(alignment: .top, spacing: 0) {
            gutter(row.oldNumber)
            gutter(row.newNumber)
            Text(verbatim: marker)
                .font(MobiusStyle.metadataFont.weight(.bold))
                .foregroundStyle(markerColor)
                .frame(width: 24)
            Text(verbatim: row.text.isEmpty ? " " : row.text)
                .font(MobiusStyle.metadataFont)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.trailing, MobiusSpace.m)
        }
        .frame(maxWidth: .infinity, minHeight: 23, alignment: .leading)
        .background {
            ZStack {
                baseBackground
                changeHighlight
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(accessibilityLabel))
    }

    private func gutter(_ number: Int?) -> some View {
        Text(verbatim: number.map(String.init) ?? "")
            .font(MobiusStyle.metadataFont)
            .monospacedDigit()
            .foregroundStyle(palette.muted)
            .frame(width: 30, alignment: .trailing)
            .padding(.trailing, MobiusSpace.xs)
            .frame(maxHeight: .infinity)
            .background(gutterBackground)
            .overlay(alignment: .trailing) {
                Rectangle().fill(palette.line.opacity(0.6)).frame(width: 0.5)
            }
    }

    private var marker: String {
        switch row.kind {
        case .addition: "+"
        case .removal: "−"
        case .context: " "
        case .metadata: "·"
        case .hunk: ""
        }
    }

    private var markerColor: Color {
        switch row.kind {
        case .addition: palette.signal
        case .removal: palette.danger
        case .context, .metadata, .hunk: palette.muted
        }
    }

    private var baseBackground: Color {
        switch row.kind {
        case .addition, .removal, .context: palette.panel
        case .metadata: palette.raised.opacity(0.72)
        case .hunk: .clear
        }
    }

    private var changeHighlight: Color {
        switch row.kind {
        case .addition: palette.signal.opacity(0.16)
        case .removal: palette.danger.opacity(0.16)
        case .context, .metadata, .hunk: .clear
        }
    }

    private var gutterBackground: Color {
        switch row.kind {
        case .addition, .removal: .clear
        case .context, .metadata, .hunk: palette.canvas.opacity(0.42)
        }
    }

    private var accessibilityLabel: LocalizedStringResource {
        let location: LocalizedStringResource
        switch (row.oldNumber, row.newNumber) {
        case let (old?, new?): location = "old line \(old), new line \(new)"
        case let (old?, nil): location = "old line \(old)"
        case let (nil, new?): location = "new line \(new)"
        default: location = "metadata"
        }
        let change: LocalizedStringResource
        switch row.kind {
        case .addition: change = "Added"
        case .removal: change = "Removed"
        case .context: change = "Context"
        case .metadata: change = "Metadata"
        case .hunk: change = "Hunk"
        }
        return "\(change), \(location): \(row.text)"
    }
}

private struct DiffListRowModifier: ViewModifier {
    let topPadding: CGFloat
    let bottomPadding: CGFloat

    func body(content: Content) -> some View {
        content
            .padding(.top, topPadding)
            .padding(.bottom, bottomPadding)
            .listRowInsets(EdgeInsets(top: 0, leading: 14, bottom: 0, trailing: 14))
            .listRowSeparator(.hidden)
            .listRowBackground(Color.clear)
    }
}

private extension View {
    func diffListRow(topPadding: CGFloat = 0, bottomPadding: CGFloat = 0) -> some View {
        modifier(DiffListRowModifier(topPadding: topPadding, bottomPadding: bottomPadding))
    }
}

private extension UnifiedDiffFile {
    var name: String { path.split(separator: "/").last.map(String.init) ?? path }

    var parentPath: String? {
        let components = path.split(separator: "/")
        guard components.count > 1 else { return nil }
        return components.dropLast().joined(separator: "/")
    }
}
