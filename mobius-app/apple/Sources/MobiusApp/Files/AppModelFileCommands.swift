import Foundation
import Observation

extension AppModel {
    func openWorkspaceBrowser() {
        guard canCreateSession else { return }
        showsWorkspaceBrowser = true
        loadDirectory(
            chat.pendingNewChatWorkspace ?? workspace?.path ?? ".")
    }

    func loadDirectory(_ path: String) {
        let id = requestID("directories")
        directoryRequestID = id
        directoryError = nil
        isLoadingDirectories = true
        gateway.transmit(.listDirectories(requestID: id, path: path, includeFiles: false)) {
            [weak self] message in
            guard self?.directoryRequestID == id else { return }
            self?.directoryRequestID = nil
            self?.isLoadingDirectories = false
            self?.directoryError = message
        }
    }

    func createWorkspaceDirectory(named rawName: String) {
        let name = rawName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else {
            directoryError = localizedString("Enter a folder name.")
            return
        }
        guard name != ".", name != "..", !name.contains("/"), !name.contains("\\") else {
            directoryError = localizedString("Enter a single folder name.")
            return
        }
        guard let parent = directoryListing?.path, canCreateSession else { return }
        let id = requestID("create-directory")
        directoryRequestID = id
        directoryError = nil
        isLoadingDirectories = true
        gateway.transmit(.createWorkspaceDirectory(requestID: id, parent: parent, name: name)) {
            [weak self] message in
            guard self?.directoryRequestID == id else { return }
            self?.directoryRequestID = nil
            self?.isLoadingDirectories = false
            self?.directoryError = message
        }
    }

    func refreshWorkspaceChanges() {
        refreshGitDiff()
        if showsInspector, filesInspectorTab == .modified {
            if let scope = modifiedFilesScope.gitScope, scope != .unstaged {
                refreshGitDiff(scope)
            }
        }
        refreshWorkspaceFiles()
    }

    func refreshGitDiff(_ scope: GitDiffScope = .unstaged) {
        guard gateway.connectionState.isReady, let sessionID = chat.selectedSessionID else {
            return
        }
        let id = requestID("git-diff")
        gitDiffs[scope, default: GitDiffState()].requestID = id
        gateway.transmit(.getGitDiff(requestID: id, sessionID: sessionID, scope: scope)) {
            [weak self] _ in
            guard self?.gitDiffs[scope]?.requestID == id else { return }
            self?.gitDiffs[scope]?.requestID = nil
        }
    }

    func selectFilesInspectorTab(_ tab: FilesInspectorTab) {
        guard filesInspectorTab != tab else { return }
        filesInspectorTab = tab
        refreshFiles(for: tab)
    }

    func selectModifiedFilesScope(_ scope: ModifiedFilesScope) {
        guard modifiedFilesScope != scope else { return }
        modifiedFilesScope = scope
        refreshModifiedFiles(scope)
    }

    func refreshWorkspaceFiles() {
        guard gateway.connectionState.isReady,
            let sessionID = chat.selectedSessionID
        else { return }
        let id = requestID("workspace-files")
        workspaceFilesRequestID = id
        workspaceFilesTruncated = false
        isLoadingWorkspaceFiles = true
        gateway.transmit(
            .listWorkspaceFiles(
                requestID: id,
                sessionID: sessionID,
                scope: .all
            )
        ) { [weak self] _ in
            guard self?.workspaceFilesRequestID == id else { return }
            self?.workspaceFilesRequestID = nil
            self?.isLoadingWorkspaceFiles = false
        }
    }

    func switchGitBranch(to branch: String) {
        guard canModifySelectedSession,
            let sessionID = chat.selectedSessionID,
            let gitStatus,
            branch != gitStatus.currentBranch,
            gitStatus.branches.contains(branch)
        else { return }
        let id = requestID("git-branch")
        gitBranchRequestID = id
        gateway.transmit(.switchGitBranch(requestID: id, sessionID: sessionID, branch: branch)) {
            [weak self] _ in
            if self?.gitBranchRequestID == id { self?.gitBranchRequestID = nil }
        }
    }

    func previewSessionFile(_ file: SessionFileReference, sessionID: String?) {
        downloadSessionFile(file, sessionID: sessionID, purpose: .preview)
    }

    func saveOrShareSessionFile(_ file: SessionFileReference, sessionID: String?) {
        downloadSessionFile(file, sessionID: sessionID, purpose: .share)
    }

    private func downloadSessionFile(
        _ file: SessionFileReference,
        sessionID: String?,
        purpose: SessionFileDownloadPurpose
    ) {
        guard let sessionID else { return }
        guard file.size <= Int64(maximumPresentedFileBytes) else {
            showToast("File downloads are limited to 50 MiB.", tone: .warning)
            return
        }
        discardFilePresentation()
        returnsToFilesAfterFilePresentation = showsInspector
        let id = requestID("session-file-read")
        let generation = UUID()
        filePresentationGeneration = generation
        chat.sessionFileDownload = SessionFileDownload(
            generation: generation,
            file: file,
            sessionID: sessionID,
            purpose: purpose,
            data: Data(),
            requestID: id
        )
        isLoadingFilePresentation = true
        gateway.transmit(
            .readSessionFile(
                requestID: id,
                sessionID: sessionID,
                fileID: file.id,
                offset: 0,
                maxBytes: 256 * 1024
            )
        ) { [weak self] message in
            guard self?.chat.sessionFileDownload?.requestID == id else { return }
            self?.chat.sessionFileDownload = nil
            self?.isLoadingFilePresentation = false
            self?.showToast(verbatim: message, tone: .error)
        }
    }

