import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testFirstMessageStagesAttachmentUntilPendingChatOpens() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        var config = composition()
        config.middleware.enabled.insert("attachments")
        model.bots = [bot(config: VersionedAgentConfig(revision: 1, config: config))]
        model.modelChoices = [ModelChoice(
            route: "openai",
            group: "OpenAI",
            model: "gpt-5.6-sol",
            reasoningEffort: "high",
            contextWindow: 200_000,
            supportsImageInput: true,
            toolDiscovery: .native
        )]
        model.modelProviders = ["openai": "openai-work"]
        model.connectionState = .ready
        model.chooseWorkspace("/srv/mobius")

        XCTAssertTrue(model.attachmentsEnabled)
        XCTAssertTrue(model.canImportAttachments)

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("scan.png")
        let bytes = try tinyPNGData()
        try bytes.write(to: fileURL)

        await model.importAttachments([fileURL])

        XCTAssertEqual(model.composerAttachments.first?.state, .queued)
        XCTAssertTrue(model.canSendComposer)
        let stagedRequests = await recorder.requests()
        XCTAssertFalse(stagedRequests.contains { request in
            if case .beginSessionFileUpload = request { return true }
            return false
        })

        model.composer = "Review this image"
        XCTAssertTrue(model.sendMessage())
        let create = await recorder.firstRequest(after: 0) { request in
            if case .createSession = request { return true }
            return false
        }
        guard case .createSession(let createID, _, _) = try XCTUnwrap(create) else {
            return XCTFail("Expected session creation")
        }

        model.handle(.sessionOpened(
            requestID: createID,
            payload: sessionReady(
                latestSequence: 0,
                sessionID: "chat-created",
                contributions: [fileAttachmentContribution()]
            )
        ))
        model.handle(.sessionReplayComplete(requestID: createID, sessionID: "chat-created"))
        model.handle(.sessionChanged(sessionReady(
            latestSequence: 0,
            sessionID: "chat-created",
            contributions: [fileAttachmentContribution()]
        )))
        XCTAssertEqual(model.pendingNewChatBotID, "bot-1")

        let begin = await recorder.firstRequest(after: 0) { request in
            if case .beginSessionFileUpload = request { return true }
            return false
        }
        guard case .beginSessionFileUpload(
            let beginID,
            let sessionID,
            let name,
            let size,
            let mediaType
        ) = try XCTUnwrap(begin) else {
            return XCTFail("Expected attachment upload")
        }
        XCTAssertEqual(sessionID, "chat-created")

        model.handle(.sessionFileUploadReady(
            requestID: beginID,
            sessionID: sessionID,
            uploadID: "upload-1",
            maxChunkBytes: model.uploadChunkByteLimit
        ))
        let chunk = await recorder.firstRequest(after: 0) { request in
            if case .uploadSessionFileChunk = request { return true }
            return false
        }
        guard case .uploadSessionFileChunk(let chunkID, _, _, _, let data) = try XCTUnwrap(chunk) else {
            return XCTFail("Expected attachment bytes")
        }
        XCTAssertEqual(data, bytes)

        model.handle(.sessionFileUploadChunkAccepted(
            requestID: chunkID,
            sessionID: sessionID,
            uploadID: "upload-1",
            nextOffset: Int64(data.count)
        ))
        let finish = await recorder.firstRequest(after: 0) { request in
            if case .finishSessionFileUpload = request { return true }
            return false
        }
        guard case .finishSessionFileUpload(let finishID, _, _) = try XCTUnwrap(finish) else {
            return XCTFail("Expected attachment upload completion")
        }
        let attachment = SessionFileReference(
            id: "file-1",
            name: name,
            size: size,
            mediaType: mediaType
        )
        model.handle(.sessionFileUploadCompleted(
            requestID: finishID,
            sessionID: sessionID,
            file: attachment
        ))
        XCTAssertFalse(model.canSendComposer)

        let submit = await recorder.firstRequest(after: 0) { request in
            if case .submit = request { return true }
            return false
        }
        guard case .submit("chat-created", let submission) = try XCTUnwrap(submit),
              case .message(let message) = submission.op
        else { return XCTFail("Expected first message submission") }
        XCTAssertEqual(message.text, "Review this image")
        XCTAssertEqual(message.attachments, [attachment])
    }

    func testAttachmentReservationIsVisibleBeforeImportCompletes() async throws {
        let model = try model()
        var config = composition()
        config.middleware.enabled.insert("attachments")
        model.bots = [bot(config: VersionedAgentConfig(revision: 1, config: config))]
        model.connectionState = .ready
        model.chooseWorkspace("/srv/mobius")
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("clip.mp4")
        try tinyH264MP4Data().write(to: fileURL)

        let id = try XCTUnwrap(model.reserveComposerAttachment(named: "clip.mp4"))

        XCTAssertEqual(model.composerAttachments.count, 1)
        XCTAssertEqual(model.composerAttachments.first?.id, id)
        XCTAssertEqual(model.composerAttachments.first?.state, .preparing)

        await model.completeComposerAttachmentImport(fileURL, reservedID: id)

        XCTAssertEqual(model.composerAttachments.first?.id, id)
        XCTAssertEqual(model.composerAttachments.first?.state, .queued)
    }

    func testRemovingFailedFirstMessageAttachmentContinuesPendingSend() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let attachmentID = UUID()
        model.connectionState = .ready
        model.sessions = [session(sessionID: "chat-created", state: .idle)]
        model.selectedSessionID = "chat-created"
        model.pendingNewChatBotID = "bot-1"
        model.pendingDrafts["create-1"] = PendingComposerDraft(
            text: "Continue without the file",
            attachments: []
        )
        model.composerAttachments = [ComposerAttachment(
            id: attachmentID,
            name: "broken.png",
            size: 1,
            mediaType: "image/png",
            state: .failed("Upload failed")
        )]

        model.removeComposerAttachment(attachmentID)

        let submitted = await eventually {
            let requests = await recorder.requests()
            return requests.contains { request in
                if case .submit = request { return true }
                return false
            }
        }
        XCTAssertTrue(submitted)
        let request = await recorder.firstRequest(after: 0) { request in
            if case .submit = request { return true }
            return false
        }
        guard case .submit("chat-created", let submission) = try XCTUnwrap(request),
              case .message(let message) = submission.op
        else { return XCTFail("Expected the preserved first message") }
        XCTAssertEqual(message.text, "Continue without the file")
        XCTAssertTrue(message.attachments.isEmpty)
    }

    func testRemovingLocalAttachmentDoesNotSendGatewayDelete() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let attachmentID = UUID()
        model.connectionState = .ready
        model.composerAttachments = [ComposerAttachment(
            id: attachmentID,
            name: "local.txt",
            size: 1,
            mediaType: "text/plain",
            state: .queued
        )]
        model.sessionFileData[attachmentID] = Data([1])

        model.removeComposerAttachment(attachmentID)

        XCTAssertTrue(model.composerAttachments.isEmpty)
        XCTAssertNil(model.sessionFileData[attachmentID])
        let requests = await recorder.requests()
        XCTAssertTrue(requests.isEmpty)
    }

    func testRemovingAttachmentBeforeUploadReadyDeletesReturnedUpload() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let attachmentID = UUID()
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.composerAttachments = [ComposerAttachment(
            id: attachmentID,
            name: "scan.png",
            size: 3,
            mediaType: "image/png",
            state: .uploading(0)
        )]
        model.sessionFileData[attachmentID] = Data([1, 2, 3])
        model.sessionFileUploadRequests["begin-1"] = .begin(
            localID: attachmentID,
            sessionID: "chat-1"
        )

        model.removeComposerAttachment(attachmentID)
        XCTAssertTrue(model.composerAttachments.isEmpty)
        XCTAssertNotNil(model.abandonedSessionFileUploadRequests["begin-1"])
        XCTAssertFalse(model.canOpenSession)

        model.handle(.sessionFileUploadReady(
            requestID: "begin-1",
            sessionID: "chat-1",
            uploadID: "file-1",
            maxChunkBytes: 3
        ))

        let request = await recorder.firstRequest(after: 0) {
            if case .deleteSessionFile = $0 { return true }
            return false
        }
        guard case .deleteSessionFile(let requestID, let sessionID, let fileID) =
            try XCTUnwrap(request)
        else { return XCTFail("Expected abandoned upload deletion") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(fileID, "file-1")
        XCTAssertNotNil(model.sessionFileDeleteRequests[requestID])
        XCTAssertFalse(model.canOpenSession)
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains {
            if case .uploadSessionFileChunk = $0 { return true }
            return false
        })

        model.handle(.accepted(requestID: requestID))
        XCTAssertTrue(model.canOpenSession)
    }

    func testRejectedAbandonedBeginStartsNextQueuedUpload() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let abandonedID = UUID()
        let queuedID = UUID()
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.composerAttachments = [
            ComposerAttachment(
                id: abandonedID,
                name: "cancelled.bin",
                size: 1,
                mediaType: "application/octet-stream",
                state: .uploading(0)
            ),
            ComposerAttachment(
                id: queuedID,
                name: "next.bin",
                size: 1,
                mediaType: "application/octet-stream",
                state: .queued
            ),
        ]
        model.sessionFileData[abandonedID] = Data([1])
        model.sessionFileData[queuedID] = Data([2])
        model.sessionFileUploadRequests["begin-1"] = .begin(
            localID: abandonedID,
            sessionID: "chat-1"
        )

        model.removeComposerAttachment(abandonedID)
        model.handle(.rejected(GatewayRejection(
            requestId: "begin-1",
            code: "session_file_rejected",
            message: "Cancelled upload was rejected",
            fatal: false
        )))

        let request = await recorder.firstRequest(after: 0) {
            guard case .beginSessionFileUpload(_, _, let name, _, _) = $0 else {
                return false
            }
            return name == "next.bin"
        }
        XCTAssertNotNil(request)
        XCTAssertNil(model.toast)
    }

    func testRejectedUploadedAttachmentDeleteRestoresComposerCard() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let file = SessionFileReference(
            id: "file-1",
            name: "scan.png",
            size: 3,
            mediaType: "image/png"
        )
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.composerAttachments = [ComposerAttachment(
            id: UUID(),
            name: file.name,
            size: file.size,
            mediaType: file.mediaType,
            state: .uploaded(file)
        )]

        model.removeComposerAttachment(try XCTUnwrap(model.composerAttachments.first?.id))

        let request = await recorder.firstRequest(after: 0) {
            if case .deleteSessionFile = $0 { return true }
            return false
        }
        guard case .deleteSessionFile(let requestID, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected uploaded file deletion") }
        XCTAssertTrue(model.composerAttachments.isEmpty)

        model.handle(.rejected(GatewayRejection(
            requestId: requestID,
            code: "session_file_rejected",
            message: "File could not be deleted",
            fatal: false
        )))

        XCTAssertEqual(model.composerAttachments.first?.state, .uploaded(file))
        XCTAssertEqual(model.toast?.message, "File could not be deleted")
    }

    func testDiscardingComposerAttachmentsDeletesCompletedUploads() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let file = SessionFileReference(
            id: "file-1",
            name: "notes.txt",
            size: 4,
            mediaType: "text/plain"
        )
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.composerAttachments = [ComposerAttachment(
            id: UUID(),
            name: file.name,
            size: file.size,
            mediaType: file.mediaType,
            state: .uploaded(file)
        )]
        XCTAssertFalse(model.canOpenSession)

        model.discardComposerAttachments()

        let request = await recorder.firstRequest(after: 0) {
            if case .deleteSessionFile = $0 { return true }
            return false
        }
        guard case .deleteSessionFile(let requestID, let sessionID, let fileID) =
            try XCTUnwrap(request)
        else { return XCTFail("Expected discarded file deletion") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(fileID, "file-1")
        XCTAssertTrue(model.composerAttachments.isEmpty)
        XCTAssertFalse(model.canOpenSession)

        model.handle(.accepted(requestID: requestID))
        XCTAssertTrue(model.canOpenSession)
    }

    func testConnectionEndClearsAttachmentRemovalRequests() throws {
        let model = try model()
        let attachment = ComposerAttachment(
            id: UUID(),
            name: "scan.png",
            size: 3,
            mediaType: "image/png",
            state: .uploading(0)
        )
        let removed = RemovedComposerAttachment(sessionID: "chat-1", attachment: attachment)
        model.abandonedSessionFileUploadRequests["begin-1"] = removed
        model.sessionFileDeleteRequests["delete-1"] = removed
        let generation = model.connectionGeneration

        model.connectionEnded(generation: generation, message: "Disconnected")

        XCTAssertTrue(model.abandonedSessionFileUploadRequests.isEmpty)
        XCTAssertTrue(model.sessionFileDeleteRequests.isEmpty)
    }

    func testAttachmentComposerUsesAdvertisedPolicyWithinClientSafetyCaps() throws {
        let model = try model()
        model.sessionFileLimits = SessionFileLimits(
            maxAttachmentReferences: 3,
            maxFileBytes: 4 * 1024 * 1024,
            maxSessionFiles: 8,
            maxSessionBytes: 6 * 1024 * 1024,
            maxUploadChunkBytes: 64 * 1024
        )

        XCTAssertEqual(model.attachmentReferenceLimit, 3)
        XCTAssertEqual(model.attachmentFileByteLimit, 4 * 1024 * 1024)
        XCTAssertEqual(model.attachmentDraftByteLimit, 6 * 1024 * 1024)
        XCTAssertEqual(model.uploadChunkByteLimit, 64 * 1024)

        model.sessionFileLimits = SessionFileLimits(
            maxAttachmentReferences: .max,
            maxFileBytes: .max,
            maxSessionFiles: .max,
            maxSessionBytes: .max,
            maxUploadChunkBytes: .max
        )
        XCTAssertEqual(model.attachmentFileByteLimit, 250 * 1024 * 1024)
        XCTAssertEqual(model.attachmentDraftByteLimit, 250 * 1024 * 1024)
    }

    func testSessionFileUploadUsesAcknowledgedChunksAndSendsNativeReferences() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.selectedModelRoute = "openai"
        let capableChoice = ModelChoice(
            route: "openai",
            group: "OpenAI",
            model: "gpt-5.6-sol",
            reasoningEffort: "high",
            contextWindow: 200_000,
            supportsImageInput: true,
            toolDiscovery: .native
        )
        model.modelChoices = [capableChoice]
        let attachmentContribution = FrontendContribution(
            capability: "files",
            acceptsFileAttachments: true,
            count: nil,
            commands: [],
            widgets: [],
            references: []
        )
        model.contributions = [attachmentContribution]

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("scan.png")
        try Data([1, 2, 3]).write(to: fileURL)

        var requestCount = await recorder.requestCount()
        await model.importAttachments([fileURL])
        let begin = await recorder.firstRequest(after: requestCount) { request in
            if case .beginSessionFileUpload = request { return true }
            return false
        }
        guard case .beginSessionFileUpload(let beginID, let sessionID, let name, let size, _) = try XCTUnwrap(
            begin
        )
        else { return XCTFail("Expected session file upload start") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(name, "scan.png")
        XCTAssertEqual(size, 3)
        XCTAssertEqual(model.composerAttachments.first?.state, .uploading(0))

        requestCount = await recorder.requestCount()
        model.handle(.sessionFileUploadReady(
            requestID: beginID,
            sessionID: "chat-1",
            uploadID: "upload-1",
            maxChunkBytes: 2
        ))
        let firstChunkRequest = await recorder.firstRequest(
            after: requestCount
        ) { request in
            if case .uploadSessionFileChunk = request { return true }
            return false
        }
        let firstChunk = try XCTUnwrap(firstChunkRequest)
        guard case .uploadSessionFileChunk(let firstID, _, _, let firstOffset, let firstData) = firstChunk else {
            return XCTFail("Expected first session file chunk")
        }
        XCTAssertEqual(firstOffset, 0)
        XCTAssertEqual(firstData, Data([1, 2]))

        requestCount = await recorder.requestCount()
        model.handle(.sessionFileUploadChunkAccepted(
            requestID: firstID,
            sessionID: "chat-1",
            uploadID: "upload-1",
            nextOffset: 2
        ))
        XCTAssertEqual(model.composerAttachments.first?.state, .uploading(2))
        let secondChunkRequest = await recorder.firstRequest(
            after: requestCount
        ) { request in
            if case .uploadSessionFileChunk = request { return true }
            return false
        }
        let secondChunk = try XCTUnwrap(secondChunkRequest)
        guard case .uploadSessionFileChunk(let secondID, _, _, let secondOffset, let secondData) = secondChunk else {
            return XCTFail("Expected second session file chunk")
        }
        XCTAssertEqual(secondOffset, 2)
        XCTAssertEqual(secondData, Data([3]))

        requestCount = await recorder.requestCount()
        model.handle(.sessionFileUploadChunkAccepted(
            requestID: secondID,
            sessionID: "chat-1",
            uploadID: "upload-1",
            nextOffset: 3
        ))
        XCTAssertEqual(model.composerAttachments.first?.state, .uploading(3))
        let finishRequest = await recorder.firstRequest(
            after: requestCount
        ) { request in
            if case .finishSessionFileUpload = request { return true }
            return false
        }
        let finish = try XCTUnwrap(finishRequest)
        guard case .finishSessionFileUpload(let finishID, _, _) = finish else {
            return XCTFail("Expected session file upload finish")
        }
        let attachment = SessionFileReference(
            id: "file-1",
            name: "scan.png",
            size: 3,
            mediaType: "image/png"
        )
        model.handle(.sessionFileUploadCompleted(
            requestID: finishID,
            sessionID: "chat-1",
            file: attachment
        ))
        XCTAssertEqual(model.sessionFiles, [SessionFileRecord(origin: .user, file: attachment)])
        XCTAssertTrue(model.canSendComposer)

        model.contributions = []
        XCTAssertFalse(model.canSendComposer)
        XCTAssertFalse(model.sendMessage())
        XCTAssertEqual(model.toast?.message, "File attachments are not enabled for this chat.")
        model.contributions = [attachmentContribution]

        model.modelChoices = [ModelChoice(
            route: "openai",
            group: "OpenAI",
            model: "text-only",
            reasoningEffort: nil,
            contextWindow: 200_000,
            supportsImageInput: false,
            toolDiscovery: .rebuild
        )]
        XCTAssertTrue(model.canImportAttachments)
        XCTAssertFalse(model.canSendComposer)
        XCTAssertFalse(model.sendMessage())
        XCTAssertEqual(model.toast?.message, "The selected model does not accept image attachments.")
        model.modelChoices = [capableChoice]

        requestCount = await recorder.requestCount()
        XCTAssertTrue(model.sendMessage())
        let submitRequest = await recorder.firstRequest(
            after: requestCount
        ) { request in
            if case .submit = request { return true }
            return false
        }
        let submit = try XCTUnwrap(submitRequest)
        guard case .submit(_, let submission) = submit,
              case .message(let message) = submission.op
        else { return XCTFail("Expected attachment submission") }
        XCTAssertEqual(message.text, "")
        XCTAssertEqual(message.attachments, [attachment])

        model.reduce(
            event: AgentEventRecord(
                submissionId: nil,
                msg: testMessageEvent(text: "", attachments: [attachment])
            ),
            blocks: [],
            preview: nil
        )
        XCTAssertEqual(model.transcript.last?.files, [attachment])
    }

    func testImageImportCreatesComposerThumbnailFromLocalBytes() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.contributions = [fileAttachmentContribution()]

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("pixel.png")
        try tinyPNGData().write(to: fileURL)

        await model.importAttachments([fileURL])

        let attachment = try XCTUnwrap(model.composerAttachments.first)
        let thumbnail = try XCTUnwrap(model.fileThumbnail(for: attachment))
        XCTAssertEqual(thumbnail.width, 1)
        XCTAssertEqual(thumbnail.height, 1)
    }

    func testVideoImportCreatesComposerThumbnailFromLocalFile() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.contributions = [fileAttachmentContribution()]

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("clip.mp4")
        try tinyH264MP4Data().write(to: fileURL)

        await model.importAttachments([fileURL])

        let attachment = try XCTUnwrap(model.composerAttachments.first)
        let thumbnail = try XCTUnwrap(model.fileThumbnail(for: attachment))
        XCTAssertGreaterThan(thumbnail.width, 0)
        XCTAssertGreaterThan(thumbnail.height, 0)
    }

    func testNonImageAttachmentSubmitsWithoutImageModelSupport() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.selectedModelRoute = "text-only"
        model.modelChoices = [ModelChoice(
            route: "text-only",
            group: "OpenAI",
            model: "text-only",
            reasoningEffort: nil,
            contextWindow: 200_000,
            supportsImageInput: false,
            toolDiscovery: .rebuild
        )]
        model.contributions = [fileAttachmentContribution()]
        let attachment = SessionFileReference(
            id: "file-1",
            name: "notes.txt",
            size: 3,
            mediaType: "text/plain"
        )
        model.composerAttachments = [ComposerAttachment(
            id: UUID(),
            name: attachment.name,
            size: attachment.size,
            mediaType: attachment.mediaType,
            state: .uploaded(attachment)
        )]

        XCTAssertTrue(model.canImportAttachments)
        XCTAssertTrue(model.canSendComposer)
        let requestCount = await recorder.requestCount()
        model.sendMessage()
        let request = await recorder.firstRequest(after: requestCount) {
            guard case .submit("chat-1", _) = $0 else { return false }
            return true
        }
        guard case .submit(_, let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected attachment submission")
        }
        guard case .message(let message) = submission.op else {
            return XCTFail("Expected attachment submission")
        }
        XCTAssertEqual(message.attachments, [attachment])
    }

    func testSessionFileUploadRejectsKnownResponseWithWrongPhase() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.contributions = [fileAttachmentContribution()]
        let fileURL = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
            .appendingPathExtension("bin")
        try Data([1, 2, 3]).write(to: fileURL)
        defer { try? FileManager.default.removeItem(at: fileURL) }

        let requestCount = await recorder.requestCount()
        await model.importAttachments([fileURL])
        let beginRequest = await recorder.firstRequest(after: requestCount) {
            if case .beginSessionFileUpload = $0 { return true }
            return false
        }
        guard case .beginSessionFileUpload(let beginID, _, _, _, _) = try XCTUnwrap(beginRequest)
        else { return XCTFail("Expected session file upload start") }

        model.handle(.sessionFileUploadChunkAccepted(
            requestID: beginID,
            sessionID: "chat-1",
            uploadID: "upload-1",
            nextOffset: 0
        ))

        let item = try XCTUnwrap(model.composerAttachments.first)
        guard case .failed(let message) = item.state else {
            return XCTFail("Expected invalid upload to fail")
        }
        XCTAssertEqual(message, "The gateway returned an invalid upload.")
    }

    func testSessionFileUploadRejectsUnexpectedAcknowledgedOffset() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.contributions = [fileAttachmentContribution()]
        let fileURL = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
            .appendingPathExtension("bin")
        try Data([1, 2, 3]).write(to: fileURL)
        defer { try? FileManager.default.removeItem(at: fileURL) }

        var requestCount = await recorder.requestCount()
        await model.importAttachments([fileURL])
        let beginRequest = await recorder.firstRequest(after: requestCount) {
            if case .beginSessionFileUpload = $0 { return true }
            return false
        }
        guard case .beginSessionFileUpload(let beginID, _, _, _, _) = try XCTUnwrap(beginRequest)
        else { return XCTFail("Expected session file upload start") }
        requestCount = await recorder.requestCount()
        model.handle(.sessionFileUploadReady(
            requestID: beginID,
            sessionID: "chat-1",
            uploadID: "upload-1",
            maxChunkBytes: 2
        ))
        let chunkRequest = await recorder.firstRequest(after: requestCount) {
            if case .uploadSessionFileChunk = $0 { return true }
            return false
        }
        guard case .uploadSessionFileChunk(let chunkID, _, _, _, _) = try XCTUnwrap(chunkRequest)
        else { return XCTFail("Expected session file chunk") }

        model.handle(.sessionFileUploadChunkAccepted(
            requestID: chunkID,
            sessionID: "chat-1",
            uploadID: "upload-1",
            nextOffset: 1
        ))

        let item = try XCTUnwrap(model.composerAttachments.first)
        guard case .failed(let message) = item.state else {
            return XCTFail("Expected invalid offset to fail")
        }
        XCTAssertEqual(message, "The gateway returned an invalid upload offset.")
        let finalRequests = await recorder.requests()
        XCTAssertEqual(finalRequests.filter {
            if case .uploadSessionFileChunk = $0 { return true }
            return false
        }.count, 1)
    }

    func testAttachmentMessageLimitIncludesUploadedFiles() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.selectedModelRoute = "openai"
        model.modelChoices = [ModelChoice(
            route: "openai",
            group: "OpenAI",
            model: "gpt-5.6-sol",
            reasoningEffort: "high",
            contextWindow: 200_000,
            supportsImageInput: true,
            toolDiscovery: .native
        )]
        model.contributions = [FrontendContribution(
            capability: "files",
            acceptsFileAttachments: true,
            count: nil,
            commands: [],
            widgets: [],
            references: []
        )]
        let fileSize = maximumClientComposerAttachmentBytes / 4
        model.composerAttachments = (0..<4).map { index in
            let attachment = SessionFileReference(
                id: "file-\(index)",
                name: "file-\(index).bin",
                size: fileSize,
                mediaType: "application/octet-stream"
            )
            return ComposerAttachment(
                id: UUID(),
                name: attachment.name,
                size: attachment.size,
                mediaType: attachment.mediaType,
                state: .uploaded(attachment)
            )
        }

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("extra.bin")
        try Data([1]).write(to: fileURL)

        await model.importAttachments([fileURL])

        XCTAssertEqual(model.composerAttachments.count, 4)
        let requests = await recorder.requests()
        XCTAssertTrue(requests.isEmpty)
        XCTAssertEqual(model.toast?.message, "Attachments in one message are limited to 250 MiB total.")
    }

    func testAttachmentImportAcceptsLargeFileWithinLimit() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.contributions = [fileAttachmentContribution()]

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("large-video.mp4")
        try Data(repeating: 0, count: 50 * 1024 * 1024).write(to: fileURL)

        await model.importAttachments([fileURL])

        let request = await recorder.firstRequest(after: 0) { request in
            if case .beginSessionFileUpload = request { return true }
            return false
        }
        guard case .beginSessionFileUpload(_, let sessionID, let name, let size, let mediaType) =
            try XCTUnwrap(request)
        else { return XCTFail("Expected large attachment upload start") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(name, "large-video.mp4")
        XCTAssertEqual(size, 50 * 1024 * 1024)
        XCTAssertEqual(mediaType, "video/mp4")
    }

    func testAttachmentImportRejectsFileOver250MiB() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.contributions = [fileAttachmentContribution()]

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("too-large-video.mp4")
        XCTAssertTrue(FileManager.default.createFile(atPath: fileURL.path, contents: nil))
        let file = try FileHandle(forWritingTo: fileURL)
        try file.truncate(atOffset: UInt64(maximumClientAttachmentBytes + 1))
        try file.close()

        await model.importAttachments([fileURL])

        XCTAssertTrue(model.composerAttachments.isEmpty)
        XCTAssertEqual(model.toast?.message, "Attachments are limited to 250 MiB each.")
        let requests = await recorder.requests()
        XCTAssertTrue(requests.isEmpty)
    }

}
