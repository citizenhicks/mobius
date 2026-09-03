@preconcurrency import AVFoundation
import Foundation
import ImageIO
import UniformTypeIdentifiers

private let maximumFileThumbnailSourceBytes: Int64 = 10 * 1024 * 1024
private let maximumCachedFileThumbnails = 32
private let maximumDiscardedFileThumbnailRequestIDs = 32
private let maximumFileThumbnailPixelDimension = 384

extension AppModel {
    nonisolated static func loadImportedAttachment(
        _ url: URL,
        maximumBytes: Int
    ) async throws -> ImportedAttachmentData {
        try await Task.detached(priority: .userInitiated) {
            let accessed = url.startAccessingSecurityScopedResource()
            defer { if accessed { url.stopAccessingSecurityScopedResource() } }

            let values = try url.resourceValues(forKeys: [
                .isRegularFileKey,
                .fileSizeKey,
                .contentTypeKey,
            ])
            guard values.isRegularFile == true else { throw AttachmentImportError.notAFile }
            if let size = values.fileSize, size > maximumBytes {
                throw AttachmentImportError.tooLarge(Int64(maximumBytes))
            }
            let data = try Data(contentsOf: url)
            guard data.count <= maximumBytes else {
                throw AttachmentImportError.tooLarge(Int64(maximumBytes))
            }
            if let size = values.fileSize, size != data.count {
                throw AttachmentImportError.changedWhileReading
            }
            let mediaType = values.contentType?.preferredMIMEType
                ?? UTType(filenameExtension: url.pathExtension)?.preferredMIMEType
                ?? "application/octet-stream"
            let thumbnail: CGImage? = if mediaType.lowercased().hasPrefix("video/") {
                await Self.videoFileThumbnail(from: url)
            } else if Self.isFileThumbnailCandidate(
                mediaType: mediaType,
                size: Int64(data.count)
            ) {
                await Self.downsampledFileThumbnail(from: data)
            } else {
                nil
            }
            return ImportedAttachmentData(
                name: url.lastPathComponent,
                mediaType: mediaType,
                data: data,
                thumbnail: thumbnail
            )
        }.value
    }

    nonisolated static func isFileThumbnailCandidate(mediaType: String, size: Int64) -> Bool {
        size >= 0
            && size <= maximumFileThumbnailSourceBytes
            && mediaType.lowercased().hasPrefix("image/")
    }

    nonisolated static func supportsFileThumbnail(mediaType: String) -> Bool {
        let mediaType = mediaType.lowercased()
        return mediaType.hasPrefix("image/") || mediaType.hasPrefix("video/")
    }

    nonisolated static func videoFileThumbnail(from url: URL) async -> CGImage? {
        let generator = AVAssetImageGenerator(asset: AVURLAsset(url: url))
        generator.appliesPreferredTrackTransform = true
        let maximumDimension = CGFloat(maximumFileThumbnailPixelDimension)
        generator.maximumSize = CGSize(
            width: maximumDimension,
            height: maximumDimension
        )
        return try? await generator.image(at: .zero).image
    }

    nonisolated static func downsampledFileThumbnail(from data: Data) async -> CGImage? {
        guard Int64(data.count) <= maximumFileThumbnailSourceBytes else { return nil }
        return await Task.detached(priority: .utility) {
            let sourceOptions = [kCGImageSourceShouldCache: false] as CFDictionary
            guard let source = CGImageSourceCreateWithData(data as CFData, sourceOptions) else {
                return nil
            }
            let thumbnailOptions: [CFString: Any] = [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceThumbnailMaxPixelSize: maximumFileThumbnailPixelDimension,
                kCGImageSourceShouldCacheImmediately: true,
            ]
            return CGImageSourceCreateThumbnailAtIndex(
                source,
                0,
                thumbnailOptions as CFDictionary
            )
        }.value
    }

    nonisolated static func encodedFileThumbnail(_ image: CGImage) async -> Data? {
        await Task.detached(priority: .utility) {
            let data = NSMutableData()
            guard
                let destination = CGImageDestinationCreateWithData(
                    data,
                    UTType.png.identifier as CFString,
                    1,
                    nil
                )
            else { return nil }
            CGImageDestinationAddImage(destination, image, nil)
            guard CGImageDestinationFinalize(destination) else { return nil }
            return data as Data
        }.value
    }

    func fileThumbnail(for file: SessionFileReference, sessionID: String?) -> CGImage? {
        guard let sessionID else { return nil }
        return fileThumbnails[.session(sessionID: sessionID, fileID: file.id)]
    }

    func fileThumbnail(for attachment: ComposerAttachment) -> CGImage? {
        if case .uploaded(let file) = attachment.state {
            return fileThumbnail(for: file, sessionID: selectedSessionID)
        }
        return fileThumbnails[.composer(attachment.id)]
    }

    func cacheFileThumbnail(_ thumbnail: CGImage, for key: FileThumbnailKey) {
        if fileThumbnails[key] == nil {
            while fileThumbnailOrder.count >= maximumCachedFileThumbnails,
                  let oldest = fileThumbnailOrder.first {
                removeFileThumbnail(for: oldest)
            }
            fileThumbnailOrder.append(key)
        }
        fileThumbnails[key] = thumbnail
    }