    func workspaceFile(for link: URL) -> WorkspaceFileRecord? {
        let scheme = link.scheme?.lowercased()
        if let scheme, !["file", "sandbox", "workspace"].contains(scheme) { return nil }
        var path = link.path
        if let root = workspace?.path {
            let prefix = root.hasSuffix("/") ? root : "\(root)/"
            if path.hasPrefix(prefix) { path = String(path.dropFirst(prefix.count)) }
        }
        if scheme == "sandbox", path.hasPrefix("/mnt/data/") {
            path = String(path.dropFirst("/mnt/data/".count))
        }
        if scheme == "workspace" { path = String(path.drop(while: { $0 == "/" })) }
        while path.hasPrefix("./") { path.removeFirst(2) }
        guard !path.isEmpty, !path.hasPrefix("/") else { return nil }
        return workspaceFiles.first { $0.path == path }
    }

    func previewWorkspaceFile(_ file: WorkspaceFileRecord) {
        guard let sessionID = chat.selectedSessionID else { return }
        guard file.size <= UInt64(maximumPresentedFileBytes) else {
            showToast("Quick Look previews are limited to 50 MiB.", tone: .warning)
            return
        }
        discardFilePresentation()
        returnsToFilesAfterFilePresentation = showsInspector
        let id = requestID("workspace-file-read")
        let generation = UUID()
        filePresentationGeneration = generation
        workspaceFilePreviewDownload = WorkspaceFilePreviewDownload(
            generation: generation,
            file: file,
            sessionID: sessionID,
            data: Data(),
            requestID: id
        )
        isLoadingFilePresentation = true
        gateway.transmit(
            .readWorkspaceFile(
                requestID: id,
                sessionID: sessionID,
                path: file.path,
                offset: 0,
                maxBytes: 256 * 1024
            )
        ) { [weak self] message in
            guard self?.workspaceFilePreviewDownload?.requestID == id else { return }
            self?.workspaceFilePreviewDownload = nil
            self?.isLoadingFilePresentation = false
            self?.showToast(verbatim: message, tone: .error)
        }
    }

    func createWorkspaceFile() {
        guard canOpenSession, let sessionID = chat.selectedSessionID else { return }
        discardFilePresentation()
        returnsToFilesAfterFilePresentation = showsInspector
        revealFilePresentation()
        let id = UUID()
        filePresentationGeneration = id
        textFilePreview = TextFilePreview(
            id: id,
            name: "New File",
            contents: "",
            workspaceSessionID: sessionID,
            workspacePath: ""
        )
    }

    func saveWorkspaceFile(sessionID: String, path: String, content: String) {
        guard canModifySelectedSession,
            chat.selectedSessionID == sessionID,
            workspaceFileWriteRequestID == nil,
            path.utf8.count <= 4_096,
            !path.isEmpty,
            content.utf8.count <= maximumWorkspaceTextFileBytes
        else { return }
        let id = requestID("workspace-file-write")
        workspaceFileWriteRequestID = id
        isSavingWorkspaceFile = true
        gateway.transmit(
            .writeWorkspaceFile(
                requestID: id,
                sessionID: sessionID,
                path: path,
                content: content
            )
        ) { [weak self] message in
            guard self?.workspaceFileWriteRequestID == id else { return }
            self?.workspaceFileWriteRequestID = nil
            self?.isSavingWorkspaceFile = false
            self?.showToast(verbatim: message, tone: .error)
        }
    }

    func updateWorkspaceFileDraft(id: UUID, path: String) {
        guard var draft = textFilePreview,
            draft.id == id,
            draft.workspaceSessionID != nil,
            draft.workspacePath != nil
        else { return }
        draft.workspacePath = path
        textFilePreview = draft
    }

    func updateWorkspaceFileDraft(id: UUID, contents: String) {
        guard var draft = textFilePreview,
            draft.id == id,
            draft.workspaceSessionID != nil,
            draft.workspacePath != nil
        else { return }
        draft.contents = contents
        textFilePreview = draft
    }

    func discardFilePresentation(preservingWorkspaceTextDraft: Bool = false) {
        filePresentationGeneration = UUID()
        chat.sessionFileDownload = nil
        workspaceFilePreviewDownload = nil
        isLoadingFilePresentation = false
        if let previewTemporaryDirectory {
            Task.detached(priority: .utility) {
                try? FileManager.default.removeItem(at: previewTemporaryDirectory)
            }
        }
        previewTemporaryDirectory = nil
        previewURL = nil
        if !preservingWorkspaceTextDraft || textFilePreview?.workspaceSessionID == nil {
            textFilePreview = nil
        }
        sessionFileShareItem = nil
        if textFilePreview == nil { returnsToFilesAfterFilePresentation = false }
    }

    func closeFilePresentation() {
        let returnsToFiles = returnsToFilesAfterFilePresentation
        discardFilePresentation()
        if returnsToFiles { showsInspector = true }
    }

    func revealFilePresentation() {
        if returnsToFilesAfterFilePresentation { showsInspector = false }
    }

    func showFiles(_ tab: FilesInspectorTab? = nil) {
        if let tab { filesInspectorTab = tab }
        showsInspector = true
        refreshFiles(for: filesInspectorTab)
    }

    func showFiles(_ scope: ModifiedFilesScope) {
        filesInspectorTab = .modified
        modifiedFilesScope = scope
        showsInspector = true
        refreshModifiedFiles(scope)
    }

    func refreshFiles(for tab: FilesInspectorTab) {
        switch tab {
        case .modified: refreshModifiedFiles(modifiedFilesScope)
        case .allFiles: refreshWorkspaceFiles()
        case .chatFiles: chat.refreshSessionFiles()
        }
    }

    func refreshModifiedFiles(_ scope: ModifiedFilesScope) {
        if let scope = scope.gitScope { refreshGitDiff(scope) }
    }
}
