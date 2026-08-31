import Foundation
import Observation
import XCTest

@MainActor
extension AppModelTests {
    func testLastTurnDiffUsesOnlyCompletedTurnToolPatches() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        func entry(
            _ id: String,
            _ text: String,
            turnID: String,
            role: FrontendBlockRole = .tool,
            terminal: Bool = false
        ) -> TranscriptEntry {
            TranscriptEntry(
                id: id,
                text: text,
                kind: terminal ? .assistant : .event,
                role: terminal ? nil : role,
                format: terminal ? "plain_text" : "unified_diff",
                pending: false,
                turnID: turnID,
                turnTerminal: terminal
            )
        }
        func patch(_ path: String) -> String {
            "--- \(path)\n+++ \(path)\n@@ -1 +1 @@\n-old\n+new"
        }
        let priorFinal = entry("prior-final", "Done", turnID: "turn-1", terminal: true)
        let firstLatestFinal = entry(
            "latest-final-1",
            "Almost done",
            turnID: "turn-2",
            terminal: true
        )
        let latestFinal = entry("latest-final-2", "Done", turnID: "turn-2", terminal: true)
        let activePatch = entry("active-patch", patch("Active.swift"), turnID: "turn-3")
        model.transcript = [
            entry("prior-patch", patch("prior.swift"), turnID: "turn-1"),
            priorFinal,
            entry("latest-patch-1", patch("First.swift"), turnID: "turn-2"),
            entry("latest-artifact", patch("Artifact.swift"), turnID: "turn-2", role: .artifact),
            entry("latest-patch-2", patch("Second.swift"), turnID: "turn-2"),
            firstLatestFinal,
            latestFinal,
            activePatch
        ]

        model.showFiles(.lastTurn)
        let requestCount = await recorder.requestCount()
        let document = UnifiedDiffDocument(model.lastTurnDiff)
        let priorDocument = UnifiedDiffDocument(model.turnDiff(for: priorFinal))

