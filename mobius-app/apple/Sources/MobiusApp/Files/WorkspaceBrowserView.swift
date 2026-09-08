import SwiftUI

struct WorkspaceBrowserView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var goingBack = false
    @State private var chosenPath: String?
    @State private var newFolderName = ""
    @State private var showsNewFolderPrompt = false
    var title: LocalizedStringResource = "Choose a workspace for the new chat"
    var onChoose: ((String) -> Void)? = nil

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                if let listing = model.directoryListing {
                    DirectoryBrowserHeader(
                        path: listing.path,
                        title: title,
                        parent: listing.parent,
                        onParent: { path in
                            goingBack = true
                            model.loadDirectory(path)
                        },
                        onCreateFolder: {
                            newFolderName = ""
                            showsNewFolderPrompt = true
                        }
                    )
                    if chosenPath == nil {
                        List {
                            ForEach(listing.entries) { entry in
                                Button {
                                    goingBack = false
                                    model.loadDirectory(entry.path)
                                } label: {
                                    MobiusLabel(verbatim: entry.name, glyph: .folder)
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
                            if let error = model.directoryError
                                ?? (onChoose == nil ? model.workspaceError : nil)
                            {
                                MobiusLabel(
                                    verbatim: error,
                                    glyph: .warning,
                                    iconColor: palette.danger
                                )
                                .foregroundStyle(palette.danger)
                                .listRowSeparator(.hidden)
                            }
                        }
                        .listStyle(.plain)
                        .scrollContentBackground(.hidden)
                        .id(listing.path)
                        .transition(
                            reduceMotion
                                ? .opacity
                                : .asymmetric(
                                    insertion: .move(edge: goingBack ? .leading : .trailing)
                                        .combined(with: .opacity),
                                    removal: .move(edge: goingBack ? .trailing : .leading).combined(
                                        with: .opacity)))
                    }
                }
            }
            .clipped()
            .animation(
                reduceMotion ? nil : .smooth(duration: 0.28), value: model.directoryListing?.path
            )
            .font(MobiusStyle.bodyFont)
            .disabled(model.isLoadingDirectories || model.isChangingWorkspace || chosenPath != nil)
            .overlay {
                if model.isLoadingDirectories { ProgressView() }
            }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") {
                        if onChoose == nil { model.showsWorkspaceBrowser = false }
                        dismiss()
                    }
                    .disabled(chosenPath != nil)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Choose", action: choose)
                        .disabled(
                            model.directoryListing?.parent == nil
                                || model.isLoadingDirectories
                                || model.isChangingWorkspace
                                || chosenPath != nil
                        )
                }
            }
        }
        .interactiveDismissDisabled(chosenPath != nil)
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
    private func choose() {
        guard let path = model.directoryListing?.path,
            !model.isLoadingDirectories, !model.isChangingWorkspace, chosenPath == nil
        else { return }
        let finish = {
            if let onChoose {
                onChoose(path)
                dismiss()
            } else {
                model.chooseWorkspace(path)
                chosenPath = nil
            }
        }
        if reduceMotion {
            finish()
        } else {
            withAnimation(.smooth(duration: 0.22)) {
                chosenPath = path
            } completion: {
                finish()
            }
        }
    }

}

private struct DirectoryBrowserHeader: View {
    @Environment(\.mobiusPalette) private var palette
    let path: String
    let title: LocalizedStringResource
    let parent: String?
    let onParent: (String) -> Void
    let onCreateFolder: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            HStack(spacing: MobiusSpace.m) {
                MobiusIcon(.folderOpen, size: 40, foreground: palette.accent)
                    .padding(MobiusSpace.m)
                    .background(palette.accentSoft, in: .rect(cornerRadius: 16))
                Text(verbatim: path)
                    .font(MobiusStyle.controlFont)
                    .foregroundStyle(palette.accent)
                    .lineLimit(3)
                    .truncationMode(.middle)
                    .contentTransition(.opacity)
            }
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
        PageScaffold(
            title: .localized(frontendPresentationText(widget.title)),
            detail: .localized(frontendPresentationText(detail))
        ) {
            if !model.isCapabilityEnabled(widget.capability) {
                let name = frontendPresentationText(widget.widget.text)
                DisabledCapabilityNotice(
                    title: "\(name) is off",
                    detail:
                        "Saved content remains visible. Enable \(name) in this chat to make changes."
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
                        frontendPresentationText(widget.widget.text),
                        glyph: widget.glyph,
                        action: { model.submitWidget(widget) }
                    )
                }
            } else {
                MobiusUnavailable(
                    title: frontendPresentationText(widget.widget.text),
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

struct GlobalContributionsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let widgets = model.navigationWidgets(in: .global)
        let title: MobiusText =
            widgets.first.map {
                .localized(frontendPresentationText($0.title))
            } ?? .localized("Scratchpad")
        PageScaffold(title: title, detail: .verbatim("")) {
            if !widgets.isEmpty {
                ForEach(widgets) { widget in
                    if let content = widget.widget.content {
                        Section {
                            FrontendWidgetContentView(
                                content: content,
                                actionsEnabled: model.gateway.connectionState.isReady,
                                usesSwipeActions: true,
                                submitOperation: { operation in
                                    model.submitContributionOperation(operation, scope: .global)
                                }
                            ) { option in
                                model.submitContributionOperation(option.op, scope: .global)
                            }
                        }
                    }
                }
            } else {
                MobiusUnavailable(
                    title: "Global Scratchpad unavailable",
                    glyph: .brain,
                    detail: "Connect to a gateway to load it."
                )
            }
        }
        .task(id: model.gateway.connectionState.isReady) {
            if model.gateway.connectionState.isReady { model.refreshContributions(scope: .global) }
        }
    }
}
