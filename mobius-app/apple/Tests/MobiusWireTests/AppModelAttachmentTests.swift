import Foundation
import XCTest

@MainActor
extension AppModelTests {
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
            supportsImageInput: true
        )
        model.modelChoices = [capableChoice]
        let attachmentContribution = FrontendContribution(
            capability: "files",
            acceptsFileAttachments: true,
            count: nil,
            commands: [],
            widgets: [],
            references: [],
            activeInput: nil
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
        model.sendMessage()
        XCTAssertEqual(model.toast?.message, "File attachments are not enabled for this chat.")
        model.contributions = [attachmentContribution]

        model.modelChoices = [ModelChoice(
            route: "openai",
            group: "OpenAI",
            model: "text-only",
            reasoningEffort: nil,
            contextWindow: 200_000,
            supportsImageInput: false
        )]
        XCTAssertTrue(model.canImportAttachments)
        XCTAssertFalse(model.canSendComposer)
        model.sendMessage()
        XCTAssertEqual(model.toast?.message, "The selected model does not accept image attachments.")
        model.modelChoices = [capableChoice]

        requestCount = await recorder.requestCount()
        model.sendMessage()
        let submitRequest = await recorder.firstRequest(
            after: requestCount
        ) { request in
            if case .submit = request { return true }
            return false
        }
        let submit = try XCTUnwrap(submitRequest)
        guard case .submit(_, let submission) = submit,
              case .userInput(let text, let attachments) = submission.op
        else { return XCTFail("Expected attachment submission") }
        XCTAssertEqual(text, "")
        XCTAssertEqual(attachments, [attachment])

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("user_message"),
                "message": .string(""),
                "attachments": .array([.object([
                    "id": .string("file-1"),
                    "name": .string("scan.png"),
                    "size": .number(3),
                    "mediaType": .string("image/png")
                ])]),
                "messageTarget": .null
            ])),
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
            supportsImageInput: false
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
        guard case .userInput(_, let attachments) = submission.op else {
            return XCTFail("Expected attachment submission")
        }
        XCTAssertEqual(attachments, [attachment])
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
            supportsImageInput: true
        )]
        model.contributions = [FrontendContribution(
            capability: "files",
            acceptsFileAttachments: true,
            count: nil,
            commands: [],
            widgets: [],
            references: [],
            activeInput: nil
        )]
        let fileSize: Int64 = 25 * 1024 * 1024
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
        XCTAssertEqual(model.toast?.message, "Attachments in one message are limited to 100 MiB total.")
    }

    func testAttachmentImportAccepts50MiBFile() async throws {
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

    func testAttachmentImportRejectsFileOver50MiB() async throws {
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
        try Data(repeating: 0, count: 50 * 1024 * 1024 + 1).write(to: fileURL)

        await model.importAttachments([fileURL])

        XCTAssertTrue(model.composerAttachments.isEmpty)
        XCTAssertEqual(model.toast?.message, "Attachments are limited to 50 MiB each.")
        let requests = await recorder.requests()
        XCTAssertTrue(requests.isEmpty)
    }

}