    func persistFileThumbnail(_ thumbnail: CGImage, sessionID: String, fileID: String) {
        guard !isClearingLocalData, let accountID = selectedAccountID else { return }
        enqueueTranscriptIO { [store] in
            guard let data = await Self.encodedFileThumbnail(thumbnail) else { return }
            await store.saveThumbnail(
                data,
                accountID: accountID,
                sessionID: sessionID,
                fileID: fileID
            )
        }
    }

    func removeFileThumbnail(for key: FileThumbnailKey) {
        fileThumbnails[key] = nil
        fileThumbnailOrder.removeAll { $0 == key }
    }

    func promoteFileThumbnail(
        localID: UUID,
        sessionID: String,
        fileID: String
    ) {
        let localKey = FileThumbnailKey.composer(localID)
        guard let thumbnail = fileThumbnails[localKey] else { return }
        removeFileThumbnail(for: localKey)
        cacheFileThumbnail(
            thumbnail,
            for: .session(sessionID: sessionID, fileID: fileID)
        )
        persistFileThumbnail(thumbnail, sessionID: sessionID, fileID: fileID)
    }

    func requestSessionFileThumbnail(_ file: SessionFileReference, sessionID: String?) {
        guard !isClearingLocalData,
              let sessionID,
              Self.supportsFileThumbnail(mediaType: file.mediaType),
              fileThumbnails[.session(sessionID: sessionID, fileID: file.id)] == nil
        else { return }
        let key = FileThumbnailKey.session(sessionID: sessionID, fileID: file.id)
        let canDownloadSource = Self.isFileThumbnailCandidate(
            mediaType: file.mediaType,
            size: file.size
        )
        guard requestedSessionFileThumbnailKeys.insert(key).inserted else { return }
        guard let accountID = selectedAccountID else {
            if canDownloadSource {
                queueSessionFileThumbnail(file, sessionID: sessionID, key: key)
            } else {
                requestedSessionFileThumbnailKeys.remove(key)
            }
            return
        }
        Task { [weak self, store] in
            let data = await store.loadThumbnail(
                accountID: accountID,
                sessionID: sessionID,
                fileID: file.id
            )
            let thumbnail: CGImage? =
                if let data {
                    await Self.downsampledFileThumbnail(from: data)
                } else {
                    nil
                }
            guard let self,
                  self.selectedAccountID == accountID,
                  self.requestedSessionFileThumbnailKeys.contains(key)
            else { return }
            if let thumbnail {
                self.requestedSessionFileThumbnailKeys.remove(key)
                self.cacheFileThumbnail(thumbnail, for: key)
                return
            }
            if data != nil {
                await store.removeThumbnail(
                    accountID: accountID,
                    sessionID: sessionID,
                    fileID: file.id
                )
            }
            if canDownloadSource {
                self.queueSessionFileThumbnail(file, sessionID: sessionID, key: key)
            } else {
                self.requestedSessionFileThumbnailKeys.remove(key)
            }
        }
    }

    private func queueSessionFileThumbnail(
        _ file: SessionFileReference,
        sessionID: String,
        key: FileThumbnailKey
    ) {
        guard requestedSessionFileThumbnailKeys.contains(key),
            fileThumbnails[key] == nil
        else { return }
        queuedSessionFileThumbnails.append((sessionID, file))
        startNextSessionFileThumbnailDownload()
    }

    func startNextSessionFileThumbnailDownload() {
        guard connectionState.isReady,
              sessionFileThumbnailDownload == nil
        else { return }

        while !queuedSessionFileThumbnails.isEmpty {
            let (sessionID, file) = queuedSessionFileThumbnails.removeFirst()
            let key = FileThumbnailKey.session(sessionID: sessionID, fileID: file.id)
            guard fileThumbnails[key] == nil else {
                requestedSessionFileThumbnailKeys.remove(key)
                continue
            }
            let id = requestID("session-file-thumbnail")
            sessionFileThumbnailDownload = SessionFileThumbnailDownload(
                file: file,
                sessionID: sessionID,
                data: Data(),
                requestID: id
            )
            transmit(.readSessionFile(
                requestID: id,
                sessionID: sessionID,
                fileID: file.id,
                offset: 0,
                maxBytes: 256 * 1024
            )) { [weak self] _ in
                guard let self,
                      let download = self.sessionFileThumbnailDownload,
                      download.requestID == id
                else { return }
                self.finishSessionFileThumbnailAttempt(download, startsNext: false)
            }
            return
        }
    }

    func cancelSessionFileThumbnailDownloads() {
        if let requestID = sessionFileThumbnailDownload?.requestID {
            rememberDiscardedSessionFileThumbnailRequest(requestID)
        }
        queuedSessionFileThumbnails.removeAll()
        requestedSessionFileThumbnailKeys.removeAll()
        sessionFileThumbnailDownload = nil
    }

