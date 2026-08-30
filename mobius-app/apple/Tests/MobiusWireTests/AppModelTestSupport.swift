import Foundation
import Observation
import XCTest

func testMessageEvent(
    author: MessageAuthor = .user,
    delivery: MessageDelivery = .turn,
    text: String,
    attachments: [SessionFileReference] = [],
    messageTarget: MessageTarget? = nil
) -> JSONValue {
    let author: JSONValue = switch author {
    case .user:
        .object(["type": .string("user")])
    case .peer(let messageID, let sessionID, let handle):
        .object([
            "type": .string("peer"),
            "messageId": .string(messageID),
            "sessionId": .string(sessionID),
            "handle": .string(handle)
        ])
    }
    let attachments = attachments.map { file in
        JSONValue.object([
            "id": .string(file.id),
            "name": .string(file.name),
            "size": .number(Double(file.size)),
            "mediaType": .string(file.mediaType)
        ])
    }
    let target: JSONValue = messageTarget.map { target in
        .object([
            "checkpointSequence": .number(Double(target.checkpointSequence)),
            "batchItemCount": .number(Double(target.batchItemCount))
        ])
    } ?? .null
    return .object([
        "type": .string("message"),
        "author": author,
        "delivery": .string(delivery.rawValue),
        "text": .string(text),
        "attachments": .array(attachments),
        "messageTarget": target
    ])
}

func testAssistantMessage(
    turnID: String,
    modelStepID: String,
    phase: String = "final_answer",
    text: String
) -> JSONValue {
    .object([
        "type": .string("assistant_message"),
        "sessionId": .string("chat-1"),
        "turnId": .string(turnID),
        "modelStepId": .string(modelStepID),
        "content": .array([.object([
            "outputIndex": .number(0),
            "partIndex": .number(0),
            "phase": .string(phase),
            "text": .string(text),
            "annotations": .array([]),
        ])]),
        "messageTarget": .null,
    ])
}

actor GatewayRequestRecorder {
    private var recorded: [GatewayRequest] = []

    func record(_ request: GatewayRequest) {
        recorded.append(request)
    }

    func requests() -> [GatewayRequest] {
        recorded
    }

    func requestCount() -> Int {
        recorded.count
    }

    func firstRequest(
        after index: Int,
        matching predicate: @Sendable (GatewayRequest) -> Bool
    ) async -> GatewayRequest? {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: .seconds(1))
        repeat {
            if let request = recorded.dropFirst(index).first(where: predicate) {
                return request
            }
            try? await Task.sleep(for: .milliseconds(5))
        } while clock.now < deadline
        return recorded.dropFirst(index).first(where: predicate)
    }
}

actor AsyncGate {
    private var isOpen = false
    private var waiter: CheckedContinuation<Void, Never>?

    func wait() async {
        guard !isOpen else { return }
        await withCheckedContinuation { waiter = $0 }
    }

    func open() {
        isOpen = true
        waiter?.resume()
        waiter = nil
    }
}

actor GatewayConnectionHarness {
    enum Failure: Error { case unavailable }

    private var attempts = 0
    private var continuation: AsyncThrowingStream<GatewayEnvelope, Error>.Continuation?

    func open(
        _ endpoint: GatewayEndpoint
    ) throws -> AsyncThrowingStream<GatewayEnvelope, Error> {
        _ = endpoint
        attempts += 1
        guard attempts > 1 else { throw Failure.unavailable }
        var continuation: AsyncThrowingStream<GatewayEnvelope, Error>.Continuation!
        let stream = AsyncThrowingStream<GatewayEnvelope, Error> { continuation = $0 }
        self.continuation = continuation
        return stream
    }

    func yield(_ envelope: GatewayEnvelope) {
        continuation?.yield(envelope)
    }

    func fail() {
        continuation?.finish(throwing: Failure.unavailable)
        continuation = nil
    }

    func attemptCount() -> Int { attempts }
}

extension AppModel {
    func reduce(
        event: AgentEventRecord,
        blocks: [FrontendBlock],
        history: [RenderedEventRecord]? = nil,
        preview: RenderedPreview?
    ) {
        _ = history
        let renderedBlocks: [RenderedBlock]
        if blocks.isEmpty,
           event.msg["frontendType"]?.stringValue == "render",
           let capability = event.msg["capability"]?.stringValue,
           let value = event.msg["block"],
           let block = try? FrontendBlock(json: value) {
            renderedBlocks = [RenderedBlock(capability: capability, block: block)]
        } else {
            renderedBlocks = blocks.map { RenderedBlock(capability: "test", block: $0) }
        }
        reduce(record: RecordedEvent(
            sequence: 1,
            recordedAtMs: 1_000,
            event: event,
            streamMetrics: [],
            blocks: renderedBlocks,
            preview: preview
        ))
    }
}

