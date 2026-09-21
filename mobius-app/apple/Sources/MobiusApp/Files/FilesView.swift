import Foundation
import SwiftUI
import HighlightSwift

struct FilesView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    // Translate the whole native action group; stacked buttons must keep their spacing.
    @State private var actionsBottom: CGFloat?

    // Isolate the inspector's actions from the chat navigation stack.
    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                FilesInspectorTabPicker()
                    .padding(.horizontal, MobiusSpace.m)
                    .padding(.bottom, MobiusSpace.m)
                FilesContent()
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .navigationTitle(navigationTitle)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .principal) {
                    FilesNavigationTitle()
                }
                ToolbarSpacer(.flexible, placement: .bottomBar)
                MobiusToolbarItem(placement: .bottomBar) {
                    MobiusToolbarIconButton(glyph: .check, label: "Done") {
                        model.discardFilePresentation()
                        model.showsInspector = false
                        dismiss()
                    }
                    .buttonStyle(.glass)
                    .mobiusBottomRailAligned(referenceBottom: actionsBottom)
                    .onGeometryChange(for: CGFloat.self) { geometry in
                        geometry.frame(in: .global).maxY
                    } action: { bottom in
                        if model.filesInspectorTab != .allFiles { actionsBottom = bottom }
                    }
                }
                .sharedBackgroundVisibility(.hidden)
                if model.filesInspectorTab == .allFiles {
                    MobiusToolbarItem(placement: .bottomBar) {
                        MobiusToolbarIconButton(
                            glyph: .plus,
                            label: "Create file",
                            action: model.createWorkspaceFile
                        )
                        .mobiusProminentToolbarButton()
                        .buttonStyle(.glass)
                        .disabled(!model.canOpenSession)
                        .mobiusBottomRailAligned(referenceBottom: actionsBottom)
                        .onGeometryChange(for: CGFloat.self) { geometry in
                            geometry.frame(in: .global).maxY
                        } action: { bottom in
                            actionsBottom = bottom
                        }
                    }
                    .sharedBackgroundVisibility(.hidden)
                }
            }
        }
        .background { palette.panel.ignoresSafeArea() }
        .mobiusSheet()
        .interactiveDismissDisabled(model.isLoadingFilePresentation)
    }

    private var navigationTitle: LocalizedStringResource {
        if model.filesInspectorTab == .modified { return model.modifiedFilesScope.title }
        return model.filesInspectorTab.title
    }
}

private struct FilesContent: View {
    @Environment(AppModel.self) private var model

    @ViewBuilder
    var body: some View {
        switch model.filesInspectorTab {
        case .modified: ModifiedFilesDiff()
        case .allFiles: WorkspaceFileList()
        case .chatFiles: ChatFileList()
        }
    }
}

private struct FilesNavigationTitle: View {
    @Environment(AppModel.self) private var model

    @ViewBuilder
    var body: some View {
        if model.filesInspectorTab == .modified {
            ModifiedFilesScopePicker()
        } else {
            HStack(spacing: MobiusSpace.xs) {
                Text(model.filesInspectorTab.title)
                    .font(MobiusStyle.titleFont)
                if model.filesInspectorTab == .allFiles && model.isLoadingWorkspaceFiles
                    || model.filesInspectorTab == .chatFiles && model.chat.isLoadingSessionFiles
                {
                    MobiusSpinner(size: MobiusStyle.glyphMark)
                }
            }
        }
    }
}

private struct ModifiedFilesDiff: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let scope = model.modifiedFilesScope
        let diff = scope.gitScope.flatMap { model.gitDiffs[$0] } ?? GitDiffState()
        WorkspaceDiffView(
            source: scope == .lastTurn ? model.lastTurnDiff : diff.text,
            revision: scope == .lastTurn ? model.lastTurnDiffRevision : diff.revision,
            isLoading: diff.isLoading,
            title: scope.diffTitle
        )
        .id(scope)
    }

}

private struct ModifiedFilesScopePicker: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Menu {
            ForEach(ModifiedFilesScope.allCases) { scope in
                let isSelected = scope == model.modifiedFilesScope
                Button {
                    model.selectModifiedFilesScope(scope)
                } label: {
                    MobiusLabel(
                        title: scope.title,
                        glyph: isSelected ? .check : .gitBranch
                    )
                }
            }
        } label: {
            Text(model.modifiedFilesScope.title)
                .font(MobiusStyle.titleFont)
                .padding(.horizontal, MobiusStyle.glyphGutter + MobiusSpace.xs)
                .overlay(alignment: .trailing) {
                    MobiusIcon(.caretDown, size: MobiusStyle.glyphMark, foreground: .secondary)
                }
                .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .menuIndicator(.hidden)
        .tint(.primary)
        .accessibilityLabel("Modified file view")
        .accessibilityValue(model.modifiedFilesScope.title)
        .help("Choose which Git changes to show")
    }
}