    func finishSessionFileThumbnailAttempt(
        _ download: SessionFileThumbnailDownload,
        startsNext: Bool = true
    ) {
        requestedSessionFileThumbnailKeys.remove(.session(
            sessionID: download.sessionID,
            fileID: download.file.id
        ))
        if sessionFileThumbnailDownload?.requestID == download.requestID {
            sessionFileThumbnailDownload = nil
        }
        if startsNext { startNextSessionFileThumbnailDownload() }
    }

    func rememberDiscardedSessionFileThumbnailRequest(_ requestID: String) {
        if discardedSessionFileThumbnailRequestIDs.count >= maximumDiscardedFileThumbnailRequestIDs,
           let requestID = discardedSessionFileThumbnailRequestIDs.first {
            discardedSessionFileThumbnailRequestIDs.remove(requestID)
        }
        discardedSessionFileThumbnailRequestIDs.insert(requestID)
    }

    func discardFileThumbnails() {
        cancelSessionFileThumbnailDownloads()
        fileThumbnails.removeAll()
        fileThumbnailOrder.removeAll()
    }

    func startNextSessionFileUpload() {
        guard connectionState.isReady,
              activeSessionFileUpload == nil,
              sessionFileUploadRequests.isEmpty,
              abandonedSessionFileUploadRequests.isEmpty,
              let sessionID = selectedSessionID,
              let index = composerAttachments.firstIndex(where: {
                  if case .queued = $0.state { return true }
                  return false
              }),
              sessionFileData[composerAttachments[index].id] != nil
        else { return }

        let item = composerAttachments[index]
        composerAttachments[index].state = .uploading(0)
        let id = requestID("session-file-begin")
        sessionFileUploadRequests[id] = .begin(localID: item.id, sessionID: sessionID)
        transmit(.beginSessionFileUpload(
            requestID: id,
            sessionID: sessionID,
            name: item.name,
            size: item.size,
            mediaType: item.mediaType
        )) { [weak self] message in
            self?.failSessionFileUploadRequest(id, message: message, showsToast: false)
        }
    }

    func handleSessionFileUploadReady(
        requestID: String,
        sessionID: String,
        uploadID: String,
        maxChunkBytes: Int
    ) {
        if let removed = abandonedSessionFileUploadRequests.removeValue(forKey: requestID) {
            guard sessionID == removed.sessionID, !uploadID.isEmpty else {
                showToast("The gateway returned an invalid upload.", tone: .error)
                startNextSessionFileUpload()
                return
            }
            requestSessionFileDeletion(removed, fileID: uploadID)
            startNextSessionFileUpload()
            return
        }
        guard let request = sessionFileUploadRequests[requestID] else { return }
        guard case .begin(let localID, let expectedSessionID) = request else {
            return failAttachment(request.localID, message: "The gateway returned an invalid upload.")
        }
        guard sessionID == expectedSessionID,
              sessionID == selectedSessionID,
              !uploadID.isEmpty,
              maxChunkBytes > 0,
              maxChunkBytes <= (sessionFileLimits?.maxUploadChunkBytes ?? 0)
        else { return failAttachment(localID, message: "The gateway returned an invalid upload.") }
        sessionFileUploadRequests.removeValue(forKey: requestID)
        activeSessionFileUpload = ActiveSessionFileUpload(
            localID: localID,
            sessionID: sessionID,
            uploadID: uploadID,
            maxChunkBytes: min(maxChunkBytes, uploadChunkByteLimit)
        )
        sendNextSessionFileChunk(localID: localID, offset: 0)
    }

    func handleSessionFileUploadChunkAccepted(
        requestID: String,
        sessionID: String,
        uploadID: String,
        nextOffset: Int64
    ) {
        guard let request = sessionFileUploadRequests[requestID] else { return }
        guard case .chunk(let localID, let expectedNextOffset) = request else {
            return failAttachment(request.localID, message: "The gateway returned an invalid upload.")
        }
        guard let upload = activeSessionFileUpload,
              upload.localID == localID,
              upload.sessionID == sessionID,
              upload.uploadID == uploadID
        else {
            return failAttachment(localID, message: "The gateway returned an invalid upload.")
        }
        guard nextOffset == expectedNextOffset else {
            return failAttachment(localID, message: "The gateway returned an invalid upload offset.")
        }
        sessionFileUploadRequests.removeValue(forKey: requestID)
        if let index = composerAttachments.firstIndex(where: { $0.id == localID }) {
            composerAttachments[index].state = .uploading(nextOffset)
        }
        sendNextSessionFileChunk(localID: localID, offset: nextOffset)
    }

    private func sendNextSessionFileChunk(localID: UUID, offset: Int64) {
        guard let upload = activeSessionFileUpload,
              upload.localID == localID,
              let data = sessionFileData[localID],
              offset >= 0,
              let start = Int(exactly: offset),
              start <= data.count
        else {
            failAttachment(localID, message: "The gateway returned an invalid upload offset.")
            return
        }
        guard start < data.count else {
            let id = requestID("session-file-finish")
            sessionFileUploadRequests[id] = .finish(localID: localID)
            transmit(.finishSessionFileUpload(
                requestID: id,
                sessionID: upload.sessionID,
                uploadID: upload.uploadID
            )) { [weak self] message in
                self?.failSessionFileUploadRequest(id, message: message, showsToast: false)
            }
            return
        }

        let end = min(start + upload.maxChunkBytes, data.count)
        let id = requestID("session-file-chunk")
        sessionFileUploadRequests[id] = .chunk(
            localID: localID,
            expectedNextOffset: Int64(end)
        )
        transmit(.uploadSessionFileChunk(
            requestID: id,
            sessionID: upload.sessionID,
            uploadID: upload.uploadID,
            offset: offset,
            data: Data(data[start..<end])
        )) { [weak self] message in
            self?.failSessionFileUploadRequest(id, message: message, showsToast: false)
        }
    }

