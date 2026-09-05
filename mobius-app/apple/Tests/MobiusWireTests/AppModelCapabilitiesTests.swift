import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testContributedSlashCommandsUseTheirOwnerAndIdlePolicy() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.contributions = [FrontendContribution(
            capability: "notes",
            acceptsFileAttachments: false,
            count: nil,
            commands: [
                FrontendCommand(name: "inspect_all", arguments: "", description: "Inspect all notes", requiresIdle: true),
                FrontendCommand(name: "inspect", arguments: "<id>", description: "Inspect a note", requiresIdle: true),
                FrontendCommand(name: "status", arguments: "", description: "Show status", requiresIdle: false)
            ],
            widgets: [],
            references: []
        )]

        let suggestions = try XCTUnwrap(model.commandSuggestions(in: "/inspect", cursorOffset: 8))
        XCTAssertEqual(suggestions.matches.first?.replacement, "/inspect ")
        XCTAssertEqual(suggestions.matches.first?.reference.value, "inspect <id>")
        XCTAssertNil(model.commandSuggestions(in: "hello /ins", cursorOffset: 10))
        XCTAssertNil(model.commandSuggestions(in: "/inspect arg", cursorOffset: 12))

        model.activeTurnID = "turn-1"
        model.composer = "/inspect note-1"
        XCTAssertFalse(model.sendMessage())
        XCTAssertEqual(model.composer, "/inspect note-1")

        model.activeTurnID = nil
        XCTAssertTrue(model.sendMessage())
        let request = await recorder.firstRequest(after: 0) {
            if case .submit = $0 { true } else { false }
        }
        guard case .submit("chat-1", let submission) = try XCTUnwrap(request),
              case .capabilityCommand(let capability, let command, let arguments, let input, let target) = submission.op
        else { return XCTFail("Expected a capability command, not a model message") }
        XCTAssertEqual(capability, "notes")
        XCTAssertEqual(command, "inspect")
        XCTAssertEqual(arguments, "note-1")
        XCTAssertNil(input)
        XCTAssertNil(target)
        XCTAssertTrue(model.composer.isEmpty)

        model.activeTurnID = "turn-1"
        model.composer = "/status"
        XCTAssertTrue(model.sendMessage())
        model.composer = "/unknown"
        XCTAssertFalse(model.sendMessage())
        XCTAssertEqual(model.composer, "/unknown")
        model.composer = "/status " + String(repeating: "x", count: maximumComposerBytes)
        XCTAssertFalse(model.sendMessage())
    }

    func testSimpleRoutineScheduleRecognizesEditorModes() {
        XCTAssertEqual(
            simpleRoutineSchedule("30 14 * * *"),
            SimpleRoutineSchedule(minute: 30, hour: 14, weekday: nil)
        )
        XCTAssertEqual(
            simpleRoutineSchedule("0 9 * * 1"),
            SimpleRoutineSchedule(minute: 0, hour: 9, weekday: 1)
        )
        XCTAssertNil(simpleRoutineSchedule("0 9 * * 1-5"))
    }

    func testRoutineManagementIsBotScoped() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        let routine = Routine(
            id: "routine-1",
            botId: "bot-1",
            workspace: "/srv/mobius",
            instructions: "Historical task",
            schedule: .cron("0 9 * * *"),
            endsAt: nil,
            enabled: true,
            finished: false,
            nextRunAt: nil
        )
        model.runRoutine(routine)
        model.deleteRoutine(routine)
        model.updateRoutine(
            routine,
            botID: "bot-1",
            workspace: "/srv/mobius",
            instructions: "Updated task",
            schedule: .cron("0 10 * * *"),
            endsAt: nil,
            enabled: false
        )
        model.createRoutine(
            botID: "bot-1",
            workspace: "/srv/mobius",
            instructions: "New task",
            schedule: .interval(seconds: 120),
            endsAt: nil
        )

        model.refreshRoutines()
        let requestsArrived = await eventually { await recorder.requestCount() == 6 }
        XCTAssertTrue(requestsArrived)
        let readRequests = await recorder.requests()
        XCTAssertTrue(readRequests.contains {
            if case .listRoutines(_, nil) = $0 { true } else { false }
        })
        XCTAssertTrue(readRequests.contains {
            if case .listRoutineHistory = $0 { true } else { false }
        })
    }

    func testCompletedRoutineRunCanBeDeleted() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        let run = RoutineRun(
            id: "run-1",
            routineId: "routine-1",
            botId: "bot-1",
            startedAt: 100,
            finishedAt: 200,
            status: .succeeded,
            sessionId: "session-1",
            message: nil
        )

        model.deleteRoutineRun(run)

        let requestArrived = await eventually { await recorder.requestCount() == 1 }
        XCTAssertTrue(requestArrived)
        let requests = await recorder.requests()
        guard let request = requests.first,
              case .deleteRoutineRun(_, "run-1") = request
        else {
            return XCTFail("Expected routine run deletion")
        }
    }

    func testRoutineRunPreviewDoesNotMutateSelectedTranscript() throws {
        let model = try model()
        model.selectedSessionID = "chat-1"
        model.transcript = [TranscriptEntry(
            id: "selected",
            text: "Selected chat",
            kind: .user,
            format: "plain_text",
            pending: false
        )]
        let routine = Routine(
            id: "routine-1",
            botId: "bot-1",
            workspace: "/srv/mobius",
            instructions: "Review nightly",
            schedule: .interval(seconds: 120),
            endsAt: nil,
            enabled: true,
            finished: false,
            nextRunAt: nil
        )
        let run = RoutineRun(
            id: "run-1",
            routineId: routine.id,
            botId: routine.botId,
            startedAt: 100,
            finishedAt: nil,
            status: .running,
            sessionId: nil,
            message: nil
        )
        model.routineRunPreviewRequestID = "preview-1"
        model.applyRoutineRunPreview(RoutineRunPreview(
            requestID: "preview-1",
            routine: routine,
            run: run,
            records: [RecordedEvent(
                sequence: 1,
                recordedAtMs: 1_000,
                event: AgentEventRecord(
                    submissionId: nil,
                    msg: testMessageEvent(text: "Routine transcript")
                ),
                streamMetrics: [],
                blocks: [],
                preview: nil
            )],
            nextBeforeSequence: nil
        ))

        XCTAssertEqual(model.transcript.map(\.text), ["Selected chat"])
        XCTAssertEqual(model.routineRunPreviewEntries.map(\.text), ["Routine transcript"])
        XCTAssertEqual(model.presentedRoutineRun?.id, "run-1")
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
            references: []
        )
        model.applyGatewayCatalog(ready(
            botDefaults: VersionedAgentConfig(revision: 1, config: composition()),
            contributions: [contribution]
        ))
        model.connectionState = .ready

        XCTAssertNil(model.selectedSessionID)
        XCTAssertEqual(model.navigationWidgets(in: .global).first?.title, "Global Scratchpad")

        model.refreshContributions(scope: .global)
        let request = await recorder.firstRequest(after: 0) {
            if case .getContributions = $0 { return true }
            return false
        }
        guard case .getContributions(_, let scope) = try XCTUnwrap(request)
        else { return XCTFail("Expected a gateway-scoped contribution refresh") }
        XCTAssertEqual(scope, .global)
    }

    func testSwarmScratchpadRefreshAndContributionStayWithSelectedSwarm() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let helper = bot()
        let swarm = SwarmRecord(
            id: "swarm-1",
            title: "Reviewers",
            leaderBotId: helper.id,
            members: [SwarmMemberRecord(botId: helper.id, handle: helper.handle)],
            messages: [],
            updatedAtMs: 100
        )
        model.bots = [helper]
        model.swarms = [swarm]
        model.connectionState = .ready

        model.refreshContributions(scope: .swarm(id: swarm.id))
        let request = await recorder.firstRequest(after: 0) {
            if case .getContributions = $0 { return true }
            return false
        }
        guard case .getContributions(_, let scope) = try XCTUnwrap(request) else {
            return XCTFail("Expected a scoped scratchpad refresh")
        }
        XCTAssertEqual(scope, .swarm(id: swarm.id))

        let contribution = FrontendContribution(
            capability: "scratchpad",
            acceptsFileAttachments: false,
            count: 1,
            commands: [],
            widgets: [FrontendWidget(
                id: "swarm",
                slot: .navigation,
                text: "Scratchpad",
                tone: "neutral",
                symbol: "brain",
                iconOnly: false,
                progress: nil,
                content: .actionList(title: "Swarm Scratchpad", items: []),
                action: nil
            )],
            references: []
        )
        model.handle(.contributionsChanged(
            requestID: "scratchpad-1",
            scope: .swarm(id: swarm.id),
            contributions: [contribution]
        ))

        XCTAssertEqual(model.swarmContributions[swarm.id]?.first?.count, 1)
        XCTAssertEqual(model.navigationWidgets(in: .swarm(id: swarm.id)).first?.title, "Swarm Scratchpad")
        XCTAssertNil(model.navigationWidgets(in: .global).first)
    }

    func testStaleSwarmContributionScopeDoesNotReachGateway() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready

        model.refreshContributions(scope: .swarm(id: "removed-swarm"))

        let requestCount = await recorder.requestCount()
        XCTAssertEqual(requestCount, 0)
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
            references: [FrontendReference(trigger: "$", value: "planning", description: "Planning skill")]
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
            botDefaults: VersionedAgentConfig(revision: 1, config: config),
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
                ]
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
            ]
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
            references: []
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
            event: AgentEventRecord(
                submissionId: nil,
                msg: testMessageEvent(
                    text: "Fork here",
                    messageTarget: MessageTarget(checkpointSequence: 12, batchItemCount: 3)
                )
            ),
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
            event: AgentEventRecord(
                submissionId: nil,
                msg: testAssistantMessage(
                    turnID: "turn-live",
                    modelStepID: "step-live",
                    text: "Answer"
                )
            ),
            streamMetrics: [],
            blocks: [],
            preview: nil
        ))
        model.reduce(record: RecordedEvent(
            sequence: 2,
            recordedAtMs: 1_250,
            event: AgentEventRecord(
                submissionId: nil,
                msg: testAssistantMessage(
                    turnID: "turn-live",
                    modelStepID: "step-live",
                    text: "Answer"
                )
            ),
            streamMetrics: [],
            blocks: [],
            preview: nil
        ))
        model.reduce(record: RecordedEvent(
            sequence: 3,
            recordedAtMs: 1_300,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("turn_complete"),
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
        model.destination = .botDefaults
        model.setChatVisible(false)

        model.applySessions([session(state: .running, turnID: "turn-1")])
        XCTAssertTrue(model.runningSessionIDs.contains("chat-1"))
        XCTAssertEqual(model.attentionSessionIDs, ["chat-1"])

        model.applySessions([session(
            state: .awaitingApproval,
            turnID: "turn-1",
            approvalRequestID: "approval-1"
        )])
        XCTAssertEqual(model.toast?.tone, .warning)
        XCTAssertEqual(model.toast?.target, .session("chat-1"))
        XCTAssertEqual(model.attentionSessionIDs, ["chat-1"])

        model.applySessions([session(state: .idle, outcome: .completed)])
        XCTAssertFalse(model.runningSessionIDs.contains("chat-1"))
        XCTAssertTrue(model.unreadSessionIDs.contains("chat-1"))
        XCTAssertEqual(model.attentionSessionIDs, ["chat-1"])
        XCTAssertEqual(model.toast?.tone, .success)
        XCTAssertEqual(model.toast?.target, .session("chat-1"))

        model.destination = .chats
        model.navigationPath = [.chat(.session("chat-1"))]
        model.setChatVisible(true)
        XCTAssertFalse(model.unreadSessionIDs.contains("chat-1"))
        XCTAssertTrue(model.attentionSessionIDs.isEmpty)
        model.dismissToast()
        model.applySessions([session(state: .running, turnID: "turn-2")])
        model.applySessions([session(state: .idle, outcome: .completed)])
        XCTAssertNil(model.toast)

        model.setChatVisible(false)
        model.applySessions([session(state: .running, turnID: "turn-3")])
        model.applySessions([session(state: .idle)])
        XCTAssertTrue(model.unreadSessionIDs.contains("chat-1"))
    }

    func testCompletedSessionNotificationUsesGatewayFinalAnswerPreviewAndBotIdentity() throws {
        let model = try model()
        model.applySessions([session(
            state: .running,
            turnID: "turn-1",
            title: "Do not use this title"
        )])

        model.applySessions([session(
            state: .idle,
            outcome: .completed,
            message: "  Fixed the parser.\n\nAll focused tests pass.  ",
            sequence: 2,
            title: "Do not use this title"
        )])

        XCTAssertEqual(model.toast?.tone, .success)
        XCTAssertEqual(model.toast?.target, .session("chat-1"))
        XCTAssertEqual(model.toast?.message, "Helper: Fixed the parser. All focused tests pass.")
    }

    func testReadSessionStaysReadWhenCatalogMetadataChangesAtSameSequence() throws {
        let model = try model()
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.applySessions([session(
            state: .idle,
            outcome: .failed,
            message: "Initial failure",
            sequence: 1
        )])
        model.markSessionRead("chat-1")

        model.applySessions([session(
            state: .idle,
            outcome: .failed,
            message: "Refined failure",
            sequence: 1
        )])

        XCTAssertFalse(model.unreadSessionIDs.contains("chat-1"))
        XCTAssertNil(model.toast)

        model.applySessions([session(
            state: .idle,
            outcome: .failed,
            message: "New failure",
            sequence: 2
        )])

        XCTAssertTrue(model.unreadSessionIDs.contains("chat-1"))
        XCTAssertEqual(model.toast?.message, "Review failed: New failure.")
    }

    func testExplicitUnreadSurvivesCatalogAndStoredStateUntilMarkedRead() throws {
        for sequence in [UInt64(0), 12] {
            let model = try model()
            let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
            model.accounts = [account]
            model.selectedAccountID = account.id
            let chat = session(state: .idle, sequence: sequence)
            model.applySessions([chat])
            model.selectedSessionID = chat.sessionId
            model.setChatVisible(true)

            model.markSessionUnread(chat.sessionId)
            model.applySessions([chat])
            XCTAssertTrue(model.unreadSessionIDs.contains(chat.sessionId))
            XCTAssertNil(try XCTUnwrap(model.sessionReadCursors?[chat.sessionId]).sequence)

            // Reload the durable cursor just as account restoration does on launch.
            model.restoreSessionReadState()
            model.applySessions([chat])
            XCTAssertTrue(model.unreadSessionIDs.contains(chat.sessionId))

            model.markSessionRead(chat.sessionId)
            model.restoreSessionReadState()
            model.applySessions([chat])
            XCTAssertFalse(model.unreadSessionIDs.contains(chat.sessionId))
            XCTAssertEqual(model.sessionReadCursors?[chat.sessionId]?.sequence, sequence)

            model.markSessionUnread(chat.sessionId)
            model.openChat(chat.sessionId)
            XCTAssertFalse(model.unreadSessionIDs.contains(chat.sessionId))
        }
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
        XCTAssertEqual(model.toast?.target, .session("chat-1"))
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
                "type": .string("turn_started"),
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

        model.pairingCode = "code with spaces"
        model.pair()

        XCTAssertEqual(
            model.pairingError,
            GatewayWireError.invalidPairingSetup.localizedDescription
        )

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

}
