import Foundation

extension AppModel {
    func handleSessionFileChunk(
        requestID: String,
        sessionID: String,
        fileID: String,
        offset: Int64,
        data: Data,
        nextOffset: Int64?
    ) {
        if chat.discardedSessionFileThumbnailRequestIDs.remove(requestID) != nil { return }
        guard chat.sessionFileDownload?.requestID == requestID else {
            chat.handleSessionFileThumbnailChunk(
                requestID: requestID,
                sessionID: sessionID,
                fileID: fileID,
                offset: offset,
                data: data,
                nextOffset: nextOffset
            )
            return
        }
        guard var download = chat.sessionFileDownload,
            download.requestID == requestID
        else { return }
        chat.sessionFileDownload = nil
        guard download.sessionID == sessionID,
            download.file.id == fileID,
            offset == Int64(download.data.count),
            data.count <= 256 * 1024,
            Int64(download.data.count + data.count) <= download.file.size
        else {
            isLoadingFilePresentation = false
            showToast("The gateway returned an invalid session file.", tone: .error)
            return
        }
        download.data.append(data)
        if let nextOffset {
            guard nextOffset == Int64(download.data.count), nextOffset > offset else {
                isLoadingFilePresentation = false
                showToast("The gateway returned an invalid session file offset.", tone: .error)
                return
            }
            let id = self.requestID("session-file-read")
            download.requestID = id
            chat.sessionFileDownload = download
            gateway.transmit(
                .readSessionFile(
                    requestID: id,
                    sessionID: sessionID,
                    fileID: fileID,
                    offset: nextOffset,
                    maxBytes: 256 * 1024
                )
            ) { [weak self] message in
                guard self?.chat.sessionFileDownload?.requestID == id else { return }
                self?.chat.sessionFileDownload = nil
                self?.isLoadingFilePresentation = false
                self?.showToast(verbatim: message, tone: .error)
            }
            return
        }

        guard Int64(download.data.count) == download.file.size else {
            isLoadingFilePresentation = false
            showToast("The downloaded file is incomplete.", tone: .error)
            return
        }
        finishFilePresentation(
            download.data,
            name: download.file.name,
            generation: download.generation,
            purpose: download.purpose,
            allowsTextPreview: !download.file.mediaType.lowercased().hasPrefix("image/")
        )
    }

    func handleWorkspaceFileChunk(
        requestID: String,
        sessionID: String,
        path: String,
        offset: UInt64,
        data: Data,
        nextOffset: UInt64?
    ) {
        guard var download = workspaceFilePreviewDownload,
            download.requestID == requestID
        else { return }
        workspaceFilePreviewDownload = nil
        guard download.sessionID == sessionID,
            download.file.path == path,
            offset == UInt64(download.data.count),
            data.count <= 256 * 1024,
            offset <= download.file.size,
            UInt64(data.count) <= download.file.size - offset
        else {
            isLoadingFilePresentation = false
            showToast("The gateway returned an invalid workspace file.", tone: .error)
            return
        }
        download.data.append(data)
        if let nextOffset {
            guard nextOffset == UInt64(download.data.count), nextOffset > offset else {
                isLoadingFilePresentation = false
                showToast("The gateway returned an invalid workspace file offset.", tone: .error)
                return
            }
            let id = self.requestID("workspace-file-read")
            download.requestID = id
            workspaceFilePreviewDownload = download
            gateway.transmit(
                .readWorkspaceFile(
                    requestID: id,
                    sessionID: sessionID,
                    path: path,
                    offset: nextOffset,
                    maxBytes: 256 * 1024
                )
            ) { [weak self] message in
                guard self?.workspaceFilePreviewDownload?.requestID == id else { return }
                self?.workspaceFilePreviewDownload = nil
                self?.isLoadingFilePresentation = false
                self?.showToast(verbatim: message, tone: .error)
            }
            return
        }

        guard UInt64(download.data.count) == download.file.size else {
            isLoadingFilePresentation = false
            showToast("The downloaded workspace file is incomplete.", tone: .error)
            return
        }
        finishFilePresentation(
            download.data,
            name: URL(fileURLWithPath: download.file.path).lastPathComponent,
            generation: download.generation,
            purpose: .preview,
            allowsTextPreview: true,
            workspaceSessionID: download.sessionID,
            workspacePath: download.file.path
        )
    }