    func handleSessionFileUploadCompleted(
        requestID: String,
        sessionID: String,
        file: SessionFileReference
    ) {
        guard let request = sessionFileUploadRequests[requestID] else { return }
        guard case .finish(let localID) = request else {
            return failAttachment(request.localID, message: "The gateway returned an invalid file.")
        }
        guard sessionID == selectedSessionID,
              activeSessionFileUpload?.localID == localID,
              activeSessionFileUpload?.sessionID == sessionID,
              let index = composerAttachments.firstIndex(where: { $0.id == localID }),
              composerAttachments[index].name == file.name,
              composerAttachments[index].size == file.size,
              composerAttachments[index].mediaType == file.mediaType
        else {
            return failAttachment(localID, message: "The gateway returned an invalid file.")
        }
        sessionFileUploadRequests.removeValue(forKey: requestID)
        promoteFileThumbnail(localID: localID, sessionID: sessionID, fileID: file.id)
        composerAttachments[index].state = .uploaded(file)
        sessionFileData[localID] = nil
        activeSessionFileUpload = nil
        upsertSessionFile(SessionFileRecord(origin: .user, file: file))
        startNextSessionFileUpload()
    }

    @discardableResult
    func failSessionFileUploadRequest(
        _ requestID: String,
        message: String,
        showsToast: Bool = true
    ) -> Bool {
        guard let request = sessionFileUploadRequests.removeValue(forKey: requestID) else {
            return false
        }
        failAttachment(request.localID, message: message, showsToast: showsToast)
        return true
    }

    private func failAttachment(
        _ localID: UUID,
        message: String,
        showsToast: Bool = true
    ) {
        sessionFileUploadRequests = sessionFileUploadRequests.filter { _, request in
            request.localID != localID
        }
        if activeSessionFileUpload?.localID == localID { activeSessionFileUpload = nil }
        if let index = composerAttachments.firstIndex(where: { $0.id == localID }) {
            composerAttachments[index].state = .failed(message)
        }
        if showsToast { showToast(verbatim: message, tone: .error) }
        startNextSessionFileUpload()
    }

    private func upsertSessionFile(_ record: SessionFileRecord) {
        if let index = sessionFiles.firstIndex(where: { $0.id == record.id }) {
            sessionFiles[index] = record
        } else {
            sessionFiles.append(record)
        }
    }

    func discardComposerAttachment(_ attachment: ComposerAttachment) {
        let localID = attachment.id
        let activeUpload = activeSessionFileUpload.flatMap { upload in
            upload.localID == localID ? upload : nil
        }
        if let request = sessionFileUploadRequests.first(where: {
            guard case .begin(let requestLocalID, _) = $0.value else { return false }
            return requestLocalID == localID
        }) {
            guard case .begin(_, let sessionID) = request.value else { return }
            sessionFileUploadRequests.removeValue(forKey: request.key)
            abandonedSessionFileUploadRequests[request.key] = RemovedComposerAttachment(
                sessionID: sessionID,
                attachment: attachment
            )
        } else {
            sessionFileUploadRequests = sessionFileUploadRequests.filter { _, request in
                request.localID != localID
            }
        }
        if activeSessionFileUpload?.localID == localID { activeSessionFileUpload = nil }
        sessionFileData[localID] = nil
        removeFileThumbnail(for: .composer(localID))
        composerAttachments.removeAll { $0.id == localID }

        if let activeUpload {
            requestSessionFileDeletion(
                RemovedComposerAttachment(
                    sessionID: activeUpload.sessionID,
                    attachment: attachment
                ),
                fileID: activeUpload.uploadID
            )
        } else if case .uploaded(let file) = attachment.state,
                  let sessionID = selectedSessionID {
            requestSessionFileDeletion(
                RemovedComposerAttachment(sessionID: sessionID, attachment: attachment),
                fileID: file.id
            )
        }
    }

    private func requestSessionFileDeletion(
        _ removed: RemovedComposerAttachment,
        fileID: String
    ) {
        let id = requestID("session-file-delete")
        sessionFileDeleteRequests[id] = removed
        sessionFiles.removeAll { $0.id == fileID }
        removeFileThumbnail(for: .session(sessionID: removed.sessionID, fileID: fileID))
        transmit(.deleteSessionFile(
            requestID: id,
            sessionID: removed.sessionID,
            fileID: fileID
        ))
    }