        XCTAssertEqual(document.files.map(\.path), ["First.swift", "Second.swift"])
        XCTAssertEqual(priorDocument.files.map(\.path), ["prior.swift"])
        XCTAssertEqual(model.turnDiff(for: latestFinal), model.lastTurnDiff)
        XCTAssertTrue(model.turnDiff(for: firstLatestFinal).isEmpty)
        XCTAssertTrue(model.turnDiff(for: activePatch).isEmpty)
        XCTAssertEqual(model.modifiedFilesScope, .lastTurn)
        XCTAssertEqual(requestCount, 0)
    }

    func testFilesInspectorRequestsTheSelectedCollection() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"

        var requestCount = await recorder.requestCount()
        model.showFiles(.unstaged)
        XCTAssertEqual(model.filesInspectorTab, .modified)
        XCTAssertEqual(model.modifiedFilesScope, .unstaged)
        let unstagedRequest = await recorder.firstRequest(after: requestCount) {
            if case .getGitDiff = $0 { return true }
            return false
        }
        let unstagedRequests = await recorder.requests()
        guard case let .getGitDiff(unstagedRequestID, sessionID, diffScope) =
            try XCTUnwrap(unstagedRequest)
        else { return XCTFail("Expected unstaged Git diff") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(diffScope, .unstaged)
        XCTAssertFalse(unstagedRequests.contains {
            if case .listWorkspaceFiles = $0 { return true }
            return false
        })
        model.handle(.gitDiff(
            requestID: unstagedRequestID,
            sessionID: "chat-1",
            scope: .unstaged,
            diff: "unstaged diff"
        ))
        XCTAssertFalse(model.isLoadingGitDiff)

        requestCount = await recorder.requestCount()
        model.selectModifiedFilesScope(.staged)
        let stagedRequest = await recorder.firstRequest(after: requestCount) {
            guard case .getGitDiff(_, "chat-1", .staged) = $0 else { return false }
            return true
        }
        guard case let .getGitDiff(stagedRequestID, _, .staged) = try XCTUnwrap(stagedRequest)
        else { return XCTFail("Expected staged Git diff") }
        model.handle(.gitDiff(
            requestID: stagedRequestID,
            sessionID: "chat-1",
            scope: .staged,
            diff: "staged diff"
        ))
        XCTAssertEqual(model.stagedGitDiff, "staged diff")
        XCTAssertEqual(model.gitDiff, "unstaged diff")
        XCTAssertFalse(model.isLoadingStagedGitDiff)

        requestCount = await recorder.requestCount()
        model.selectModifiedFilesScope(.committed)
        let committedRequest = await recorder.firstRequest(after: requestCount) {
            guard case .getGitDiff(_, "chat-1", .committed) = $0 else { return false }
            return true
        }
        guard case let .getGitDiff(committedRequestID, _, .committed) =
            try XCTUnwrap(committedRequest)
        else { return XCTFail("Expected committed Git diff") }
        model.handle(.gitDiff(
            requestID: committedRequestID,
            sessionID: "chat-1",
            scope: .committed,
            diff: "committed diff"
        ))
        XCTAssertEqual(model.committedGitDiff, "committed diff")
        XCTAssertEqual(model.gitDiff, "unstaged diff")
        XCTAssertFalse(model.isLoadingCommittedGitDiff)

        requestCount = await recorder.requestCount()
        model.selectFilesInspectorTab(.allFiles)
        let allRequest = await recorder.firstRequest(after: requestCount) {
            if case .listWorkspaceFiles = $0 { return true }
            return false
        }
        guard case .listWorkspaceFiles(_, _, let allScope) = try XCTUnwrap(allRequest)
        else { return XCTFail("Expected all workspace files") }
        XCTAssertEqual(allScope, .all)

        requestCount = await recorder.requestCount()
        model.selectFilesInspectorTab(.chatFiles)
        let filesRequest = await recorder.firstRequest(after: requestCount) {
            if case .listSessionFiles(_, "chat-1") = $0 { return true }
            return false
        }
        XCTAssertNotNil(filesRequest)

        requestCount = await recorder.requestCount()
        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("turn_complete")
            ])),
            blocks: [],
            preview: nil
        )
        let refreshedFilesRequest = await recorder.firstRequest(after: requestCount) {
            if case .listSessionFiles(_, "chat-1") = $0 { return true }
            return false
        }
        XCTAssertNotNil(refreshedFilesRequest)
    }

    func testIPadLayoutAndSidebarTogglePolicies() {
        XCTAssertTrue(MobiusLayout.usesIPadLayout(platform: .ipados))
        XCTAssertFalse(MobiusLayout.usesIPadLayout(platform: .ios))
        XCTAssertEqual(
            MobiusLayout.toggledSplitSidebarVisibility(from: .all),
            .detailOnly
        )
        XCTAssertEqual(
            MobiusLayout.toggledSplitSidebarVisibility(from: .detailOnly),
            .all
        )
    }

    func testWorkspaceCatalogRetainsPartialResultsWhenTruncated() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.showFiles(.allFiles)
        let request = await recorder.firstRequest(after: 0) {
            if case .listWorkspaceFiles = $0 { return true }
            return false
        }
        guard case .listWorkspaceFiles(let requestID, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected workspace file request") }
        let file = WorkspaceFileRecord(path: "Sources/App.swift", size: 3)

        model.handle(.workspaceFiles(
            requestID: requestID,
            sessionID: "chat-1",
            files: [file],
            truncated: true
        ))

        XCTAssertEqual(model.workspaceFiles, [file])
        XCTAssertTrue(model.workspaceFilesTruncated)
        XCTAssertFalse(model.isLoadingWorkspaceFiles)
    }

    func testWorkspaceReferencesUseCLIFuzzyRankingAndReplacement() throws {
        let model = try model()
        model.workspaceFiles = [
            WorkspaceFileRecord(path: "examples/application.txt", size: 1),
            WorkspaceFileRecord(path: "Sources/App.swift", size: 1),
            WorkspaceFileRecord(path: "docs/My App.md", size: 1),
            WorkspaceFileRecord(path: "src/main.rs", size: 1)
        ]

        let prefix = "Review @app"
        let prefixSuggestions = try XCTUnwrap(model.referenceSuggestions(
            in: prefix,
            cursor: prefix.endIndex
        ))
        XCTAssertEqual(prefixSuggestions.matches.first?.label, "@Sources/App.swift")
        XCTAssertEqual(prefixSuggestions.matches.first?.replacement, "Sources/App.swift")

        let spaced = "Review @my"
        let spacedSuggestions = try XCTUnwrap(model.referenceSuggestions(
            in: spaced,
            cursor: spaced.endIndex
        ))
        XCTAssertEqual(spacedSuggestions.matches.first?.label, "@docs/My App.md")
        XCTAssertEqual(spacedSuggestions.matches.first?.replacement, "\"docs/My App.md\"")

        let fuzzy = "Review @smr"
        let fuzzySuggestions = try XCTUnwrap(model.referenceSuggestions(
            in: fuzzy,
            cursor: fuzzy.endIndex
        ))
        XCTAssertEqual(fuzzySuggestions.matches.first?.label, "@src/main.rs")
    }

    func testWorkspaceSourceFileUsesTextPreview() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.showsInspector = true
        let contents = "let answer = 42\n"
        let data = Data(contents.utf8)
        let file = WorkspaceFileRecord(path: "Sources/App.swift", size: UInt64(data.count))

        let requestCount = await recorder.requestCount()
        model.previewWorkspaceFile(file)
        let readRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .readWorkspaceFile = request { return true }
            return false
        }
        guard case .readWorkspaceFile(let readID, _, let path, let offset, _) = try XCTUnwrap(
            readRequest
        )
        else { return XCTFail("Expected workspace file read request") }
        XCTAssertEqual(path, file.path)
        XCTAssertEqual(offset, 0)
        let previewFinished = expectation(description: "Source preview finished")
        withObservationTracking {
            _ = model.textFilePreview
        } onChange: {
            previewFinished.fulfill()
        }
        model.handle(.workspaceFileChunk(
            requestID: readID,
            sessionID: "chat-1",
            path: file.path,
            offset: 0,
            data: data,
            nextOffset: nil
        ))
        await fulfillment(of: [previewFinished], timeout: 1)

        XCTAssertEqual(model.textFilePreview?.contents, contents)
        XCTAssertEqual(model.textFilePreview?.workspaceSessionID, "chat-1")
        XCTAssertEqual(model.textFilePreview?.workspacePath, file.path)
        XCTAssertNil(model.previewURL)
        XCTAssertFalse(model.showsInspector)
        model.closeFilePresentation()
        XCTAssertTrue(model.showsInspector)
    }

    func testWorkspaceFileSaveUsesTheAuthenticatedFileChannel() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.createWorkspaceFile()

        XCTAssertEqual(model.textFilePreview?.workspacePath, "")
        model.saveWorkspaceFile(
            sessionID: "chat-1",
            path: ".env",
            content: "TOKEN=secret\n"
        )
        let write = await recorder.firstRequest(after: 0) {
            if case .writeWorkspaceFile = $0 { return true }
            return false
        }
        guard case .writeWorkspaceFile(let requestID, let sessionID, let path, let content) =
            try XCTUnwrap(write)
        else { return XCTFail("Expected workspace file write") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(path, ".env")
        XCTAssertEqual(content, "TOKEN=secret\n")
        XCTAssertTrue(model.isSavingWorkspaceFile)

        model.handle(.accepted(requestID: requestID))

        XCTAssertFalse(model.isSavingWorkspaceFile)
        XCTAssertNil(model.textFilePreview)
        let refresh = await recorder.firstRequest(after: 1) {
            guard case .listWorkspaceFiles(_, "chat-1", .all) = $0 else { return false }
            return true
        }
        XCTAssertNotNil(refresh)
    }

    func testActiveRunAllowsCreatingWorkspaceFileDraft() throws {
        let model = try model()
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.activeTurnID = "turn-1"

        model.createWorkspaceFile()

        XCTAssertEqual(model.textFilePreview?.workspaceSessionID, "chat-1")
        XCTAssertEqual(model.textFilePreview?.workspacePath, "")
    }

    func testUnsavedWorkspaceTextDraftSurvivesTransientConnectionReset() throws {
        let model = try model()
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.createWorkspaceFile()
        let draftID = try XCTUnwrap(model.textFilePreview?.id)
        model.updateWorkspaceFileDraft(id: draftID, path: ".env")
        model.updateWorkspaceFileDraft(id: draftID, contents: "TOKEN=unsaved\n")

        model.connectionEnded(
            generation: model.connectionGeneration,
            message: GatewayWireError.disconnected.localizedDescription
        )
        _ = model.resetGatewayState(preservingDrafts: true, preservingSession: true)

        XCTAssertEqual(model.textFilePreview?.id, draftID)
        XCTAssertEqual(model.textFilePreview?.workspacePath, ".env")
        XCTAssertEqual(model.textFilePreview?.contents, "TOKEN=unsaved\n")
        XCTAssertEqual(model.textFilePreview?.originalWorkspacePath, "")
        XCTAssertEqual(model.textFilePreview?.originalContents, "")
    }

    func testWorkspaceBinaryFileUsesQuickLookPreview() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        let file = WorkspaceFileRecord(path: "image.bin", size: 3)

        let requestCount = await recorder.requestCount()
        model.previewWorkspaceFile(file)
        let request = await recorder.firstRequest(after: requestCount) { request in
            if case .readWorkspaceFile = request { return true }
            return false
        }
        guard case .readWorkspaceFile(let requestID, _, _, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected workspace file read request") }
        let previewFinished = expectation(description: "Binary preview finished")
        withObservationTracking {
            _ = model.previewURL
        } onChange: {
            previewFinished.fulfill()
        }
        model.handle(.workspaceFileChunk(
            requestID: requestID,
            sessionID: "chat-1",
            path: file.path,
            offset: 0,
            data: Data([0, 1, 2]),
            nextOffset: nil
        ))
        await fulfillment(of: [previewFinished], timeout: 1)

        XCTAssertNotNil(model.previewURL)
        XCTAssertNil(model.textFilePreview)
        model.discardFilePresentation()
    }

    func testUnsupportedSessionFileCanBeDownloadedForSharing() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        let data = Data([0, 1, 2, 3])
        let file = SessionFileReference(
            id: "file-1",
            name: "report.xlsx",
            size: Int64(data.count),
            mediaType: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        )

        let requestCount = await recorder.requestCount()
        model.saveOrShareSessionFile(file, sessionID: "chat-1")
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .readSessionFile(_, _, let fileID, _, _) = request else { return false }
            return fileID == file.id
        }
        guard case .readSessionFile(let requestID, let sessionID, let fileID, let offset, _) = try XCTUnwrap(
            request
        )
        else { return XCTFail("Expected session file read request") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(fileID, file.id)
        XCTAssertEqual(offset, 0)

        let shareFinished = expectation(description: "Share download finished")
        withObservationTracking {
            _ = model.sessionFileShareItem
        } onChange: {
            shareFinished.fulfill()
        }
        model.handle(.sessionFileChunk(
            requestID: requestID,
            sessionID: sessionID,
            fileID: fileID,
            offset: 0,
            data: data,
            nextOffset: nil
        ))
        await fulfillment(of: [shareFinished], timeout: 1)

        let shareItem = try XCTUnwrap(model.sessionFileShareItem)
        XCTAssertEqual(shareItem.name, file.name)
        XCTAssertEqual(shareItem.url.lastPathComponent, file.name)
        XCTAssertEqual(try Data(contentsOf: shareItem.url), data)
        XCTAssertNil(model.previewURL)
        XCTAssertNil(model.textFilePreview)
        model.discardFilePresentation()
    }

    func testRemoteImageThumbnailUsesSessionFileChunksWithoutPresentation() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        let data = try tinyPNGData()
        let file = SessionFileReference(
            id: "image-1",
            name: "pixel.png",
            size: Int64(data.count),
            mediaType: "image/png"
        )

        model.requestSessionFileThumbnail(file, sessionID: "chat-1")
        let firstRequest = await recorder.firstRequest(after: 0) {
            guard case .readSessionFile(_, _, let fileID, _, _) = $0 else { return false }
            return fileID == file.id
        }
        guard case .readSessionFile(let firstID, _, _, _, _) = try XCTUnwrap(firstRequest)
        else { return XCTFail("Expected first thumbnail read") }

        let split = data.count / 2
        model.handle(.sessionFileChunk(
            requestID: firstID,
            sessionID: "chat-1",
            fileID: file.id,
            offset: 0,
            data: Data(data[..<split]),
            nextOffset: Int64(split)
        ))
        let secondRequest = await recorder.firstRequest(after: 1) {
            guard case .readSessionFile(_, _, let fileID, let offset, _) = $0 else { return false }
            return fileID == file.id && offset == Int64(split)
        }
        guard case .readSessionFile(let secondID, _, _, _, _) = try XCTUnwrap(secondRequest)
        else { return XCTFail("Expected second thumbnail read") }

        model.handle(.sessionFileChunk(
            requestID: secondID,
            sessionID: "chat-1",
            fileID: file.id,
            offset: Int64(split),
            data: Data(data[split...]),
            nextOffset: nil
        ))

        let thumbnailLoaded = await eventually {
            model.fileThumbnail(for: file, sessionID: "chat-1") != nil
        }
        XCTAssertTrue(thumbnailLoaded)
        XCTAssertNil(model.previewURL)
        XCTAssertNil(model.textFilePreview)
        XCTAssertFalse(model.isLoadingFilePresentation)
    }

    func testCronRunThumbnailUsesTheExecutionSession() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.presentedCronRun = CronRun(
            id: "run-1",
            taskId: "cron-1",
            sourceSessionId: "chat-1",
            startedAt: 100,
            finishedAt: 200,
            status: .succeeded,
            sessionId: "cron-session-1",
            message: nil
        )
        let data = try tinyPNGData()
        let file = SessionFileReference(
            id: "cron-image",
            name: "report.png",
            size: Int64(data.count),
            mediaType: "image/png"
        )

        model.requestSessionFileThumbnail(file, sessionID: "cron-session-1")
        let request = await recorder.firstRequest(after: 0) {
            guard case .readSessionFile(_, let sessionID, let fileID, _, _) = $0 else {
                return false
            }
            return sessionID == "cron-session-1" && fileID == file.id
        }
        guard case .readSessionFile(let requestID, _, _, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected cron thumbnail read") }

        model.handle(.sessionFileChunk(
            requestID: requestID,
            sessionID: "cron-session-1",
            fileID: file.id,
            offset: 0,
            data: data,
            nextOffset: nil
        ))

        let thumbnailLoaded = await eventually {
            model.fileThumbnail(for: file, sessionID: "cron-session-1") != nil
        }
        XCTAssertTrue(thumbnailLoaded)
    }

    func testThumbnailQueueKeepsTheOwningSessionForDuplicateFileIDs() throws {
        let model = try model()
        model.connectionState = .ready
        let file = SessionFileReference(
            id: "shared-id",
            name: "image.png",
            size: 1,
            mediaType: "image/png"
        )

        model.requestSessionFileThumbnail(file, sessionID: "chat-1")
        model.requestSessionFileThumbnail(file, sessionID: "cron-session-1")

        XCTAssertEqual(model.sessionFileThumbnailDownload?.sessionID, "chat-1")
        XCTAssertEqual(model.queuedSessionFileThumbnails.first?.sessionID, "cron-session-1")
        XCTAssertEqual(model.requestedSessionFileThumbnailKeys.count, 2)
        model.cancelSessionFileThumbnailDownloads()
    }

    func testSessionFileThumbnailSourceCapIsExactlyTenMiB() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        let limit: Int64 = 10 * 1024 * 1024
        let eligible = SessionFileReference(
            id: "at-limit",
            name: "image.png",
            size: limit,
            mediaType: "image/png"
        )

        model.requestSessionFileThumbnail(eligible, sessionID: "chat-1")
        let eligibleRequest = await recorder.firstRequest(after: 0) {
            guard case .readSessionFile(_, _, let fileID, _, _) = $0 else { return false }
            return fileID == eligible.id
        }
        XCTAssertNotNil(eligibleRequest)
        model.cancelSessionFileThumbnailDownloads()
        let requestCount = await recorder.requestCount()

        model.requestSessionFileThumbnail(SessionFileReference(
            id: "over-limit",
            name: "large.png",
            size: limit + 1,
            mediaType: "image/png"
        ), sessionID: "chat-1")
        model.requestSessionFileThumbnail(SessionFileReference(
            id: "not-an-image",
            name: "notes.txt",
            size: 4,
            mediaType: "text/plain"
        ), sessionID: "chat-1")
        await Task.yield()

        let finalRequestCount = await recorder.requestCount()
        XCTAssertEqual(finalRequestCount, requestCount)
    }

    func testInvalidImageThumbnailFallsBackSilently() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        let data = Data([1, 2, 3])
        let file = SessionFileReference(
            id: "invalid-image",
            name: "broken.png",
            size: Int64(data.count),
            mediaType: "image/png"
        )

        model.requestSessionFileThumbnail(file, sessionID: "chat-1")
        let request = await recorder.firstRequest(after: 0) {
            guard case .readSessionFile(_, _, let fileID, _, _) = $0 else { return false }
            return fileID == file.id
        }
        guard case .readSessionFile(let requestID, _, _, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected thumbnail read") }
        model.handle(.sessionFileChunk(
            requestID: requestID,
            sessionID: "chat-1",
            fileID: file.id,
            offset: 0,
            data: data,
            nextOffset: nil
        ))

        let thumbnailFinished = await eventually { model.sessionFileThumbnailDownload == nil }
        XCTAssertTrue(thumbnailFinished)
        XCTAssertNil(model.fileThumbnail(for: file, sessionID: "chat-1"))
        XCTAssertNil(model.toast)

        let requestCount = await recorder.requestCount()
        model.requestSessionFileThumbnail(file, sessionID: "chat-1")
        let retry = await recorder.firstRequest(after: requestCount) {
            guard case .readSessionFile(_, _, let fileID, _, _) = $0 else { return false }
            return fileID == file.id
        }
        guard case .readSessionFile(let retryID, _, _, _, _) = try XCTUnwrap(retry)
        else { return XCTFail("Expected thumbnail retry") }
        model.cancelSessionFileThumbnailDownloads()
        model.handle(.rejected(GatewayRejection(
            requestId: retryID,
            code: "file_unavailable",
            message: "File unavailable",
            fatal: false
        )))
        XCTAssertNil(model.toast)
    }

    func testFileThumbnailCacheIsBounded() async throws {
        let model = try model()
        let imageData = try tinyPNGData()
        let generatedThumbnail = await AppModel.downsampledFileThumbnail(from: imageData)
        let thumbnail = try XCTUnwrap(generatedThumbnail)
        for index in 0...32 {
            model.cacheFileThumbnail(
                thumbnail,
                for: .session(sessionID: "chat-1", fileID: "file-\(index)")
            )
        }

        XCTAssertEqual(model.fileThumbnails.count, 32)
        XCTAssertNil(model.fileThumbnails[.session(sessionID: "chat-1", fileID: "file-0")])
    }

    func testSessionThumbnailPersistsAcrossModelRelaunch() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        func store() -> GatewayStore {
            GatewayStore(
                defaults: defaults,
                catalogDirectory: root.appendingPathComponent("Catalogs", isDirectory: true),
                transcriptDirectory: root.appendingPathComponent("Transcripts", isDirectory: true),
                thumbnailDirectory: root.appendingPathComponent("Thumbnails", isDirectory: true),
                draftDirectory: root.appendingPathComponent("Drafts", isDirectory: true)
            )
        }
        let firstStore = store()
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try firstStore.save(account, token: "test-token")
        addTeardownBlock { try await firstStore.remove(account) }
        let data = try tinyPNGData()
        let file = SessionFileReference(
            id: "cached-image",
            name: "pixel.png",
            size: Int64(data.count),
            mediaType: "image/png"
        )
        let firstRecorder = GatewayRequestRecorder()
        let firstModel = AppModel(
            client: GatewayClient(),
            store: firstStore,
            settingsDefaults: defaults,
            requestSender: { request in await firstRecorder.record(request) }
        )
        firstModel.accounts = [account]
        firstModel.selectedAccountID = account.id
        firstModel.connectionState = .ready

        firstModel.requestSessionFileThumbnail(file, sessionID: "chat-1")
        let request = await firstRecorder.firstRequest(after: 0) {
            guard case .readSessionFile(_, _, let fileID, _, _) = $0 else { return false }
            return fileID == file.id
        }
        guard case .readSessionFile(let requestID, _, _, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected thumbnail read") }
        firstModel.handle(
            .sessionFileChunk(
                requestID: requestID,
                sessionID: "chat-1",
                fileID: file.id,
                offset: 0,
                data: data,
                nextOffset: nil
            ))
        let persisted = await eventually {
            await firstStore.loadThumbnail(
                accountID: account.id,
                sessionID: "chat-1",
                fileID: file.id
            ) != nil
        }
        XCTAssertTrue(persisted)

        let secondRecorder = GatewayRequestRecorder()
        let secondModel = AppModel(
            client: GatewayClient(),
            store: store(),
            settingsDefaults: defaults,
            requestSender: { request in await secondRecorder.record(request) }
        )
        secondModel.accounts = [account]
        secondModel.selectedAccountID = account.id
        secondModel.connectionState = .ready
        secondModel.requestSessionFileThumbnail(file, sessionID: "chat-1")

        let restored = await eventually {
            secondModel.fileThumbnail(for: file, sessionID: "chat-1") != nil
        }
        let secondRequestCount = await secondRecorder.requestCount()
        XCTAssertTrue(restored)
        XCTAssertEqual(secondRequestCount, 0)
    }

    func testSessionResetKeepsThumbnailCacheUntilGatewayReset() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        let file = SessionFileReference(
            id: "image-1",
            name: "pixel.png",
            size: 1,
            mediaType: "image/png"
        )
        let image = await AppModel.downsampledFileThumbnail(from: try tinyPNGData())
        model.cacheFileThumbnail(
            try XCTUnwrap(image),
            for: .session(sessionID: "chat-1", fileID: file.id)
        )

        model.resetSessionState()
        XCTAssertNotNil(model.fileThumbnail(for: file, sessionID: "chat-1"))

        model.requestSessionFileThumbnail(file, sessionID: "chat-1")
        await Task.yield()
        let requestCount = await recorder.requestCount()
        XCTAssertEqual(requestCount, 0)

        model.resetGatewayState(preservingDrafts: false)
        XCTAssertNil(model.fileThumbnail(for: file, sessionID: "chat-1"))
    }

    func testTextEncodedImageSessionFileUsesQuickLookPreview() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        let data = Data("<svg/>".utf8)
        let file = SessionFileReference(
            id: "file-1",
            name: "diagram.svg",
            size: Int64(data.count),
            mediaType: "image/svg+xml"
        )

        let requestCount = await recorder.requestCount()
        model.previewSessionFile(file, sessionID: "chat-1")
        let request = await recorder.firstRequest(after: requestCount) {
            guard case .readSessionFile(_, _, let fileID, _, _) = $0 else { return false }
            return fileID == file.id
        }
        guard case .readSessionFile(let requestID, _, _, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected session file read request") }

        let previewFinished = expectation(description: "Image preview finished")
        withObservationTracking {
            _ = model.previewURL
        } onChange: {
            previewFinished.fulfill()
        }
        model.handle(.sessionFileChunk(
            requestID: requestID,
            sessionID: "chat-1",
            fileID: file.id,
            offset: 0,
            data: data,
            nextOffset: nil
        ))
        await fulfillment(of: [previewFinished], timeout: 1)

        XCTAssertEqual(model.previewURL?.pathExtension, "svg")
        XCTAssertNil(model.textFilePreview)
        model.discardFilePresentation()
    }

    func testStaleSessionFileChunkDoesNotCancelNewerDownload() async throws {
        let firstData = Data("first".utf8)
        let secondData = Data("second".utf8)
        let firstFile = SessionFileReference(
            id: "file-1",
            name: "first.txt",
            size: Int64(firstData.count),
            mediaType: "text/plain"
        )
        let secondFile = SessionFileReference(
            id: "file-2",
            name: "second.txt",
            size: Int64(secondData.count),
            mediaType: "text/plain"
        )
        let recorder = GatewayRequestRecorder()
        let firstReadSent = expectation(description: "First file read sent")
        let secondReadSent = expectation(description: "Second file read sent")
        let model = try model(requestSender: { request in
            await recorder.record(request)
            guard case .readSessionFile(_, _, let fileID, _, _) = request else { return }
            if fileID == firstFile.id { firstReadSent.fulfill() }
            if fileID == secondFile.id { secondReadSent.fulfill() }
        })
        model.selectedSessionID = "chat-1"

        model.previewSessionFile(firstFile, sessionID: "chat-1")
        await fulfillment(of: [firstReadSent], timeout: 1)
        guard let firstRequest = await recorder.requests().last(where: {
            guard case .readSessionFile(_, _, let fileID, _, _) = $0 else { return false }
            return fileID == firstFile.id
        }), case .readSessionFile(let firstRequestID, _, _, _, _) = firstRequest
        else { return XCTFail("Expected first session file read") }

        model.previewSessionFile(secondFile, sessionID: "chat-1")
        await fulfillment(of: [secondReadSent], timeout: 1)
        guard let secondRequest = await recorder.requests().last(where: {
            guard case .readSessionFile(_, _, let fileID, _, _) = $0 else { return false }
            return fileID == secondFile.id
        }), case .readSessionFile(let secondRequestID, _, _, _, _) = secondRequest
        else { return XCTFail("Expected second session file read") }
        XCTAssertNotEqual(firstRequestID, secondRequestID)

        model.handle(.sessionFileChunk(
            requestID: firstRequestID,
            sessionID: "chat-1",
            fileID: firstFile.id,
            offset: 0,
            data: firstData,
            nextOffset: nil
        ))
        XCTAssertTrue(model.isLoadingFilePresentation)
        XCTAssertNil(model.toast)

        let previewFinished = expectation(description: "Second file preview finished")
        withObservationTracking {
            _ = model.textFilePreview
        } onChange: {
            previewFinished.fulfill()
        }
        model.handle(.sessionFileChunk(
            requestID: secondRequestID,
            sessionID: "chat-1",
            fileID: secondFile.id,
            offset: 0,
            data: secondData,
            nextOffset: nil
        ))
        await fulfillment(of: [previewFinished], timeout: 1)

        XCTAssertEqual(model.textFilePreview?.name, secondFile.name)
        XCTAssertEqual(model.textFilePreview?.contents, "second")
        XCTAssertFalse(model.isLoadingFilePresentation)
        model.discardFilePresentation()
    }

    func testStaleWorkspaceFileChunkDoesNotCancelNewerDownload() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.selectedSessionID = "chat-1"
        let firstData = Data("first".utf8)
        let secondData = Data("second".utf8)
        let firstFile = WorkspaceFileRecord(path: "first.txt", size: UInt64(firstData.count))
        let secondFile = WorkspaceFileRecord(path: "second.txt", size: UInt64(secondData.count))

        model.previewWorkspaceFile(firstFile)
        try await Task.sleep(for: .milliseconds(20))
        guard let firstRequest = await recorder.requests().last(where: {
            if case .readWorkspaceFile = $0 { return true }
            return false
        }), case .readWorkspaceFile(let firstRequestID, _, _, _, _) = firstRequest
        else { return XCTFail("Expected first workspace file read") }

        model.previewWorkspaceFile(secondFile)
        try await Task.sleep(for: .milliseconds(20))
        guard let secondRequest = await recorder.requests().last(where: {
            if case .readWorkspaceFile = $0 { return true }
            return false
        }), case .readWorkspaceFile(let secondRequestID, _, _, _, _) = secondRequest
        else { return XCTFail("Expected second workspace file read") }
        XCTAssertNotEqual(firstRequestID, secondRequestID)

        model.handle(.workspaceFileChunk(
            requestID: firstRequestID,
            sessionID: "chat-1",
            path: firstFile.path,
            offset: 0,
            data: firstData,
            nextOffset: nil
        ))
        XCTAssertTrue(model.isLoadingFilePresentation)
        XCTAssertNil(model.toast)

        model.handle(.workspaceFileChunk(
            requestID: secondRequestID,
            sessionID: "chat-1",
            path: secondFile.path,
            offset: 0,
            data: secondData,
            nextOffset: nil
        ))
        try await Task.sleep(for: .milliseconds(20))

        XCTAssertEqual(model.textFilePreview?.name, "second.txt")
        XCTAssertEqual(model.textFilePreview?.contents, "second")
        XCTAssertFalse(model.isLoadingFilePresentation)
        model.discardFilePresentation()
    }

}