extension GatewayEnvelope {
    static func agentEvent(
        sessionID: String,
        sequence: UInt64,
        event: AgentEventRecord,
        blocks: [FrontendBlock],
        history: [RenderedEventRecord]? = nil,
        preview: RenderedPreview?
    ) -> Self {
        _ = history
        return .agentEvent(
            sessionID: sessionID,
            record: RecordedEvent(
                sequence: sequence,
                recordedAtMs: 1_000,
                event: event,
                streamMetrics: [],
                blocks: blocks.map { RenderedBlock(capability: "test", block: $0) },
                preview: preview
            )
        )
    }
}

@MainActor
final class AppModelTests: XCTestCase {
    func model(
        requestSender: (@MainActor @Sendable (GatewayRequest) async throws -> Void)? = nil,
        titleWriter: ChatTitleWriter? = nil
    ) throws -> AppModel {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(
                defaults: defaults,
                transcriptDirectory: directory,
                draftDirectory: directory.appendingPathComponent("Drafts", isDirectory: true)
            ),
            settingsDefaults: defaults,
            appLockAuthenticator: AppLockAuthenticator(
                method: { .unavailable },
                authenticate: { _ in false }
            ),
            requestSender: requestSender,
            titleWriter: titleWriter
        )
        model.sessionFileLimits = testSessionFileLimits()
        return model
    }

    func eventually(
        timeout: Duration = .seconds(1),
        _ predicate: @MainActor () async -> Bool
    ) async -> Bool {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        repeat {
            if await predicate() { return true }
            try? await Task.sleep(for: .milliseconds(5))
        } while clock.now < deadline
        return await predicate()
    }

    func tinyPNGData() throws -> Data {
        try XCTUnwrap(Data(base64Encoded:
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
        ))
    }

    func composition(systemPrompt: String = "Test") -> AgentComposition {
        AgentComposition(
            provider: ProviderConfig(
                instance: "openai-work",
                provider: "openai_socket",
                model: "gpt-5.6-sol",
                baseUrl: nil,
                reasoningEffort: "high",
                webSearch: .cached
            ),
            middleware: MiddlewareConfig(
                enabled: ["extensions", "subagents"],
                settings: [
                    "context_offloading": ["stale_after_tokens": .integer(50_000)],
                    "subagents": [
                        "model_route": .string("openai_socket::gpt-5.6-sol::high")
                    ]
                ]
            ),
            extensions: [],
            systemPrompt: systemPrompt,
            maxModelSteps: 256
        )
    }

    func providerStatus(
        for config: ProviderConfig,
        models: [ProviderModel] = [],
        label: String = "OpenAI",
        toolDiscovery: ToolDiscoveryMode = .rebuild,
        customEndpointToolDiscovery: ToolDiscoveryMode? = nil
    ) -> ProviderStatus {
        ProviderStatus(
            provider: config.provider,
            label: label,
            symbol: "chat_gpt",
            description: "Test provider",
            auth: .apiKey,
            defaultBaseUrl: config.baseUrl,
            defaultApiKeyEnv: "OPENAI_API_KEY",
            models: models,
            modelIdsConfigurable: false,
            webSearch: webSearchOptions(config.webSearch),
            toolDiscovery: toolDiscovery,
            customEndpointToolDiscovery: customEndpointToolDiscovery
        )
    }

    func webSearchOptions(_ values: HostedWebSearch...) -> [FrontendSettingOption] {
        values.map { value in
            let metadata = switch value {
            case .off: ("Off", "Do not use provider-hosted web search")
            case .cached: ("Cached", "Allow cached provider-hosted search")
            case .live: ("Live", "Allow live provider-hosted search")
            }
            return FrontendSettingOption(
                value: value.rawValue,
                label: metadata.0,
                description: metadata.1
            )
        }
    }

    func testSessionFileLimits() -> SessionFileLimits {
        SessionFileLimits(
            maxAttachmentReferences: 16,
            maxFileBytes: 50 * 1024 * 1024,
            maxSessionFiles: 128,
            maxSessionBytes: 250 * 1024 * 1024,
            maxUploadChunkBytes: 256 * 1024
        )
    }

    func ready(
        defaultConfig: VersionedAgentConfig,
        sessions: [SessionRecord]? = nil,
        extensions: [ExtensionRecord] = [],
        contributions: [FrontendContribution] = []
    ) -> ReadyPayload {
        ReadyPayload(
            machineName: "snowwhite.local",
            sessions: sessions ?? [session(state: .idle)],
            swarms: [],
            providers: [],
            providerInstances: [],
            defaultConfig: defaultConfig,
            models: [],
            modelProviders: [:],
            middlewareFeatures: [],
            extensions: extensions,
            contributions: contributions,
            maxActiveSessions: 4,
            sessionFileLimits: testSessionFileLimits()
        )
    }

    func extensionRecord(
        hooksTrusted: Bool = true
    ) -> ExtensionRecord {
        ExtensionRecord(
            id: "plugin:ponytail",
            capability: "extensions",
            kind: .plugin,
            name: "ponytail",
            description: "Minimal coding guidance.",
            version: "4.9.0",
            source: "https://github.com/DietrichGebert/ponytail.git",
            reference: "main",
            subdirectory: nil,
            resolvedRevision: "0123456789abcdef",
            digest: "abcdef0123456789",
            skills: ["ponytail"],
            hooks: [ExtensionHookRecord(
                event: "pre_tool_use",
                matcher: "shell",
                command: "bin/review",
                timeoutSeconds: 10
            )],
            hooksTrusted: hooksTrusted
        )
    }

    func fileAttachmentContribution() -> FrontendContribution {
        FrontendContribution(
            capability: "files",
            acceptsFileAttachments: true,
            count: nil,
            commands: [],
            widgets: [],
            references: []
        )
    }

    func editableWidget(
        input: String = "Original input",
        capability: String = "notes",
        id: String = "queued"
    ) -> MountedWidget {
        MountedWidget(
            capability: capability,
            widget: FrontendWidget(
                id: id,
                slot: .transcriptTail,
                text: input,
                tone: "neutral",
                symbol: nil,
                iconOnly: false,
                progress: nil,
                content: nil,
                action: .capabilityCommand(
                    capability: capability,
                    command: "edit",
                    arguments: "item-1",
                    input: input,
                    target: nil
                )
            )
        )
    }

    @discardableResult
    func beginComposerEdit(
        in model: AppModel,
        recorder: GatewayRequestRecorder,
        account: GatewayAccount,
        sessionID: String = "chat-1"
    ) async throws -> Submission {
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready
        model.selectedSessionID = sessionID
        model.composer = "Displaced draft"
        let requestCount = await recorder.requestCount()
        model.editWidgetInputInComposer(editableWidget())
        let request = await recorder.firstRequest(after: requestCount) {
            if case .submit = $0 { return true }
            return false
        }
        let submission = try XCTUnwrap(request.flatMap { request -> Submission? in
            guard case .submit(_, let submission) = request else { return nil }
            return submission
        })
        model.reduce(
            event: AgentEventRecord(submissionId: submission.id, msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("remove_widget"),
                "capability": .string("notes"),
                "id": .string("queued")
            ])),
            blocks: [],
            preview: nil
        )
        return submission
    }

    func sessionReady(
        latestSequence: UInt64,
        nextBeforeSequence: UInt64? = nil,
        sessionID: String = "chat-1",
        contributions: [FrontendContribution] = [],
        widgets: [SessionWidget] = [],
        compactionCount: UInt64 = 0,
        runStats: RunStats = RunStats()
    ) -> SessionReadyPayload {
        SessionReadyPayload(
            latestSequence: latestSequence,
            nextBeforeSequence: nextBeforeSequence,
            workspace: WorkspaceInfo(id: "workspace-1", path: "/srv/mobius"),
            git: nil,
            session: SessionConfigured(
                sessionId: sessionID,
                context: SessionContext(
                    tenantId: nil,
                    userId: nil,
                    userName: nil,
                    workspaceId: "workspace-1",
                    workspaceLabel: "/srv/mobius",
                    originLabel: nil
                ),
                model: ModelChanged(
                    route: "openai",
                    model: "gpt-5.6-sol",
                    reasoningEffort: "high",
                    modelContextWindow: 200_000
                )
            ),
            contributions: contributions,
            widgets: widgets,
            toolCount: 0,
            compactionCount: compactionCount,
            contextLimitTokens: 200_000,
            runStats: runStats,
            config: VersionedAgentConfig(revision: 1, config: composition())
        )
    }

    func openNewSession(
        in model: AppModel,
        recorder: GatewayRequestRecorder,
        account: GatewayAccount,
        sessionID: String = "chat-1"
    ) async throws {
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready
        let requestCount = await recorder.requestCount()
        model.chooseWorkspace("/srv/mobius")
        let request = await recorder.firstRequest(after: requestCount) {
            if case .createSession = $0 { return true }
            return false
        }
        guard case .createSession(let requestID, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected a create-session request")
        }
        model.handle(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 0, sessionID: sessionID)
        ))
        model.handle(.sessionReplayComplete(requestID: requestID, sessionID: sessionID))
        guard !model.canCreateSession else { return }
        let ready = expectation(description: "New session finished loading")
        withObservationTracking {
            _ = model.canCreateSession
        } onChange: {
            ready.fulfill()
        }
        if !model.canCreateSession {
            await fulfillment(of: [ready], timeout: 1)
        }
        XCTAssertTrue(model.canCreateSession)
    }

    @discardableResult
    func submitMessage(
        _ text: String,
        in model: AppModel,
        recorder: GatewayRequestRecorder,
        sessionID: String = "chat-1"
    ) async throws -> Submission {
        model.composer = text
        let requestCount = await recorder.requestCount()
        model.sendMessage()
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit(let submittedSessionID, _) = request else { return false }
            return submittedSessionID == sessionID
        }
        let submission = try XCTUnwrap(request.flatMap { request -> Submission? in
            guard case .submit(_, let submission) = request else { return nil }
            return submission
        })
        try await Task.sleep(for: .milliseconds(30))
        return submission
    }

    func renderEvent(
        capability: String = "tools",
        id: String = "result",
        group: String? = "turn",
        append: Bool = false,
        pending: Bool = false,
        title: String = "Tool",
        text: String,
        format: String = "plain_text",
        tone: String = "neutral",
        files: [SessionFileReference] = []
    ) -> AgentEventRecord {
        AgentEventRecord(submissionId: nil, msg: .object([
            "type": .string("frontend"),
            "frontendType": .string("render"),
            "capability": .string(capability),
            "block": .object([
                "id": .string(id),
                "group": group.map(JSONValue.string) ?? .null,
                "update": .string(append ? "append" : "replace"),
                "state": .string(pending ? "pending" : "complete"),
                "role": .string("tool"),
                "title": .string(title),
                "text": .string(text),
                "symbol": .null,
                "format": .string(format),
                "tone": .string(tone),
                "files": .array(files.map { file in
                    .object([
                        "id": .string(file.id),
                        "name": .string(file.name),
                        "size": .number(Double(file.size)),
                        "mediaType": .string(file.mediaType)
                    ])
                })
            ])
        ]))
    }

    func recorded(
        _ sequence: UInt64,
        _ msg: JSONValue,
        blocks: [RenderedBlock] = []
    ) -> RecordedEvent {
        RecordedEvent(
            sequence: sequence,
            recordedAtMs: Int64(sequence * 100),
            event: AgentEventRecord(submissionId: nil, msg: msg),
            streamMetrics: [],
            blocks: blocks,
            preview: nil
        )
    }

    func session(
        sessionID: String = "chat-1",
        state: SessionActivityState,
        outcome: SessionOutcome? = nil,
        message: String? = nil,
        turnID: String? = nil,
        executionStats: ExecutionStats = ExecutionStats(),
        sequence: UInt64 = 1,
        createdAt: Int64 = 100,
        updatedAt: Int64 = 100,
        firstUserMessage: String? = "Review",
        title: String? = nil,
        workspaceID: String = "workspace-1",
        workspaceLabel: String = "/srv/mobius",
        originLabel: String? = nil
    ) -> SessionRecord {
        SessionRecord(
            sessionId: sessionID,
            sessionContext: SessionContext(
                tenantId: nil,
                userId: nil,
                userName: nil,
                workspaceId: workspaceID,
                workspaceLabel: workspaceLabel,
                originLabel: originLabel
            ),
            parentSessionId: nil,
            parentSequence: nil,
            sequence: sequence,
            firstUserMessage: firstUserMessage,
            executionStats: executionStats,
            title: title,
            pinned: false,
            activity: SessionActivity(
                state: state,
                turnId: turnID,
                startedAt: state == .idle ? nil : 100,
                lastOutcome: outcome,
                message: message
            ),
            createdAt: createdAt,
            updatedAt: updatedAt
        )
    }

}