    @discardableResult
    func failSessionFileDeletionRequest(
        _ requestID: String,
        message: String,
        refreshesFiles: Bool = false,
        showsToast: Bool = true
    ) -> Bool {
        guard let removed = sessionFileDeleteRequests.removeValue(forKey: requestID) else {
            return false
        }
        if case .uploaded = removed.attachment.state,
           selectedSessionID == removed.sessionID,
           !composerAttachments.contains(where: { $0.id == removed.attachment.id }) {
            composerAttachments.append(removed.attachment)
        }
        if refreshesFiles, selectedSessionID == removed.sessionID { refreshSessionFiles() }
        if showsToast { showToast(verbatim: message, tone: .error) }
        startNextSessionFileUpload()
        return true
    }

    @discardableResult
    func discardAbandonedSessionFileUploadRequest(_ requestID: String) -> Bool {
        guard abandonedSessionFileUploadRequests.removeValue(forKey: requestID) != nil else {
            return false
        }
        startNextSessionFileUpload()
        return true
    }

    func acceptSessionFileDeletionRequest(_ requestID: String) {
        guard sessionFileDeleteRequests.removeValue(forKey: requestID) != nil else { return }
        startNextSessionFileUpload()
    }

    func discardComposerAttachments() {
        for attachment in composerAttachments { discardComposerAttachment(attachment) }
    }

    func discardPendingComposerAttachments() {
        for attachment in composerAttachments {
            if case .uploaded = attachment.state { continue }
            removeFileThumbnail(for: .composer(attachment.id))
        }
        composerAttachments.removeAll { item in
            if case .uploaded = item.state { return false }
            return true
        }
        sessionFileData.removeAll()
    }

