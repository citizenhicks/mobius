import SwiftUI

struct WorkspaceBrowserView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @State private var newFolderName = ""
    @State private var showsNewFolderPrompt = false

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                if let listing = model.directoryListing {
                    DirectoryBrowserHeader(
                        path: listing.path,
                        title: "Choose a workspace for the new chat",
                        parent: listing.parent,
                        onParent: model.loadDirectory,
                        onCreateFolder: {
                            newFolderName = ""
                            showsNewFolderPrompt = true
                        }
                    )
                    List {
                        ForEach(listing.entries) { entry in
                            Button { model.loadDirectory(entry.path) } label: {
                                MobiusLabel(title: entry.name, glyph: .folder)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .contentShape(Rectangle())
                            }
                            .buttonStyle(.mobiusPlain)
                            .listRowSeparator(.hidden)
                        }
                        if listing.entries.isEmpty && !model.isLoadingDirectories {
                            Text("No folders")
                                .foregroundStyle(palette.muted)
                                .listRowSeparator(.hidden)
                        }
                        if let error = model.directoryError ?? model.workspaceError {
                            MobiusLabel(
                                title: error,
                                glyph: .warning,
                                iconColor: palette.danger
                            )
                                .foregroundStyle(palette.danger)
                                .listRowSeparator(.hidden)
                        }
                    }
                    .listStyle(.plain)
                    .scrollContentBackground(.hidden)
                }
            }
            .font(MobiusStyle.bodyFont)
            .disabled(model.isLoadingDirectories || model.isChangingWorkspace)
            .overlay {
                if model.isLoadingDirectories { ProgressView() }
            }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") {
                        model.showsWorkspaceBrowser = false
                        dismiss()
                    }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Choose") {
                        if let path = model.directoryListing?.path { model.chooseWorkspace(path) }
                    }
                    .disabled(
                        model.directoryListing?.parent == nil
                            || model.isLoadingDirectories
                            || model.isChangingWorkspace
                    )
                }
            }
        }
        .alert("New folder", isPresented: $showsNewFolderPrompt) {
            TextField("Folder name", text: $newFolderName)
            Button("Cancel", role: .cancel) {}
            Button("Create") {
                model.createWorkspaceDirectory(named: newFolderName)
            }
            .disabled(newFolderName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        } message: {
            Text("Create a folder inside \(model.directoryListing?.path ?? "this location").")
        }
    }
}

private struct DirectoryBrowserHeader: View {
    @Environment(\.mobiusPalette) private var palette
    let path: String
    let title: String
    let parent: String?
    let onParent: (String) -> Void
    let onCreateFolder: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            Text(path)
                .font(MobiusStyle.metadataFont.weight(.bold))
                .tracking(1)
                .foregroundStyle(palette.accent)
                .lineLimit(2)
            HStack {
                Text(title)
                    .font(MobiusStyle.controlFont)
                Spacer()
                Button("New folder", glyph: .folderPlus, action: onCreateFolder)
                    .mobiusIconButton()
                    .help("New folder")
                if let parent {
                    Button("Parent folder", glyph: .arrowUp) { onParent(parent) }
                        .mobiusIconButton()
                        .help("Parent folder")
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.vertical, MobiusSpace.s)
    }
}

struct FrontendContributionPage: View {
    @Environment(AppModel.self) private var model
    let widget: MountedWidget

    var body: some View {
        PageScaffold(title: widget.title, detail: detail) {
            if !model.isCapabilityEnabled(widget.capability) {
                DisabledCapabilityNotice(
                    title: "\(widget.widget.text) is off",
                    detail: "Saved content remains visible. Enable \(widget.widget.text) in this chat to make changes."
                )
            }
            if let content = widget.widget.content {
                Section {
                    FrontendWidgetContentView(
                        content: content,
                        actionsEnabled: model.isCapabilityEnabled(widget.capability),
                        usesSwipeActions: true
                    ) { option in
                        model.submitPickerOption(option)
                    }
                }
            } else if widget.widget.action != nil {
                Section {
                    Button(
                        widget.widget.text,
                        glyph: widget.glyph,
                        action: { model.submitWidget(widget) }
                    )
                }
            } else {
                MobiusUnavailable(
                    title: widget.widget.text,
                    glyph: widget.glyph,
                    detail: "No content is currently available."
                )
            }
        }
    }

    private var detail: String {
        if case .actionList? = widget.widget.content { return "" }
        return widget.widget.text == widget.title ? "" : widget.widget.text
    }
}

struct ScratchpadView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if let widget = model.globalScratchpadWidget,
               let content = widget.widget.content {
                PageScaffold(title: widget.title, detail: "") {
                    Section {
                        FrontendWidgetContentView(
                            content: content,
                            actionsEnabled: model.connectionState.isReady,
                            usesSwipeActions: true,
                            submitOperation: model.submitGlobalScratchpadOperation
                        ) { _ in }
                    }
                }
            } else {
                MobiusUnavailable(
                    title: "Global Scratchpad unavailable",
                    glyph: .brain,
                    detail: "Connect to a gateway to load it."
                )
                .navigationTitle("Scratchpad")
                .background(MobiusBackdrop())
            }
        }
        .task(id: model.connectionState.isReady) {
            if model.connectionState.isReady { model.refreshGlobalScratchpad() }
        }
    }
}