private struct FilesInspectorTabPicker: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Picker(
            "File collection",
            selection: Binding(
                get: { model.filesInspectorTab },
                set: { tab in model.selectFilesInspectorTab(tab) }
            )
        ) {
            ForEach(FilesInspectorTab.allCases) { tab in
                Text(tab.title).tag(tab)
            }
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        .accessibilityLabel("File collection")
    }
}

private extension FilesInspectorTab {
    var title: LocalizedStringResource {
        switch self {
        case .modified: "Modified"
        case .allFiles: "All Files"
        case .chatFiles: "Chat Files"
        }
    }
}

private extension ModifiedFilesScope {
    var diffTitle: LocalizedStringResource {
        switch self {
        case .lastTurn: "changes from the last turn"
        case .unstaged: "unstaged changes"
        case .staged: "staged changes"
        case .committed: "last commit"
        }
    }

    var title: LocalizedStringResource {
        switch self {
        case .lastTurn: "Last turn"
        case .unstaged: "Unstaged"
        case .staged: "Staged"
        case .committed: "Last Commit"
        }
    }
}

private struct FileExtensionMetadata {
    let glyph: MobiusGlyph
    let highlightLanguage: HighlightLanguage?
}

private let defaultFileExtensionMetadata = FileExtensionMetadata(
    glyph: .fileText,
    highlightLanguage: nil
)

private let fileExtensionMetadataByExtension: [String: FileExtensionMetadata] = {
    let groups: [([String], FileExtensionMetadata)] = [
        (["py", "pyi", "pyw"], .init(glyph: .python, highlightLanguage: .python)),
        (["ts", "tsx"], .init(glyph: .typeScript, highlightLanguage: .typeScript)),
        (["js", "jsx", "mjs", "cjs"], .init(glyph: .javaScript, highlightLanguage: .javaScript)),
        (["csv", "tsv"], .init(glyph: .csv, highlightLanguage: nil)),
        (["rs"], .init(glyph: .rust, highlightLanguage: .rust)),
        (["go"], .init(glyph: .go, highlightLanguage: .go)),
        (["md", "mdx", "markdown"], .init(glyph: .markdown, highlightLanguage: .markdown)),
        (["swift"], .init(glyph: .fileScript, highlightLanguage: .swift)),
        (["c", "h"], .init(glyph: .fileScript, highlightLanguage: .c)),
        (["cpp", "hpp", "cc", "cxx"], .init(glyph: .fileScript, highlightLanguage: .cPlusPlus)),
        (["java"], .init(glyph: .fileScript, highlightLanguage: .java)),
        (["kt", "kts"], .init(glyph: .fileScript, highlightLanguage: .kotlin)),
        (["rb"], .init(glyph: .fileScript, highlightLanguage: .ruby)),
        (["php"], .init(glyph: .fileScript, highlightLanguage: .php)),
        (["sh", "zsh", "bash"], .init(glyph: .fileScript, highlightLanguage: .shell)),
        (["doc", "docx", "odt", "pages", "rtf"], .init(glyph: .doc, highlightLanguage: nil)),
        (
            ["png", "jpg", "jpeg", "gif", "heic", "webp", "svg"],
            .init(glyph: .image01, highlightLanguage: nil)
        ),
        (["json"], .init(glyph: .gear, highlightLanguage: .json)),
        (["yaml", "yml"], .init(glyph: .gear, highlightLanguage: .yaml)),
        (["toml"], .init(glyph: .gear, highlightLanguage: .toml)),
        (["xml", "ini", "plist"], .init(glyph: .gear, highlightLanguage: nil)),
    ]
    return Dictionary(
        uniqueKeysWithValues: groups.flatMap { group in
            group.0.map { ($0, group.1) }
        })
}()

extension String {
    var fileGlyph: MobiusGlyph { fileExtensionMetadata.glyph }
    var sourceHighlightLanguage: HighlightLanguage? {
        fileExtensionMetadata.highlightLanguage
    }

    private var fileExtensionMetadata: FileExtensionMetadata {
        fileExtensionMetadataByExtension[URL(fileURLWithPath: self).pathExtension.lowercased()]
            ?? defaultFileExtensionMetadata
    }
}

