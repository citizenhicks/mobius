import CoreGraphics
import Foundation
import AVFoundation
import ImageIO
import UniformTypeIdentifiers

private let maximumFileThumbnailSourceBytes: Int64 = 10 * 1024 * 1024
private let maximumCachedFileThumbnails = 32
private let maximumDiscardedFileThumbnailRequestIDs = 32
private let maximumFileThumbnailPixelDimension = 384

extension ChatSessionModel {
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
        guard !isClearingLocalData, let accountID = gateway.selectedAccountID else { return }
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
        guard let accountID = gateway.selectedAccountID else {
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
                  self.gateway.selectedAccountID == accountID,
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
        guard gateway.connectionState.isReady,
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
            gateway.transmit(.readSessionFile(
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
        guard gateway.connectionState.isReady,
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
        gateway.transmit(.beginSessionFileUpload(
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
            gateway.transmit(.finishSessionFileUpload(
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
        gateway.transmit(.uploadSessionFileChunk(
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
        gateway.transmit(.deleteSessionFile(
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

    func handleSessionFileThumbnailChunk(
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
            gateway.transmit(.readSessionFile(
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
}