    private func finishFilePresentation(
        _ data: Data,
        name: String,
        generation: UUID,
        purpose: SessionFileDownloadPurpose,
        allowsTextPreview: Bool,
        workspaceSessionID: String? = nil,
        workspacePath: String? = nil
    ) {
        Task { [weak self] in
            if purpose == .preview, allowsTextPreview {
                let contents = await Self.utf8Text(in: data)
                guard let self, self.filePresentationGeneration == generation else { return }
                if let contents {
                    self.revealFilePresentation()
                    self.textFilePreview = TextFilePreview(
                        id: generation,
                        name: name,
                        contents: contents,
                        workspaceSessionID: workspaceSessionID,
                        workspacePath: workspacePath
                    )
                    self.isLoadingFilePresentation = false
                    return
                }
            }
            do {
                let file = try await Self.writeTemporarySessionFile(data, name: name)
                guard let self else {
                    await Self.removePreviewDirectory(file.directory)
                    return
                }
                guard self.filePresentationGeneration == generation else {
                    await Self.removePreviewDirectory(file.directory)
                    return
                }
                let previousDirectory = self.previewTemporaryDirectory
                self.previewTemporaryDirectory = file.directory
                self.revealFilePresentation()
                if purpose == .share {
                    self.sessionFileShareItem = SessionFileShareItem(
                        id: generation,
                        name: name,
                        url: file.url
                    )
                } else {
                    self.previewURL = file.url
                }
                self.isLoadingFilePresentation = false
                if let previousDirectory {
                    Task { await Self.removePreviewDirectory(previousDirectory) }
                }
            } catch {
                guard let self, self.filePresentationGeneration == generation else { return }
                self.isLoadingFilePresentation = false
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
            }
        }
    }

    nonisolated static func utf8Text(in data: Data) async -> String? {
        guard data.count <= maximumHighlightedPreviewBytes else { return nil }
        return await Task.detached(priority: .userInitiated) {
            guard let text = String(data: data, encoding: .utf8) else { return nil }
            let allowedControls: Set<Unicode.Scalar> = ["\t", "\n", "\r"]
            guard
                !text.unicodeScalars.contains(where: {
                    CharacterSet.controlCharacters.contains($0) && !allowedControls.contains($0)
                })
            else { return nil }
            return text
        }.value
    }

    nonisolated static func writeTemporarySessionFile(
        _ data: Data,
        name: String
    ) async throws -> TemporarySessionFile {
        try await Task.detached(priority: .userInitiated) {
            let directory = URL.temporaryDirectory.appending(
                path: UUID().uuidString, directoryHint: .isDirectory)
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
            let candidateExtension = URL(fileURLWithPath: name).pathExtension
            let safeExtension =
                candidateExtension.utf8.count <= 16
                    && candidateExtension.unicodeScalars.allSatisfy(
                        CharacterSet.alphanumerics.contains)
                ? candidateExtension
                : ""
            let candidateName = URL(fileURLWithPath: name).lastPathComponent
            let safeName =
                candidateName.utf8.count <= 255
                    && candidateName != "."
                    && candidateName != ".."
                    && !candidateName.unicodeScalars.contains(where: {
                        CharacterSet.controlCharacters.contains($0) || $0 == "/" || $0 == "\\"
                            || $0 == ":"
                    })
                ? candidateName
                : ""
            let url: URL
            if !safeName.isEmpty {
                url = directory.appending(path: safeName)
            } else if safeExtension.isEmpty {
                url = directory.appending(path: "file")
            } else {
                url = directory.appending(path: "file").appendingPathExtension(safeExtension)
            }
            try data.write(to: url, options: [.atomic, .completeFileProtection])
            return TemporarySessionFile(directory: directory, url: url)
        }.value
    }

    nonisolated static func removePreviewDirectory(_ directory: URL) async {
        await Task.detached(priority: .utility) {
            try? FileManager.default.removeItem(at: directory)
        }.value
    }
}
