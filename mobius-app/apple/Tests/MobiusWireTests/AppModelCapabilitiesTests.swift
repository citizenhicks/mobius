import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testSimpleCronScheduleRecognizesEditorModes() {
        XCTAssertEqual(
            simpleCronSchedule("30 14 * * *"),
            SimpleCronSchedule(minute: 30, hour: 14, weekday: nil)
        )
        XCTAssertEqual(
            simpleCronSchedule("0 9 * * 1"),
            SimpleCronSchedule(minute: 0, hour: 9, weekday: 1)
        )
        XCTAssertNil(simpleCronSchedule("0 9 * * 1-5"))
    }

    func testScheduledTaskManagementIsGlobal() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        let task = CronTask(
            id: "cron-1",
            sourceSessionId: "chat-1",
            task: "Historical task",
            schedule: .cron("0 9 * * *"),
            endsAt: nil,
            enabled: true,
            finished: false,
            nextRunAt: nil
        )
        model.runCron(task)
        model.deleteCron(task)
        model.updateCron(
            task,
            sourceSessionID: "chat-1",
            instructions: "Updated task",
            schedule: .cron("0 10 * * *"),
            endsAt: nil,
            enabled: false
        )
        model.createCron(
            sourceSessionID: "chat-1",
            task: "New task",
            schedule: .interval(seconds: 120),
            endsAt: nil
        )

        model.refreshCron()
        let requestsArrived = await eventually { await recorder.requestCount() == 6 }
        XCTAssertTrue(requestsArrived)
        let readRequests = await recorder.requests()
        XCTAssertTrue(readRequests.contains { if case .listCron = $0 { true } else { false } })
        XCTAssertTrue(readRequests.contains { if case .listCronHistory = $0 { true } else { false } })
    }

    func testCronRunPreviewDoesNotMutateSelectedTranscript() throws {
        let model = try model()
        model.selectedSessionID = "chat-1"
        model.transcript = [TranscriptEntry(
            id: "selected",
            text: "Selected chat",
            kind: .user,
            format: "plain_text",
            pending: false
        )]
        let task = CronTask(
            id: "cron-1",
            sourceSessionId: "chat-1",
            task: "Review nightly",
            schedule: .interval(seconds: 120),
            endsAt: nil,
            enabled: true,
            finished: false,
            nextRunAt: nil
        )
        let run = CronRun(
            id: "run-1",
            taskId: task.id,
            sourceSessionId: task.sourceSessionId,
            startedAt: 100,
            finishedAt: nil,
            status: .running,
            sessionId: nil,
            message: nil
        )
        model.cronRunPreviewRequestID = "preview-1"
        model.applyCronRunPreview(CronRunPreview(
            requestID: "preview-1",
            task: task,
            run: run,
            records: [RecordedEvent(
                sequence: 1,
                recordedAtMs: 1_000,
                event: AgentEventRecord(submissionId: nil, msg: .object([
                    "type": .string("user_message"),
                    "message": .string("Scheduled transcript"),
                    "attachments": .array([]),
                    "messageTarget": .null
                ])),
                streamMetrics: [],
                blocks: [],
                preview: nil
            )],
            nextBeforeSequence: nil
        ))

        XCTAssertEqual(model.transcript.map(\.text), ["Selected chat"])
        XCTAssertEqual(model.cronRunPreviewEntries.map(\.text), ["Scheduled transcript"])
        XCTAssertEqual(model.presentedCronRun?.id, "run-1")
    }

    func testDisabledScratchpadActionDoesNotSubmitUntilEnabled() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.selectedSessionID = "chat-1"
        model.middlewareFeatures = [MiddlewareFeature(
            id: "scratchpad",
            label: "Scratchpad",
            description: "Durable notes",
            required: false,
            settings: []
        )]
        var config = composition()
        model.agentSnapshot = VersionedAgentConfig(revision: 1, config: config)
        let forget = AgentOperation.capabilityCommand(
            capability: "scratchpad",
            command: "scratchpad",
            arguments: "forget session note-1",
            input: nil,
            target: nil
        )

        model.submitFrontendOperation(forget)
        let disabledRequestCount = await recorder.requestCount()
        XCTAssertEqual(disabledRequestCount, 0)

        config.middleware.enabled.insert("scratchpad")
        model.agentSnapshot = VersionedAgentConfig(revision: 2, config: config)
        model.submitFrontendOperation(forget)
        let request = await recorder.firstRequest(after: 0) {
            if case .submit = $0 { return true }
            return false
        }
        guard case .submit(let sessionID, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected scratchpad submission")
        }
        XCTAssertEqual(sessionID, "chat-1")
    }

    func testGlobalScratchpadWorksWithoutASelectedChat() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        let contribution = FrontendContribution(
            capability: "scratchpad",
            acceptsFileAttachments: false,
            count: 1,
            commands: [],
            widgets: [FrontendWidget(
                id: "navigation",
                slot: .navigation,
                text: "Scratchpad",
                tone: "neutral",
                symbol: "brain",
                iconOnly: false,
                progress: nil,
                content: .actionList(title: "Global Scratchpad", items: []),
                action: nil
            )],
            references: [],
            activeInput: nil
        )
        model.applyGatewayCatalog(ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: composition()),
            contributions: [contribution]
        ))
        model.connectionState = .ready

        XCTAssertNil(model.selectedSessionID)
        XCTAssertEqual(model.globalScratchpadWidget?.title, "Global Scratchpad")

        model.refreshGlobalScratchpad()
        let request = await recorder.firstRequest(after: 0) {
            if case .submitGlobalScratchpad = $0 { return true }
            return false
        }
        guard case .submitGlobalScratchpad(_, let operation) = try XCTUnwrap(request),
              case .capabilityCommand(
                let capability,
                let command,
                let arguments,
                _,
                let target
              ) = operation
        else { return XCTFail("Expected a gateway-scoped scratchpad refresh") }
        XCTAssertEqual([capability, command, arguments], ["scratchpad", "scratchpad", "refresh"])
        XCTAssertNil(target)
    }


    func testContributionCatalogReferencesAndWidgetsAreGeneric() throws {
        let model = try model()
        model.contributions = [FrontendContribution(
            capability: "tasks",
            acceptsFileAttachments: false,
            count: 3,
            commands: [],
            widgets: [
                FrontendWidget(
                    id: "count",
                    slot: .header,
                    text: "3 tasks",
                    tone: "success",
                    symbol: nil,
                    iconOnly: false,
                    progress: nil,
                    content: nil,
                    action: nil
                ),
                FrontendWidget(
                    id: "fork",
                    slot: .messageActions,
                    text: "Fork chat",
                    tone: "neutral",
                    symbol: "branch",
                    iconOnly: true,
                    progress: nil,
                    content: nil,
                    action: .capabilityCommand(
                        capability: "sessions",
                        command: "fork",
                        arguments: "",
                        input: nil,
                        target: nil
                    )
                ),
                FrontendWidget(
                    id: "journal",
                    slot: .navigation,
                    text: "Journal",
                    tone: "neutral",
                    symbol: "brain",
                    iconOnly: false,
                    progress: nil,
                    content: nil,
                    action: nil
                ),
                FrontendWidget(
                    id: "journal-menu",
                    slot: .chatMenu,
                    text: "Open journal",
                    tone: "neutral",
                    symbol: "brain",
                    iconOnly: false,
                    progress: nil,
                    content: nil,
                    action: nil
                )
            ],
            references: [FrontendReference(trigger: "$", value: "planning", description: "Planning skill")],
            activeInput: nil
        )]
        model.mountedWidgets = model.contributions.flatMap { contribution in
            contribution.widgets.map {
                MountedWidget(capability: contribution.capability, widget: $0)
            }
        }

        XCTAssertEqual(model.headerWidgets.first?.widget.text, "3 tasks")
        XCTAssertEqual(model.messageActionWidgets.first?.widget.text, "Fork chat")
        XCTAssertEqual(model.navigationWidgets.first?.id, "tasks\u{0}journal")
        XCTAssertEqual(model.chatMenuWidgets.first?.widget.text, "Open journal")
        let text = "Use $plan"
        let suggestions = try XCTUnwrap(model.referenceSuggestions(in: text, cursor: text.endIndex))
        XCTAssertEqual(String(text[suggestions.range]), "$plan")
        XCTAssertEqual(suggestions.matches.first?.replacement, "$planning")
    }

    func testExtensionSkillReferencesCombineGatewayAndSessionContributions() throws {
        let model = try model()
        var config = composition()
        config.extensions.insert("plugin:ponytail")
        model.applyGatewayCatalog(ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: config),
            extensions: [extensionRecord()],
            contributions: [FrontendContribution(
                capability: "gateway-skills",
                acceptsFileAttachments: false,
                count: 2,
                commands: [],
                widgets: [],
                references: [
                    FrontendReference(trigger: "$", value: "global", description: "Global"),
                    FrontendReference(trigger: "$", value: "workspace", description: "Duplicate")
                ],
                activeInput: nil
            )]
        ))
        model.agentSnapshot = VersionedAgentConfig(revision: 1, config: config)
        model.contributions = [FrontendContribution(
            capability: "session-skills",
            acceptsFileAttachments: false,
            count: 3,
            commands: [],
            widgets: [],
            references: [
                FrontendReference(trigger: "$", value: "ponytail", description: "Managed"),
                FrontendReference(trigger: "$", value: "workspace", description: "Workspace"),
                FrontendReference(trigger: "$", value: "project", description: "Project")
            ],
            activeInput: nil
        )]

        XCTAssertEqual(
            model.extensionSkillReferences.map(\.value),
            ["global", "workspace", "project"]
        )

        model.resetGatewayState(preservingDrafts: false)
        XCTAssertTrue(model.gatewayContributions.isEmpty)
    }

    func testSessionSnapshotKeepsStaticWidgetsAndUpsertsDynamicWidgets() throws {
        let model = try model()
        model.selectedSessionID = "chat-1"
        let staticStatus = FrontendWidget(
            id: "status",
            slot: .composerFooter,
            text: "Queued",
            tone: "neutral",
            symbol: "task",
            iconOnly: false,
            progress: nil,
            content: nil,
            action: nil
        )
        let navigation = FrontendWidget(
            id: "tasks",
            slot: .navigation,
            text: "Tasks",
            tone: "neutral",
            symbol: "task",
            iconOnly: false,
            progress: nil,
            content: nil,
            action: nil
        )
        let dynamicStatus = FrontendWidget(
            id: "status",
            slot: .composerFooter,
            text: "Running",
            tone: "warning",
            symbol: "task",
            iconOnly: false,
            progress: nil,
            content: nil,
            action: nil
        )
        let contribution = FrontendContribution(
            capability: "tasks",
            acceptsFileAttachments: false,
            count: 1,
            commands: [],
            widgets: [staticStatus, navigation],
            references: [],
            activeInput: nil
        )

        model.handle(.sessionChanged(sessionReady(
            latestSequence: 1,
            contributions: [contribution],
            widgets: [SessionWidget(capability: "tasks", item: dynamicStatus)]
        )))

        XCTAssertEqual(model.mountedWidgets.count, 2)
        XCTAssertEqual(model.composerFooterWidgets.first?.widget.text, "Running")
        XCTAssertEqual(model.navigationWidgets.first?.widget.text, "Tasks")
    }

    func testMessageActionSubmitsTheClickedHistoryTarget() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.selectedSessionID = "chat-1"
        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("user_message"),
                "message": .string("Fork here"),
                "attachments": .array([]),
                "messageTarget": .object([
                    "checkpointSequence": .number(12),
                    "batchItemCount": .number(3)
                ])
            ])),
            blocks: [],
            history: nil,
            preview: nil
        )
        let target = try XCTUnwrap(model.transcript.first?.messageTarget)
        let widget = MountedWidget(
            capability: "sessions",
            widget: FrontendWidget(
                id: "fork",
                slot: .messageActions,
                text: "Fork chat",
                tone: "neutral",
                symbol: "arrow.triangle.branch",
                iconOnly: true,
                progress: nil,
                content: nil,
                action: .capabilityCommand(
                    capability: "sessions",
                    command: "fork",
                    arguments: "",
                    input: nil,
                    target: nil
                )
            )
        )

        let requestCount = await recorder.requestCount()
        model.submitMessageAction(widget, target: target)
        let request = await recorder.firstRequest(after: requestCount) {
            guard case .submit("chat-1", _) = $0 else { return false }
            return true
        }
        let requests = await recorder.requests()
        guard case .submit(let sessionID, let submission) = try XCTUnwrap(request),
              case .capabilityCommand(
                  let capability,
                  let command,
                  let arguments,
                  let input,
                  let submittedTarget
              ) = submission.op
        else { return XCTFail("Expected a targeted capability command") }
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(capability, "sessions")
        XCTAssertEqual(command, "fork")
        XCTAssertEqual(arguments, "")
        XCTAssertNil(input)
        XCTAssertEqual(submittedTarget, MessageTarget(checkpointSequence: 12, batchItemCount: 3))
    }

    func testWebSearchAbortAndLiveUsageMatchCLI() throws {
        let model = try model()
        let events: [(JSONValue, [RenderedBlock])] = [
            (
                .object([
                    "type": .string("web_search_begin"),
                    "sessionId": .string("chat-1"),
                    "turnId": .string("turn-1"),
                    "modelStepId": .string("step-1"),
                    "callId": .string("search-1")
                ]),
                [RenderedBlock(capability: "web_search", block: FrontendBlock(
                    id: "step-1/search-1",
                    group: "turn-1",
                    update: .replace,
                    state: .pending,
                    role: .webSearch,
                    title: "Searching the web",
                    text: "",
                    symbol: "search",
                    format: "plain_text",
                    tone: "neutral",
                    files: []
                ))]
            ),
            (
                .object([
                    "type": .string("web_search_end"),
                    "sessionId": .string("chat-1"),
                    "turnId": .string("turn-1"),
                    "modelStepId": .string("step-1"),
                    "callId": .string("search-1"),
                    "action": .object([
                        "type": .string("search"),
                        "query": .string("möbius")
                    ])
                ]),
                [RenderedBlock(capability: "web_search", block: FrontendBlock(
                    id: "step-1/search-1",
                    group: "turn-1",
                    update: .replace,
                    state: .complete,
                    role: .webSearch,
                    title: "Searched the web",
                    text: "möbius",
                    symbol: "search",
                    format: "plain_text",
                    tone: "success",
                    files: []
                ))]
            ),
            (
                .object([
                    "type": .string("turn_aborted"),
                    "turnId": .string("turn-1"),
                    "reason": .string("Stopped")
                ]),
                [RenderedBlock(capability: "agent", block: FrontendBlock(
                    id: nil,
                    group: "turn-1",
                    update: .replace,
                    state: .complete,
                    role: .notice,
                    title: "Turn aborted",
                    text: "Stopped",
                    symbol: nil,
                    format: "plain_text",
                    tone: "warning",
                    files: []
                ))]
            ),
            (
                .object([
                    "type": .string("token_count"),
                    "info": .object([
                        "totalTokenUsage": .object([
                            "inputTokens": .number(1_000),
                            "cachedInputTokens": .number(100),
                            "cacheWriteInputTokens": .number(25),
                            "outputTokens": .number(100),
                            "reasoningOutputTokens": .number(50),
                            "totalTokens": .number(1_100)
                        ]),
                        "lastTokenUsage": .object([
                            "inputTokens": .number(40),
                            "cachedInputTokens": .number(20),
                            "cacheWriteInputTokens": .number(5),
                            "outputTokens": .number(10),
                            "reasoningOutputTokens": .number(3),
                            "totalTokens": .number(99)
                        ]),
                        "modelContextWindow": .number(200)
                    ])
                ]),
                []
            ),
        ]
        for (offset, item) in events.enumerated() {
            model.reduce(record: RecordedEvent(
                sequence: UInt64(offset + 1),
                recordedAtMs: Int64(1_000 + offset),
                event: AgentEventRecord(submissionId: nil, msg: item.0),
                streamMetrics: [],
                blocks: item.1,
                preview: nil
            ))
        }

        XCTAssertEqual(model.transcript.map(\.title), ["Searched the web", "Turn aborted"])
        XCTAssertEqual(model.transcript.map(\.text), ["möbius", "Stopped"])
        XCTAssertEqual(model.transcript.map(\.role), [.webSearch, .notice])
        XCTAssertEqual(model.transcript.map(\.tone), ["success", "warning"])
        XCTAssertEqual(model.transcript.map(\.turnTerminal), [false, true])
        let projection = model.transcriptProjection(breakBefore: nil)
        XCTAssertEqual(projection.rows.map(\.kind), [.workedGroup, .activityGroup])
        XCTAssertEqual(projection.rows[0].records.map(\.title), ["Searched the web"])
        XCTAssertEqual(projection.rows[1].records.map(\.title), ["Turn aborted"])
        XCTAssertEqual(model.currentUsage.inputTokens, 1_000)
        XCTAssertEqual(model.lastUsage.cachedInputTokens, 20)
        XCTAssertEqual(model.lastUsage.cacheWriteInputTokens, 5)
        XCTAssertEqual(model.contextTokens, 99)
        XCTAssertEqual(model.modelContextWindow, 200)
    }

    func testContextFillUsesSessionLimit() throws {
        let model = try model()
        model.contextTokens = 100_000
        model.modelContextWindow = 1_000_000
        model.contextLimitTokens = 250_000

        XCTAssertEqual(model.contextLimitTokens, 250_000)
        XCTAssertEqual(model.contextFillFraction, 0.4, accuracy: 0.000_001)
        XCTAssertEqual(model.contextFillPercent, 40)

        model.contextLimitTokens = model.modelContextWindow

        XCTAssertEqual(model.contextLimitTokens, 1_000_000)
        XCTAssertEqual(model.contextFillFraction, 0.1, accuracy: 0.000_001)
        XCTAssertEqual(model.contextFillPercent, 10)
    }

    func testLiveCanonicalAssistantBackfillsTurnIDBeforeTaskCompletion() throws {
        let model = try model()
        model.activeTurnID = nil
        model.transcript = [
            TranscriptEntry(
                id: "prompt",
                text: "Prompt",
                kind: .user,
                format: "plain_text",
                pending: false,
                turnID: "turn-live",
                startsTurn: true,
                recordedAtMs: 1_000
            ),
            TranscriptEntry(
                id: "work",
                text: "Work",
                kind: .event,
                format: "plain_text",
                pending: false,
                turnID: "turn-live",
                recordedAtMs: 1_100
            ),
        ]

        model.reduce(record: RecordedEvent(
            sequence: 1,
            recordedAtMs: 1_200,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("agent_message"),
                "modelStepId": .string("step-live"),
                "phase": .string("final_answer"),
                "message": .string("Answer")
            ])),
            streamMetrics: [],
            blocks: [],
            preview: nil
        ))
        model.reduce(record: RecordedEvent(
            sequence: 2,
            recordedAtMs: 1_250,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("agent_message"),
                "turnId": .string("turn-live"),
                "modelStepId": .string("step-live"),
                "phase": .string("final_answer"),
                "message": .string("Answer")
            ])),
            streamMetrics: [],
            blocks: [],
            preview: nil
        ))
        model.reduce(record: RecordedEvent(
            sequence: 3,
            recordedAtMs: 1_300,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("task_complete"),
                "turnId": .string("turn-live")
            ])),
            streamMetrics: [],
            blocks: [],
            preview: nil
        ))

        XCTAssertEqual(model.transcript.last?.turnID, "turn-live")
        XCTAssertEqual(model.transcript.last?.turnTerminal, true)
        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.user, .workedGroup, .narrative]
        )
    }

    func testSessionSnapshotsDriveActivityAndOnlyUnseenCompletion() throws {
        let model = try model()
        model.applySessions([session(state: .idle)])
        model.selectedSessionID = "chat-1"
        model.destination = .agent
        model.setChatVisible(false)

        model.applySessions([session(state: .running, turnID: "turn-1")])
        XCTAssertTrue(model.runningSessionIDs.contains("chat-1"))
        XCTAssertEqual(model.attentionSessionIDs, ["chat-1"])

        model.applySessions([session(state: .awaitingApproval, turnID: "turn-1")])
        XCTAssertEqual(model.toast?.tone, .warning)
        XCTAssertEqual(model.attentionSessionIDs, ["chat-1"])

        model.applySessions([session(state: .idle, outcome: .completed)])
        XCTAssertFalse(model.runningSessionIDs.contains("chat-1"))
        XCTAssertTrue(model.unreadSessionIDs.contains("chat-1"))
        XCTAssertEqual(model.attentionSessionIDs, ["chat-1"])
        XCTAssertEqual(model.toast?.tone, .success)
        XCTAssertEqual(model.toast?.sessionID, "chat-1")

        model.destination = .chats
        model.navigationPath = [.chat(.session("chat-1"))]
        model.setChatVisible(true)
        XCTAssertFalse(model.unreadSessionIDs.contains("chat-1"))
        XCTAssertTrue(model.attentionSessionIDs.isEmpty)
        model.dismissToast()
        model.applySessions([session(state: .running, turnID: "turn-2")])
        model.applySessions([session(state: .idle, outcome: .completed)])
        XCTAssertNil(model.toast)
    }

    func testFailedSessionSnapshotUsesGatewayMessage() throws {
        let model = try model()
        model.applySessions([session(state: .idle, title: "Review")])
        model.selectedSessionID = "chat-1"
        model.setChatVisible(false)

        model.applySessions([session(state: .running, turnID: "turn-1", title: "Review")])
        model.applySessions([session(
            state: .idle,
            outcome: .failed,
            message: "Provider failed",
            title: "Review"
        )])

        XCTAssertEqual(model.toast?.message, "Review failed: Provider failed.")
        XCTAssertEqual(model.toast?.tone, .error)
        XCTAssertTrue(model.unreadSessionIDs.contains("chat-1"))

        model.showToast("Credential saved.", tone: .success)
        XCTAssertEqual(model.toast?.message, "Credential saved.")
        XCTAssertEqual(model.toast?.tone, .success)
    }

    func testAgentEventsDoNotDriveCatalogActivityOrToasts() throws {
        let model = try model()
        model.applySessions([session(state: .idle)])
        model.selectedSessionID = "chat-1"

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("task_started"),
                "turnId": .string("turn-1")
            ])),
            blocks: [],
            preview: nil
        )
        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("error"),
                "message": .string("Provider failed")
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertFalse(model.runningSessionIDs.contains("chat-1"))
        XCTAssertTrue(model.unreadSessionIDs.isEmpty)
        XCTAssertNil(model.toast)
        XCTAssertTrue(model.transcript.isEmpty)
    }

    func testSetupValidationUsesGlobalToast() throws {
        let model = try model()
        model.pairingEndpoint = "tcp://localhost:9191"

        model.pair()

        XCTAssertEqual(model.toast?.message, "Enter the one-time code shown by the gateway.")
        XCTAssertEqual(model.toast?.tone, .error)

        model.dismissToast()
        model.saveProviderCredential()

        XCTAssertEqual(model.toast?.message, "Enter an API key. It will be sent once and never read back.")
        XCTAssertEqual(model.toast?.tone, .error)
    }

    func testPairingSetupPrefillsWithoutPairing() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.showsPairing = false
        model.pairingError = "Old error"

        model.applyPairingSetup(
            "mobius-pair:v1|wss://gateway.example|0123456789abcdef"
        )

        XCTAssertTrue(model.showsPairing)
        XCTAssertEqual(model.pairingEndpoint, "wss://gateway.example")
        XCTAssertEqual(model.pairingCode, "0123456789abcdef")
        XCTAssertNil(model.pairingError)
        let requests = await recorder.requests()
        XCTAssertTrue(requests.isEmpty)
    }

    func testPairingURLPrefillsWithoutPairing() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }

        model.applyPairingURL(try XCTUnwrap(URL(string:
            "mobius://pair?endpoint=wss%3A%2F%2Fgateway.example&code=0123456789abcdef"
        )))

        XCTAssertTrue(model.showsPairing)
        XCTAssertEqual(model.pairingEndpoint, "wss://gateway.example")
        XCTAssertEqual(model.pairingCode, "0123456789abcdef")
        XCTAssertNil(model.pairingError)
        let requests = await recorder.requests()
        XCTAssertTrue(requests.isEmpty)
    }

}