    func handleSessionFileChunk(
        requestID: String,
        sessionID: String,
        fileID: String,
        offset: Int64,
        data: Data,
        nextOffset: Int64?
    ) {
        if discardedSessionFileThumbnailRequestIDs.remove(requestID) != nil { return }
        guard sessionFileDownload?.requestID == requestID else {
            handleSessionFileThumbnailChunk(
                requestID: requestID,
                sessionID: sessionID,
                fileID: fileID,
                offset: offset,
                data: data,
                nextOffset: nextOffset
            )
            return
        }
        guard var download = sessionFileDownload,
              download.requestID == requestID
        else { return }
        sessionFileDownload = nil
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
            sessionFileDownload = download
            transmit(.readSessionFile(
                requestID: id,
                sessionID: sessionID,
                fileID: fileID,
                offset: nextOffset,
                maxBytes: 256 * 1024
            )) { [weak self] message in
                guard self?.sessionFileDownload?.requestID == id else { return }
                self?.sessionFileDownload = nil
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

    private func handleSessionFileThumbnailChunk(
        requestID: String,
        sessionID: String,
        fileID: String,
        offset: Int64,
        data: Data,
        nextOffset: Int64?
    ) {
        guard var download = sessionFileThumbnailDownload,
              download.requestID == requestID
        else { return }
        sessionFileThumbnailDownload = nil
        guard download.sessionID == sessionID,
              download.file.id == fileID,
              offset == Int64(download.data.count),
              data.count <= 256 * 1024,
              Int64(download.data.count + data.count) <= download.file.size,
              Int64(download.data.count + data.count) <= maximumFileThumbnailSourceBytes
        else {
            finishSessionFileThumbnailAttempt(download)
            return
        }
        download.data.append(data)
        if let nextOffset {
            guard nextOffset == Int64(download.data.count), nextOffset > offset else {
                finishSessionFileThumbnailAttempt(download)
                return
            }
            let id = self.requestID("session-file-thumbnail")
            download.requestID = id
            sessionFileThumbnailDownload = download
            transmit(.readSessionFile(
                requestID: id,
                sessionID: sessionID,
                fileID: fileID,
                offset: nextOffset,
                maxBytes: 256 * 1024
            )) { [weak self] _ in
                guard let self,
                      let download = self.sessionFileThumbnailDownload,
                      download.requestID == id
                else { return }
                self.finishSessionFileThumbnailAttempt(download, startsNext: false)
            }
            return
        }

        guard Int64(download.data.count) == download.file.size else {
            finishSessionFileThumbnailAttempt(download)
            return
        }
        sessionFileThumbnailDownload = download
        Task { [weak self] in
            let thumbnail = await Self.downsampledFileThumbnail(from: download.data)
            guard let self,
                  self.sessionFileThumbnailDownload?.requestID == download.requestID
            else { return }
            self.sessionFileThumbnailDownload = nil
            if let thumbnail {
                self.cacheFileThumbnail(
                    thumbnail,
                    for: .session(sessionID: sessionID, fileID: fileID)
                )
                self.persistFileThumbnail(thumbnail, sessionID: sessionID, fileID: fileID)
            }
            self.finishSessionFileThumbnailAttempt(download)
        }
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
            transmit(.readWorkspaceFile(
                requestID: id,
                sessionID: sessionID,
                path: path,
                offset: nextOffset,
                maxBytes: 256 * 1024
            )) { [weak self] message in
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
            guard !text.unicodeScalars.contains(where: {
                CharacterSet.controlCharacters.contains($0) && !allowedControls.contains($0)
            }) else { return nil }
            return text
        }.value
    }

    nonisolated static func writeTemporarySessionFile(
        _ data: Data,
        name: String
    ) async throws -> TemporarySessionFile {
        try await Task.detached(priority: .userInitiated) {
            let directory = URL.temporaryDirectory.appending(path: UUID().uuidString, directoryHint: .isDirectory)
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let candidateExtension = URL(fileURLWithPath: name).pathExtension
            let safeExtension = candidateExtension.utf8.count <= 16
                && candidateExtension.unicodeScalars.allSatisfy(CharacterSet.alphanumerics.contains)
                ? candidateExtension
                : ""
            let candidateName = URL(fileURLWithPath: name).lastPathComponent
            let safeName = candidateName.utf8.count <= 255
                && candidateName != "."
                && candidateName != ".."
                && !candidateName.unicodeScalars.contains(where: {
                    CharacterSet.controlCharacters.contains($0) || $0 == "/" || $0 == "\\" || $0 == ":"
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

    func widgets(in slot: FrontendSlot) -> [MountedWidget] {
        mountedWidgets.filter { $0.widget.slot == slot }
    }

    func requestID(_ prefix: String) -> String {
        "\(prefix)-\(UUID().uuidString.lowercased())"
    }

    func cacheChatCatalog(lastSessionID: String? = nil) {
        guard !isClearingLocalData, let accountID = selectedAccountID else { return }
        let catalog = CachedChatCatalog(
            bots: bots,
            sessions: sessions,
            swarms: swarms,
            lastSessionID: lastSessionID ?? selectedSessionID
        )
        enqueueTranscriptIO { [store] in
            await store.saveChatCatalog(catalog, accountID: accountID)
        }
    }

    func clearCachedData() async {
        guard !isClearingLocalData else { return }
        isClearingLocalData = true
        defer { isClearingLocalData = false }
        discardFileThumbnails()
        let previous = transcriptIOTask
        await previous?.value
        do {
            try await store.clearCachedData()
            showToast("Cached data cleared.", tone: .success)
        } catch {
            showToast(verbatim: localizedErrorDescription(error), tone: .error)
        }
    }

    func enqueueTranscriptIO(
        _ operation: @escaping @MainActor @Sendable () async -> Void
    ) {
        let previous = transcriptIOTask
        transcriptIOTask = Task {
            await previous?.value
            await operation()
        }
    }

    func scheduleComposerDraftSave() {
        guard !suppressesComposerDraftSave,
              !isLoadingComposerDraft,
              !isLoadingComposerEditRecovery,
              let owner = composerDraftOwner
        else { return }
        composerDraftSaveTask?.cancel()
        if var pending = pendingWidgetEdit,
           pending.owner == owner,
           pending.recovery.phase == .editing {
            guard composer.utf8.count <= maximumComposerBytes else { return }
            pending.recovery.editedInput = composer
            pendingWidgetEdit = pending
            let recovery = pending.recovery
            composerDraftSaveTask = Task { [weak self] in
                do {
                    try await Task.sleep(for: .milliseconds(400))
                } catch {
                    return
                }
                guard let self,
                      self.pendingWidgetEdit?.owner == owner,
                      self.pendingWidgetEdit?.recovery.phase == .editing,
                      self.pendingWidgetEdit?.recovery.editedInput == recovery.editedInput
                else { return }
                self.composerDraftSaveTask = nil
                self.enqueueComposerEditRecoverySave(recovery, owner: owner)
            }
            return
        }
        guard stashedComposerDraft == nil else { return }
        let text = composer
        composerDraftSaveTask = Task { [weak self] in
            do {
                try await Task.sleep(for: .milliseconds(400))
            } catch {
                return
            }
            guard let self, owner == composerDraftOwner else { return }
            composerDraftSaveTask = nil
            enqueueComposerDraftSave(text, owner: owner)
        }
    }

    func flushComposerDraft() {
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        guard stashedComposerDraft == nil, let owner = composerDraftOwner else { return }
        enqueueComposerDraftSave(composer, owner: owner)
    }

    func enqueueComposerDraftSave(_ text: String, owner: ComposerDraftOwner) {
        let previous = composerDraftIOTask
        let store = store
        composerDraftIOTask = Task {
            await previous?.value
            await store.saveComposerDraft(
                text,
                accountID: owner.accountID,
                sessionID: owner.sessionID
            )
        }
    }

    func enqueueComposerEditRecoverySave(
        _ recovery: ComposerEditRecovery,
        owner: ComposerDraftOwner,
        completion: ((Result<Void, Error>) -> Void)? = nil
    ) {
        let previous = composerDraftIOTask
        let store = store
        composerDraftIOTask = Task {
            await previous?.value
            do {
                try await store.saveComposerEditRecovery(
                    recovery,
                    accountID: owner.accountID,
                    sessionID: owner.sessionID
                )
                completion?(.success(()))
            } catch {
                completion?(.failure(error))
            }
        }
    }

    func enqueueComposerEditRecoveryRemoval(owner: ComposerDraftOwner) {
        let previous = composerDraftIOTask
        let store = store
        composerDraftIOTask = Task {
            await previous?.value
            try? await store.removeComposerEditRecovery(
                accountID: owner.accountID,
                sessionID: owner.sessionID
            )
        }
    }

    func prepareComposerEditRecovery(for owner: ComposerDraftOwner) {
        guard composerDraftOwner == owner else { return }
        if pendingWidgetEdit?.owner == owner {
            if replayRequestID == nil { reconcileComposerEditRecovery() }
            return
        }
        let generation = UUID()
        composerEditRecoveryGeneration = generation
        isLoadingComposerEditRecovery = true
        let previous = composerDraftIOTask
        let store = store
        composerDraftIOTask = Task { [weak self] in
            await previous?.value
            let recovery = await store.loadComposerEditRecovery(
                accountID: owner.accountID,
                sessionID: owner.sessionID
            )
            guard let self,
                  self.composerEditRecoveryGeneration == generation,
                  self.composerDraftOwner == owner
            else { return }
            self.isLoadingComposerEditRecovery = false
            self.pendingWidgetEdit = recovery.map {
                PendingWidgetEdit(owner: owner, recovery: $0)
            }
            if self.replayRequestID == nil { self.reconcileComposerEditRecovery() }
        }
    }

    func observeReplayCompletion(_ buffered: BufferedAgentEvent) {
        guard replayRequestID != nil else { return }
        let event = buffered.record.event
        let type = event.msg["type"]?.stringValue
        let message = type == "message" ? try? MessageEventPayload(json: event.msg) : nil
        if let submissionID = event.submissionId,
           message?.author == .user
               || (type == "frontend"
                   && event.msg["frontendType"]?.stringValue == "widget"),
           replayCompletionSubmissionIDs.count < maximumObservedReplaySubmissions
               || replayCompletionSubmissionIDs.contains(submissionID) {
            replayCompletionSubmissionIDs.insert(submissionID)
        }

        var messages: [ReplayUserMessage] = []
        if let message, message.author == .user {
            let sequence = message.messageTarget?.checkpointSequence
                ?? buffered.record.sequence
            messages.append(ReplayUserMessage(sequence: sequence, text: message.text))
        }
        guard !messages.isEmpty else { return }
        replayUserMessages.append(contentsOf: messages.suffix(maximumObservedReplaySubmissions))
        if replayUserMessages.count > maximumObservedReplaySubmissions {
            replayUserMessages.removeFirst(
                replayUserMessages.count - maximumObservedReplaySubmissions
            )
        }
    }

    func reconcileComposerEditRecovery() {
        guard replayRequestID == nil,
              !isLoadingComposerEditRecovery
        else { return }
        defer {
            replayCompletionSubmissionIDs.removeAll(keepingCapacity: true)
            replayUserMessages.removeAll(keepingCapacity: true)
            completedComposerEditReplay = false
        }
        guard let pending = pendingWidgetEdit,
              pending.owner == composerDraftOwner
        else { return }
        let matchingWidgetInput = mountedWidgets.first(where: {
            $0.capability == pending.recovery.capability
                && $0.widget.id == pending.recovery.widgetID
        })?.widget.action?.capabilityInput
        let renderedEditedInput: Bool = if let baseline = pending.recovery.submissionBaselineSequence {
            transcript.contains {
                $0.kind == .user
                    && $0.text == pending.recovery.editedInput
                    && ($0.messageTarget?.checkpointSequence ?? 0) > baseline
            } || replayUserMessages.contains {
                $0.sequence > baseline && $0.text == pending.recovery.editedInput
            }
        } else {
            false
        }
        switch pending.recovery.phase {
        case .removingQueuedInput where matchingWidgetInput == pending.recovery.originalInput:
            completeComposerEditRecovery(pending)
        case .submitting where matchingWidgetInput == pending.recovery.editedInput
            || replayCompletionSubmissionIDs.contains(pending.recovery.requestID)
            || renderedEditedInput:
            completeComposerEditRecovery(pending)
        case .removingQueuedInput, .editing:
            restoreComposerEditMode(pending)
        case .submitting where completedComposerEditReplay:
            restoreComposerEditMode(pending)
        case .submitting:
            break
        case .completed:
            completeComposerEditRecovery(pending)
        }
    }

    func restoreComposerEditMode(requestID: String) {
        guard let pending = pendingWidgetEdit,
              pending.recovery.requestID == requestID,
              pending.recovery.phase == .submitting
        else { return }
        restoreComposerEditMode(pending)
    }

    func restoreComposerEditMode(_ current: PendingWidgetEdit) {
        var pending = current
        pending.recovery.phase = .editing
        pendingWidgetEdit = pending
        stashedComposerDraft = pending.recovery.displacedDraft
        suppressesComposerDraftSave = true
        composer = pending.recovery.editedInput
        suppressesComposerDraftSave = false
        composerFocusRequest &+= 1
        enqueueComposerEditRecoverySave(pending.recovery, owner: pending.owner)
    }

    func rejectComposerEdit(requestID: String) {
        guard let pending = pendingWidgetEdit,
              pending.recovery.requestID == requestID
        else { return }
        switch pending.recovery.phase {
        case .removingQueuedInput:
            completeComposerEditRecovery(pending)
        case .submitting:
            restoreComposerEditMode(pending)
        case .editing, .completed:
            break
        }
    }

    func completeSubmittedComposerEdit(requestID: String) {
        guard let pending = pendingWidgetEdit,
              pending.recovery.requestID == requestID,
              pending.recovery.phase == .submitting
        else { return }
        completeComposerEditRecovery(pending)
    }

    private func completeComposerEditRecovery(_ current: PendingWidgetEdit) {
        guard let pending = pendingWidgetEdit,
              pending.owner == current.owner,
              pending.recovery.requestID == current.recovery.requestID
        else { return }
        var completed = pending
        completed.recovery.phase = .completed
        pendingWidgetEdit = completed
        enqueueComposerEditRecoverySave(completed.recovery, owner: completed.owner) { [weak self] result in
            guard let self,
                  self.pendingWidgetEdit?.owner == completed.owner,
                  self.pendingWidgetEdit?.recovery.requestID == completed.recovery.requestID,
                  self.pendingWidgetEdit?.recovery.phase == .completed
            else { return }
            switch result {
            case .success:
                self.pendingWidgetEdit = nil
                self.stashedComposerDraft = nil
                self.cacheSelectedTranscript()
            case .failure(let error):
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
            }
        }
    }

    func changeComposerDraftOwner(to owner: ComposerDraftOwner?) {
        guard owner != composerDraftOwner else { return }
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        let previousOwner = composerDraftOwner
        if var pending = pendingWidgetEdit,
           pending.owner == previousOwner,
           pending.recovery.phase == .editing,
           composer.utf8.count <= maximumComposerBytes {
            pending.recovery.editedInput = composer
            pendingWidgetEdit = pending
            enqueueComposerEditRecoverySave(pending.recovery, owner: pending.owner)
        }
        let previousText = pendingWidgetEdit?.recovery.displacedDraft ?? composer
        pendingWidgetEdit = nil
        stashedComposerDraft = nil
        composerEditRecoveryGeneration = UUID()
        isLoadingComposerEditRecovery = false
        let previousIO = composerDraftIOTask
        let generation = UUID()
        composerDraftGeneration = generation
        composerDraftOwner = owner
        isLoadingComposerDraft = owner != nil
        suppressesComposerDraftSave = true
        composer = previousOwner == nil ? previousText : ""
        suppressesComposerDraftSave = false
        let store = store
        composerDraftIOTask = Task { [weak self] in
            await previousIO?.value
            if let previousOwner {
                await store.saveComposerDraft(
                    previousText,
                    accountID: previousOwner.accountID,
                    sessionID: previousOwner.sessionID
                )
            }
            guard let owner else { return }
            let restored = await store.loadComposerDraft(
                accountID: owner.accountID,
                sessionID: owner.sessionID
            )
            guard let self,
                  composerDraftGeneration == generation,
                  composerDraftOwner == owner
            else { return }
            suppressesComposerDraftSave = true
            if composer.isEmpty {
                composer = restored
            } else if !restored.isEmpty {
                composer = "\(restored)\n\n\(composer)"
            }
            suppressesComposerDraftSave = false
            isLoadingComposerDraft = false
            scheduleComposerDraftSave()
        }
    }

    func discardComposerDraft() {
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        invalidateComposerEditRecovery()
        composerDraftGeneration = UUID()
        composerDraftOwner = nil
        isLoadingComposerDraft = false
        suppressesComposerDraftSave = true
        composer = ""
        suppressesComposerDraftSave = false
    }

    func invalidateComposerEditRecovery(for owner: ComposerDraftOwner? = nil) {
        if let owner {
            guard pendingWidgetEdit?.owner == owner || composerDraftOwner == owner else { return }
        }
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        pendingWidgetEdit = nil
        stashedComposerDraft = nil
        composerEditRecoveryGeneration = UUID()
        isLoadingComposerEditRecovery = false
    }

    func restoreDraft(id: String) {
        guard let draft = pendingDrafts.removeValue(forKey: id) else { return }
        restoreDraft(draft)
    }

    func restoreDraft(_ draft: PendingComposerDraft) {
        if !draft.text.isEmpty {
            composer = composer.isEmpty ? draft.text : "\(draft.text)\n\n\(composer)"
        }
        let currentIDs = Set(composerAttachments.compactMap { item -> String? in
            guard case .uploaded(let attachment) = item.state else { return nil }
            return attachment.id
        })
        let available = max(0, attachmentReferenceLimit - composerAttachments.count)
        composerAttachments.insert(contentsOf: draft.attachments
            .filter { !currentIDs.contains($0.id) }
            .prefix(available)
            .map { attachment in
                ComposerAttachment(
                    id: UUID(),
                    name: attachment.name,
                    size: attachment.size,
                    mediaType: attachment.mediaType,
                    state: .uploaded(attachment)
                )
            }, at: 0)
    }

    func restorePendingDrafts() {
        let drafts = pendingDrafts.keys.sorted().compactMap { pendingDrafts[$0] }
        pendingDrafts.removeAll()
        guard !drafts.isEmpty else { return }
        for draft in drafts.reversed() { restoreDraft(draft) }
    }

}