private struct WorkspaceFileList: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var tree: [FileTreeNode] = []

    var body: some View {
        VStack(spacing: 0) {
            if model.workspaceFilesTruncated && !model.isLoadingWorkspaceFiles {
                HStack(spacing: MobiusSpace.s) {
                    MobiusIcon(
                        .warning,
                        size: MobiusStyle.glyphInline,
                        foreground: palette.warning
                    )
                    Text(
                        "Some workspace files are not shown. Ignore generated folders to keep the catalog focused."
                    )
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, MobiusSpace.m)
                .padding(.vertical, MobiusSpace.s)
                .accessibilityElement(children: .combine)
                Divider()
            }
            content
        }
        .task(id: model.workspaceFilesRevision) {
            let files = model.workspaceFiles
            async let builtTree = FileTreeNode.tree(from: files)
            let result = await builtTree
            guard !Task.isCancelled else { return }
            tree = result
        }
    }

    @ViewBuilder
    private var content: some View {
        if model.isLoadingWorkspaceFiles {
            WorkspaceFileLoadingList()
        } else if model.workspaceFiles.isEmpty {
            MobiusUnavailable(title: "No workspace files", glyph: .fileMagnifyingGlass)
        } else {
            List {
                OutlineGroup(tree, children: \.children) { node in
                    if node.isFolder {
                        FileTreeRow(node: node)
                    } else {
                        fileButton(path: node.id, label: FileTreeRow(node: node))
                    }
                }
                .buttonStyle(.mobiusPlain)
                .inspectorFileListRow()
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
        }
    }

    private func fileButton(path: String, label: some View) -> some View {
        Button {
            guard let file = model.workspaceFiles.first(where: { $0.path == path }) else { return }
            model.previewWorkspaceFile(file)
        } label: {
            label
        }
        .buttonStyle(.mobiusPlain)
        .disabled(model.isLoadingFilePresentation)
        .accessibilityLabel("Open workspace file \(path)")
    }
}

private struct WorkspaceFileLoadingList: View {
    private static let nodes = [
        FileTreeNode(id: "Sources", name: "Sources", size: nil, children: []),
        FileTreeNode(id: "MobiusApp.swift", name: "MobiusApp.swift", size: 4_096, children: nil),
        FileTreeNode(id: "README.md", name: "README.md", size: 2_048, children: nil),
        FileTreeNode(id: "Tests", name: "Tests", size: nil, children: []),
        FileTreeNode(id: "Package.resolved", name: "Package.resolved", size: 8_192, children: nil),
    ]

    var body: some View {
        List(Self.nodes) { node in
            FileTreeRow(node: node)
                .inspectorFileListRow()
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .mobiusLoadingPlaceholder("Loading workspace files")
    }
}

private struct FileTreeRow: View {
    @Environment(\.mobiusPalette) private var palette
    let node: FileTreeNode

    var body: some View {
        HStack(spacing: MobiusSpace.m) {
            MobiusIcon(
                node.isFolder ? .folder : node.id.fileGlyph,
                size: 15,
                foreground: node.isFolder ? palette.muted : palette.accent
            )
            Text(verbatim: node.name)
                .font(MobiusStyle.bodyFont)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: MobiusSpace.s)
            if let size = node.size {
                Text(size, format: .byteCount(style: .file))
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            }
        }
        .frame(minHeight: MobiusStyle.rowRegular)
        .contentShape(Rectangle())
    }
}

private struct ChatFileList: View {
    @Environment(AppModel.self) private var model

    private var agentFiles: [SessionFileRecord] {
        model.chat.sessionFiles.filter { $0.origin == .agent }
    }

    private var userFiles: [SessionFileRecord] {
        model.chat.sessionFiles.filter { $0.origin == .user }
    }

