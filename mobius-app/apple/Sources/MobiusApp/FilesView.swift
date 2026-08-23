import Foundation
import SwiftUI
import HighlightSwift

struct FilesView: View {
    @Environment(AppModel.self) private var model

    // A NavigationStack for the title and, more to the point, for `.searchable`: the search
    // field only renders inside a navigation container.
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
            .background(MobiusBackdrop())
            .navigationTitle(navigationTitle)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .principal) {
                    FilesNavigationTitle()
                }
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { model.showsInspector = false }
                }
            }
        }
        .presentationDragIndicator(.visible)
    }

    private var navigationTitle: String {
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
                    || model.filesInspectorTab == .chatFiles && model.isLoadingSessionFiles {
                    MobiusSpinner(size: MobiusStyle.glyphMark)
                }
            }
        }
    }
}

private struct ModifiedFilesDiff: View {
    @Environment(AppModel.self) private var model

    @ViewBuilder
    var body: some View {
        switch model.modifiedFilesScope {
        case .lastTurn:
            WorkspaceDiffView(
                source: model.lastTurnDiff,
                revision: model.lastTurnDiffRevision,
                isLoading: false,
                title: "changes from the last turn"
            )
            .id(ModifiedFilesScope.lastTurn)
        case .unstaged:
            WorkspaceDiffView(
                source: model.gitDiff,
                revision: model.gitDiffRevision,
                isLoading: model.isLoadingGitDiff,
                title: "unstaged changes"
            )
            .id(ModifiedFilesScope.unstaged)
        case .staged:
            WorkspaceDiffView(
                source: model.stagedGitDiff,
                revision: model.stagedGitDiffRevision,
                isLoading: model.isLoadingStagedGitDiff,
                title: "staged changes"
            )
            .id(ModifiedFilesScope.staged)
        case .committed:
            WorkspaceDiffView(
                source: model.committedGitDiff,
                revision: model.committedGitDiffRevision,
                isLoading: model.isLoadingCommittedGitDiff,
                title: "last commit"
            )
            .id(ModifiedFilesScope.committed)
        }
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
            HStack(spacing: MobiusSpace.xs) {
                Text(model.modifiedFilesScope.title)
                    .font(MobiusStyle.titleFont)
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
    var title: String {
        switch self {
        case .modified: "Modified"
        case .allFiles: "All Files"
        case .chatFiles: "Chat Files"
        }
    }
}

private extension ModifiedFilesScope {
    var title: String {
        switch self {
        case .lastTurn: "Last turn"
        case .unstaged: "Unstaged"
        case .staged: "Staged"
        case .committed: "Last Commit"
        }
    }
}

extension String {
    var fileGlyph: MobiusGlyph {
        switch URL(fileURLWithPath: self).pathExtension.lowercased() {
        case "py", "pyi", "pyw": .python
        case "ts", "tsx": .typeScript
        case "js", "jsx", "mjs", "cjs": .javaScript
        case "csv", "tsv": .csv
        case "rs": .rust
        case "go": .go
        case "md", "mdx", "markdown": .markdown
        case "swift", "c", "h", "cpp", "hpp", "java", "kt", "kts", "rb", "php", "sh", "zsh": .fileScript
        case "doc", "docx", "odt", "pages", "rtf": .doc
        case "png", "jpg", "jpeg", "gif", "heic", "webp", "svg": .image01
        case "json", "yaml", "yml", "toml", "xml", "ini", "plist": .gear
        default: .fileText
        }
    }

    var sourceHighlightLanguage: HighlightLanguage? {
        switch URL(fileURLWithPath: self).pathExtension.lowercased() {
        case "py", "pyi", "pyw": .python
        case "rs": .rust
        case "go": .go
        case "ts", "tsx": .typeScript
        case "js", "jsx", "mjs", "cjs": .javaScript
        case "md", "mdx", "markdown": .markdown
        case "swift": .swift
        case "c", "h": .c
        case "cpp", "hpp", "cc", "cxx": .cPlusPlus
        case "java": .java
        case "kt", "kts": .kotlin
        case "rb": .ruby
        case "php": .php
        case "sh", "zsh", "bash": .shell
        case "json": .json
        case "yaml", "yml": .yaml
        case "toml": .toml
        default: nil
        }
    }
}

private struct WorkspaceFileList: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var tree: [FileTreeNode] = []
    @State private var query = ""
    @State private var matches: [WorkspaceFileRecord] = []
    @State private var matchedQuery = ""

    var body: some View {
        VStack(spacing: 0) {
            if model.workspaceFilesTruncated && !model.isLoadingWorkspaceFiles {
                HStack(spacing: MobiusSpace.s) {
                    MobiusIcon(
                        .warning,
                        size: MobiusStyle.glyphInline,
                        foreground: palette.warning
                    )
                    Text("Some workspace files are not shown. Ignore generated folders to keep the catalog focused.")
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
            .searchable(text: $query, placement: .toolbar, prompt: "Search files")
            .task(id: model.workspaceFilesRevision) {
                let files = model.workspaceFiles
                async let builtTree = FileTreeNode.tree(from: files)
                let result = await builtTree
                guard !Task.isCancelled else { return }
                tree = result
            }
            .task(id: searchRequest) {
                guard !query.isEmpty else {
                    matches = []
                    matchedQuery = ""
                    return
                }
                try? await Task.sleep(for: .milliseconds(120))
                guard !Task.isCancelled else { return }
                let files = model.workspaceFiles
                let query = query
                let searchTask = Task.detached(priority: .userInitiated) {
                    files.filter { $0.path.localizedCaseInsensitiveContains(query) }
                }
                let result = await searchTask.value
                guard !Task.isCancelled else { return }
                matches = result
                matchedQuery = query
            }
    }

    @ViewBuilder
    private var content: some View {
        if model.isLoadingWorkspaceFiles {
            WorkspaceFileLoadingList()
        } else if model.workspaceFiles.isEmpty {
            MobiusUnavailable(title: "No workspace files", glyph: .fileMagnifyingGlass)
        } else if !query.isEmpty {
            searchResults
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

    /// A tree hides matches inside collapsed folders, so searching switches to the flat list
    /// of hits with their full path.
    @ViewBuilder
    private var searchResults: some View {
        if matchedQuery != query {
            InspectorLoadingView(title: "Searching files")
        } else if matches.isEmpty {
            MobiusUnavailable(title: "No matching files", glyph: .magnifyingGlass)
        } else {
            List(matches) { file in
                fileButton(
                    path: file.path,
                    label: InspectorFileRow(
                        name: URL(fileURLWithPath: file.path).lastPathComponent,
                        detail: file.path,
                        size: Int64(clamping: file.size)
                    )
                )
                .inspectorFileListRow()
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
        }
    }

    private var searchRequest: WorkspaceFileSearchRequest {
        WorkspaceFileSearchRequest(query: query, catalogRevision: model.workspaceFilesRevision)
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

private struct WorkspaceFileSearchRequest: Equatable {
    let query: String
    let catalogRevision: Int
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
            Text(node.name)
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
        model.sessionFiles.filter { $0.origin == .agent }
    }

    private var userFiles: [SessionFileRecord] {
        model.sessionFiles.filter { $0.origin == .user }
    }

    var body: some View {
        List {
            fileSection(
                "Agent files",
                records: agentFiles,
                emptyGlyph: .robot,
                accessibilityOrigin: "agent"
            )
            fileSection(
                "User uploads",
                records: userFiles,
                emptyGlyph: .fileUpload,
                accessibilityOrigin: "user-uploaded"
            )
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
    }

    private func fileSection(
        _ title: String,
        records: [SessionFileRecord],
        emptyGlyph: MobiusGlyph,
        accessibilityOrigin: String
    ) -> some View {
        Section(title) {
            if model.isLoadingSessionFiles {
                InspectorFileLoadingRows(title: "Loading \(title.lowercased())")
            } else if records.isEmpty {
                InspectorEmptyRow(title: "No \(title.lowercased())", glyph: emptyGlyph)
            } else {
                ForEach(records) { record in
                    SessionFileInspectorRow(
                        file: record.file,
                        accessibilityLabel: "Open \(accessibilityOrigin) file \(record.file.name)"
                    )
                }
            }
        }
    }
}

private struct SessionFileInspectorRow: View {
    @Environment(AppModel.self) private var model
    let file: SessionFileReference
    let accessibilityLabel: String

    var body: some View {
        HStack(spacing: 0) {
            Button {
                model.previewSessionFile(file)
            } label: {
                InspectorFileRow(
                    name: file.name,
                    detail: file.mediaType,
                    size: file.size,
                    showsDisclosure: false
                )
            }
            .accessibilityLabel(accessibilityLabel)

            Menu {
                Button("Preview", glyph: file.name.fileGlyph) {
                    model.previewSessionFile(file)
                }
                Button("Share or Save…", glyph: .arrowUpRight01) {
                    model.saveOrShareSessionFile(file)
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
    let title: String

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
                        .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                }
            }
        }
        .mobiusLoadingPlaceholder(title)
        .inspectorFileListRow()
    }
}

private struct InspectorEmptyRow: View {
    @Environment(\.mobiusPalette) private var palette
    let title: String
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
            MobiusIcon(name.fileGlyph, foreground: palette.accent)
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                Text(name)
                    .font(MobiusStyle.metadataFont.weight(.semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(detail)
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