    var body: some View {
        List {
            fileSection(
                "Agent files",
                loadingTitle: "Loading agent files",
                emptyTitle: "No agent files",
                records: agentFiles,
                emptyGlyph: .aiScan,
                accessibilityOrigin: "agent"
            )
            fileSection(
                "User uploads",
                loadingTitle: "Loading user uploads",
                emptyTitle: "No user uploads",
                records: userFiles,
                emptyGlyph: .fileUpload,
                accessibilityOrigin: "user-uploaded"
            )
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
    }

    private func fileSection(
        _ title: LocalizedStringResource,
        loadingTitle: LocalizedStringResource,
        emptyTitle: LocalizedStringResource,
        records: [SessionFileRecord],
        emptyGlyph: MobiusGlyph,
        accessibilityOrigin: LocalizedStringResource
    ) -> some View {
        Section {
            if model.chat.isLoadingSessionFiles {
                InspectorFileLoadingRows(title: loadingTitle)
            } else if records.isEmpty {
                InspectorEmptyRow(title: emptyTitle, glyph: emptyGlyph)
            } else {
                ForEach(records) { record in
                    SessionFileInspectorRow(
                        file: record.file,
                        accessibilityLabel: "Open \(accessibilityOrigin) file \(record.file.name)"
                    )
                }
            }
        } header: {
            Text(title)
        }
    }
}

private struct SessionFileInspectorRow: View {
    @Environment(AppModel.self) private var model
    let file: SessionFileReference
    let accessibilityLabel: LocalizedStringResource

    var body: some View {
        HStack(spacing: 0) {
            Button {
                model.previewSessionFile(file, sessionID: model.chat.selectedSessionID)
            } label: {
                InspectorFileRow(
                    name: file.name,
                    detail: file.mediaType,
                    size: file.size,
                    showsDisclosure: false
                )
            }
            .accessibilityLabel(Text(accessibilityLabel))

            Menu {
                Button("Preview", glyph: file.name.fileGlyph) {
                    model.previewSessionFile(file, sessionID: model.chat.selectedSessionID)
                }
                Button("Share or Save…", glyph: .arrowUpRight01) {
                    model.saveOrShareSessionFile(file, sessionID: model.chat.selectedSessionID)
                }
            } label: {
                MobiusIcon(.dotsThree, size: MobiusStyle.glyphInline)
                    .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                    .contentShape(Rectangle())
            }
            .accessibilityLabel("File actions for \(file.name)")
            .help("File actions")
        }
        .buttonStyle(.mobiusPlain)
        .disabled(model.isLoadingFilePresentation)
        .inspectorFileListRow()
    }
}

private struct InspectorFileLoadingRows: View {
    let title: LocalizedStringResource

    var body: some View {
        // One row holding both, so the shimmer sweeps the block: applied per row, the band
        // is masked by each row on its own and only one of them ever lights.
        VStack(spacing: 0) {
            ForEach(0..<2, id: \.self) { index in
                HStack(spacing: 0) {
                    InspectorFileRow(
                        name: index == 0 ? "conversation.txt" : "attachment.pdf",
                        detail: index == 0 ? "text/plain" : "application/pdf",
                        size: index == 0 ? 2_048 : 8_192,
                        showsDisclosure: false
                    )
                    Color.clear
                        .frame(
                            width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                }
            }
        }
        .mobiusLoadingPlaceholder(.localized(title))
        .inspectorFileListRow()
    }
}

private struct InspectorEmptyRow: View {
    @Environment(\.mobiusPalette) private var palette
    let title: LocalizedStringResource
    let glyph: MobiusGlyph

    var body: some View {
        VStack(spacing: MobiusSpace.s) {
            MobiusIcon(glyph, size: 44, foreground: palette.muted)
            Text(title)
                .font(MobiusStyle.metadataFont.weight(.semibold))
                .foregroundStyle(palette.muted)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, MobiusSpace.l)
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .accessibilityElement(children: .combine)
    }
}

private struct InspectorFileListRow: ViewModifier {
    @Environment(\.mobiusPalette) private var palette

    func body(content: Content) -> some View {
        content
            .listRowInsets(EdgeInsets(top: 4, leading: 16, bottom: 4, trailing: 12))
            .listRowBackground(Color.clear)
            .listRowSeparatorTint(palette.line)
    }
}

extension View {
    fileprivate func inspectorFileListRow() -> some View {
        modifier(InspectorFileListRow())
    }
}

private struct InspectorFileRow: View {
    @Environment(\.mobiusPalette) private var palette
    let name: String
    let detail: String
    let size: Int64
    var showsDisclosure = true

    var body: some View {
        HStack(spacing: MobiusSpace.m) {
            MobiusIcon(name.fileGlyph, foreground: .primary)
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                Text(verbatim: name)
                    .font(MobiusStyle.metadataFont.weight(.semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(verbatim: detail)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer(minLength: MobiusSpace.s)
            Text(size, format: .byteCount(style: .file))
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
            if showsDisclosure {
                MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
            }
        }
        .frame(minHeight: MobiusStyle.iconButtonSize)
        .contentShape(Rectangle())
    }
}
