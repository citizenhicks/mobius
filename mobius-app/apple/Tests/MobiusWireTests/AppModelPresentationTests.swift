import Foundation
import Observation
import XCTest

@MainActor
extension AppModelTests {
    func testMessageDeliverySymbolsUseWorkflowGlyphs() {
        XCTAssertEqual(MobiusSymbol.knownGlyph(for: "steer"), .workflowSquare03)
        XCTAssertEqual(MobiusSymbol.knownGlyph(for: "queue"), .workflowSquare01)
    }

    func testToolLoadUsesTheStandardToolTranscriptPresentation() throws {
        let app = try model()
        let event = AgentEventRecord(submissionId: "input-1", msg: .object([
            "type": .string("tool_load"),
            "turnId": .string("turn-1"),
            "loadId": .string("step-1"),
            "catalogRevision": .string("catalog-1"),
            "tools": .array([.string("swarm_post"), .string("swarm_read")])
        ]))
        try AgentEventRecord.validate(event.msg)
        app.reduce(record: RecordedEvent(
            sequence: 1,
            recordedAtMs: 1_000,
            event: event,
            streamMetrics: [],
            blocks: [RenderedBlock(capability: "tools", block: FrontendBlock(
                id: "turn-1/step-1/load",
                group: nil,
                update: .replace,
                state: .complete,
                role: .tool,
                title: "Loaded tools",
                text: "swarm_post\nswarm_read",
                symbol: nil,
                format: "plain_text",
                tone: "success",
                files: []
            ))],
            preview: nil
        ))

        let entry = try XCTUnwrap(app.transcript.first)
        XCTAssertEqual(entry.role, .tool)
        XCTAssertEqual(entry.title, "Loaded tools")
        XCTAssertEqual(entry.text, "swarm_post\nswarm_read")
        XCTAssertEqual(entry.turnID, "turn-1")
    }

    func testFrontendPresentationMetadataUsesTheAppLanguageCatalog() {
        let french = Locale(identifier: "fr")
        let values = [
            "Scratchpad", "Global Scratchpad", "Chat Scratchpad", "Promote", "Edit", "Delete",
            "Plugin title outside the app catalog",
        ].map { value in
            MobiusText.localized(frontendPresentationText(value)).resolved(locale: french)
        }

        XCTAssertEqual(values, [
            "Bloc-notes", "Bloc-notes global", "Bloc-notes de la conversation",
            "Promouvoir", "Modifier", "Supprimer", "Plugin title outside the app catalog",
        ])
    }

    func testCanonicalFrontendRenderIsCapabilityScopedAndAppliedOnce() throws {
        let app = try model()
        let first = renderEvent(
            pending: true,
            text: "Started",
            tone: "warning"
        )
        let started = RenderedBlock(capability: "tools", block: FrontendBlock(
            id: "result",
            group: "turn",
            update: .replace,
            state: .pending,
            role: .tool,
            title: "Tool",
            text: "Started",
            symbol: nil,
            format: "plain_text",
            tone: "warning",
            files: []
        ))
        let finishedEvent = renderEvent(
            group: nil,
            append: true,
            text: " and finished",
            tone: "success"
        )
        let finished = RenderedBlock(capability: "tools", block: FrontendBlock(
            id: "result",
            group: nil,
            update: .append,
            state: .complete,
            role: .tool,
            title: "Tool",
            text: " and finished",
            symbol: nil,
            format: "plain_text",
            tone: "success",
            files: []
        ))
        let firstRecord = RecordedEvent(
            sequence: 1,
            recordedAtMs: 1_000,
            event: first,
            streamMetrics: [],
            blocks: [started],
            preview: nil
        )
        app.reduce(record: firstRecord)
        app.reduce(record: RecordedEvent(
            sequence: 2,
            recordedAtMs: 1_001,
            event: finishedEvent,
            streamMetrics: [],
            blocks: [finished],
            preview: nil
        ))

        let entry = try XCTUnwrap(app.transcript.first)
        XCTAssertEqual(app.transcript.count, 1)
        XCTAssertEqual(entry.id, "block:5:toolsresult")
        XCTAssertEqual(entry.group, "turn")
        XCTAssertEqual(entry.text, "Started and finished")
        XCTAssertEqual(entry.tone, "success")
        XCTAssertFalse(entry.pending)

        let replay = try model()
        replay.reduce(record: firstRecord)
        XCTAssertEqual(replay.transcript.first?.id, "block:5:toolsresult")
        XCTAssertEqual(replay.transcript.first?.tone, "warning")
    }

    func testRenderedBlocksPreserveCapabilityAndGroup() throws {
        let app = try model()
        for (sequence, capability) in [(UInt64(1), "tools"), (UInt64(2), "review")] {
            app.reduce(record: RecordedEvent(
                sequence: sequence,
                recordedAtMs: Int64(sequence),
                event: renderEvent(group: "turn", text: capability),
                streamMetrics: [],
                blocks: [RenderedBlock(capability: capability, block: FrontendBlock(
                    id: "result",
                    group: "turn",
                    update: .replace,
                    state: .complete,
                    role: .tool,
                    title: capability,
                    text: capability,
                    symbol: nil,
                    format: "plain_text",
                    tone: "neutral",
                    files: []
                ))],
                preview: nil
            ))
        }

        XCTAssertEqual(app.transcript.map(\.group), ["turn", "turn"])
        XCTAssertEqual(app.transcript.compactMap(\.capability), ["tools", "review"])
    }

    func testFrontendRenderCarriesFilesThroughReplacementAndAppend() throws {
        let model = try model()
        let file = SessionFileReference(
            id: "file-1",
            name: "report.xlsx",
            size: 4,
            mediaType: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        )

        model.reduce(
            event: renderEvent(pending: true, text: "Creating report"),
            blocks: [],
            preview: nil
        )
        model.reduce(
            event: renderEvent(text: "Report ready", files: [file]),
            blocks: [],
            preview: nil
        )

        let completed = try XCTUnwrap(model.transcript.first)
        XCTAssertEqual(completed.text, "Report ready")
        XCTAssertEqual(completed.files, [file])
        XCTAssertFalse(completed.pending)

        model.reduce(
            event: renderEvent(group: nil, append: true, text: "\nOpen it below."),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.transcript.first?.text, "Report ready\nOpen it below.")
        XCTAssertEqual(model.transcript.first?.files, [file])
    }

    func testProjectionRecomputesWhenAFileOnlyActivityBlockGainsText() throws {
        let model = try model()
        let file = SessionFileReference(
            id: "file-1",
            name: "report.txt",
            size: 4,
            mediaType: "text/plain"
        )
        let phrase = TranscriptWaitingPhrase(startedAt: Date(timeIntervalSince1970: 1), order: [
            "Waiting"
        ])

        model.reduce(
            event: renderEvent(title: "", text: "", files: [file]),
            blocks: [],
            preview: nil
        )
        // The run owns the phrase from the moment it exists. It used to start as a line of
        // its own and move into the row once text landed, which grew the transcript by a row
        // and shrank it again — one arrival, two corrections, a visible bump at the tail.
        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil, waitingPhrase: phrase).waiting,
            .row("block:5:toolsresult", phrase)
        )

        model.reduce(
            event: renderEvent(title: "", text: "Ready", files: [file]),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil, waitingPhrase: phrase).waiting,
            .row("block:5:toolsresult", phrase)
        )
    }

    func testPreviewPreservesRenderedBlocksAndCapabilityRender() throws {
        let model = try model()
        let outer = FrontendBlock(
            id: "tools/call",
            group: "tools/turn",
            update: .replace,
            state: .complete,
            role: .tool,
            title: "Read file",
            text: "Read file",
            symbol: "task",
            format: "plain_text",
            tone: "neutral",
            files: []
        )
        let rendered = renderEvent(
            capability: "reviewer",
            id: "change",
            group: "work",
            text: "@@ -1 +1 @@",
            format: "unified_diff",
            tone: "success"
        )
        let preview = RenderedPreview(
            id: "/root/worker",
            title: "worker",
            subtitle: "full",
            pageId: "latest",
            update: .replace,
            events: [
            RenderedEventRecord(
                event: .object(["type": .string("tool_call_end")]),
                blocks: [RenderedBlock(capability: "tools", block: outer)]
            ),
            RenderedEventRecord(
                event: rendered.msg,
                blocks: [RenderedBlock(capability: "reviewer", block: FrontendBlock(
                    id: "change",
                    group: "work",
                    update: .replace,
                    state: .complete,
                    role: .artifact,
                    title: "Code change",
                    text: "@@ -1 +1 @@",
                    symbol: nil,
                    format: "unified_diff",
                    tone: "success",
                    files: []
                ))]
            )
            ],
            next: nil
        )

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("preview"),
                "title": .string("worker"),
                "events": .array([])
            ])),
            blocks: [],
            preview: preview
        )

        let snapshot = try XCTUnwrap(model.previews.first)
        XCTAssertEqual(snapshot.title, "worker")
        XCTAssertEqual(snapshot.context, "full")
        XCTAssertEqual(snapshot.entries.map(\.text), ["Read file", "@@ -1 +1 @@"])
        XCTAssertEqual(snapshot.entries.last?.group, "work")
        XCTAssertEqual(snapshot.entries.last?.format, "unified_diff")
        XCTAssertEqual(snapshot.entries.last?.tone, "success")
        XCTAssertNil(model.presentedPreview)
        XCTAssertFalse(model.showsInspector)
    }

    func testSubagentPreviewProjectsOneCompleteWorkedTurn() throws {
        let model = try model()
        let turnID = "turn-1"
        let compacted = RenderedBlock(capability: "agent", block: FrontendBlock(
            id: nil,
            group: turnID,
            update: .replace,
            state: .complete,
            role: .notice,
            title: "Context compacted",
            text: "",
            symbol: nil,
            format: "plain_text",
            tone: "neutral",
            files: []
        ))
        let events = [
            RenderedEventRecord(
                event: .object([
                    "type": .string("turn_started"),
                    "turnId": .string(turnID),
                ]),
                blocks: [],
                recordedAtMs: 1_000
            ),
            RenderedEventRecord(
                event: testMessageEvent(text: "Please review this"),
                blocks: [],
                recordedAtMs: 1_100
            ),
            RenderedEventRecord(
                event: testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-1",
                    phase: "commentary",
                    text: "Checking"
                ),
                blocks: [],
                recordedAtMs: 2_000
            ),
            RenderedEventRecord(
                event: .object(["type": .string("context_compacted")]),
                blocks: [compacted],
                recordedAtMs: 2_200
            ),
            RenderedEventRecord(
                event: testMessageEvent(
                    delivery: .steer,
                    text: "Use the smaller patch"
                ),
                blocks: [],
                recordedAtMs: 2_500
            ),
            RenderedEventRecord(
                event: testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-2",
                    text: "Done"
                ),
                blocks: [],
                recordedAtMs: 4_000
            ),
            RenderedEventRecord(
                event: .object([
                    "type": .string("turn_complete"),
                    "turnId": .string(turnID),
                ]),
                blocks: [],
                recordedAtMs: 4_200
            ),
        ]

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("preview"),
            ])),
            blocks: [],
            preview: RenderedPreview(
                id: "/root/reviewer",
                title: "reviewer",
                subtitle: "Full context",
                pageId: "latest",
                update: .replace,
                events: events,
                next: nil
            )
        )

        let preview = try XCTUnwrap(model.previews.first)
        let rows = TranscriptProjection(entries: preview.entries).rows
        XCTAssertEqual(rows.map(\.kind), [.user, .workedGroup, .narrative])
        XCTAssertEqual(
            rows[1].records.map(\.text),
            ["Checking", "", "Use the smaller patch"]
        )
        XCTAssertEqual(rows[1].elapsedMs, 3_100)
        XCTAssertEqual(rows[2].records.map(\.text), ["Done"])
    }

    func testSelectedPickerPreviewPresentsOneTranscriptWithAgentMetadata() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.selectedSessionID = "chat-1"
        let requestCount = await recorder.requestCount()
        model.submitPickerOption(try FrontendPickerOption(json: .object([
            "label": .string("reviewer"),
            "description": .string("running"),
            "detail": .string("gpt-5.6-sol"),
            "symbol": .string("agent"),
            "showsDetail": .bool(false),
            "op": .object([
                "type": .string("capability_command"),
                "capability": .string("subagents"),
                "command": .string("subagents"),
                "arguments": .string("reviewer"),
                "input": .null,
                "target": .null
            ])
        ])))
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit("chat-1", _) = request else { return false }
            return true
        }
        guard case .submit(_, let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected picker submission")
        }
        let block = FrontendBlock(
            id: "worker/message",
            group: nil,
            update: .replace,
            state: .complete,
            role: .notice,
            title: "Done",
            text: "Done",
            symbol: nil,
            format: "plain_text",
            tone: "success",
            files: []
        )
        model.reduce(
            event: AgentEventRecord(submissionId: submission.id, msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("preview"),
                "title": .string("reviewer"),
                "events": .array([])
            ])),
            blocks: [],
            preview: RenderedPreview(
                id: "/root/reviewer",
                title: "reviewer",
                subtitle: "none",
                pageId: "latest",
                update: .replace,
                events: [
                    RenderedEventRecord(
                        event: testAssistantMessage(
                            turnID: "turn-1",
                            modelStepID: "worker-step",
                            text: ""
                        ),
                        blocks: [RenderedBlock(capability: "worker", block: block)]
                    )
                ],
                next: nil
            )
        )

        XCTAssertEqual(model.presentedPreview?.title, "reviewer")
        XCTAssertEqual(model.presentedPreview?.status, "running")
        XCTAssertEqual(model.presentedPreview?.model, "gpt-5.6-sol")
        XCTAssertEqual(model.presentedPreview?.context, "none")
        XCTAssertEqual(model.presentedPreview?.entries.map(\.text), ["Done"])
        XCTAssertFalse(model.showsInspector)
    }

    func testPreviewPaginationPrependsOlderBlocksAndClearsLoadingState() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.selectedSessionID = "chat-1"
        let next = AgentOperation.capabilityCommand(
            capability: "subagents",
            command: "subagents",
            arguments: #"{"path":"/root/reviewer","before_sequence":12}"#,
            input: nil,
            target: nil
        )
        func block(_ text: String) -> RenderedBlock {
            RenderedBlock(capability: "agent", block: FrontendBlock(
                id: nil,
                group: nil,
                update: .replace,
                state: .complete,
                role: .notice,
                title: "möbius",
                text: text,
                symbol: nil,
                format: "plain_text",
                tone: "neutral",
                files: []
            ))
        }
        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object(["type": .string("frontend")])),
            blocks: [],
            preview: RenderedPreview(
                id: "/root/reviewer",
                title: "reviewer",
                subtitle: "full",
                pageId: "latest",
                update: .replace,
                events: [RenderedEventRecord(
                    event: testAssistantMessage(
                        turnID: "turn-1",
                        modelStepID: "worker-step",
                        text: ""
                    ),
                    blocks: [block("new")]
                )],
                next: next
            )
        )
        XCTAssertEqual(model.previews.first?.entries.map(\.text), ["new"])

        let requestCount = await recorder.requestCount()
        model.loadPreviewPage(next)
        XCTAssertTrue(model.isLoadingPreviewPage)
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit("chat-1", _) = request else { return false }
            return true
        }
        guard case .submit(_, let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected preview page submission")
        }
        model.reduce(
            event: AgentEventRecord(
                submissionId: submission.id,
                msg: .object(["type": .string("frontend")])
            ),
            blocks: [],
            preview: RenderedPreview(
                id: "/root/reviewer",
                title: "reviewer",
                subtitle: "full",
                pageId: "before-12",
                update: .prepend,
                events: [RenderedEventRecord(
                    event: testMessageEvent(text: ""),
                    blocks: [block("old")]
                )],
                next: nil
            )
        )

        XCTAssertFalse(model.isLoadingPreviewPage)
        XCTAssertEqual(model.previews.first?.entries.map(\.text), ["old", "new"])
        XCTAssertNil(model.previews.first?.next)
    }

    func testPreviewPaginationComposesCrossPageAppendAndAcceptsAnEmptyTerminalPage() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.selectedSessionID = "chat-1"
        let next = AgentOperation.capabilityCommand(
            capability: "subagents",
            command: "subagents",
            arguments: #"{"path":"/root/reviewer","before_sequence":12}"#,
            input: nil,
            target: nil
        )
        func event(text: String, update: FrontendBlockUpdate) -> RenderedEventRecord {
            RenderedEventRecord(
                event: .object(["type": .string("tool_call_end")]),
                blocks: [RenderedBlock(capability: "tools", block: FrontendBlock(
                    id: "call-1",
                    group: "turn-1",
                    update: update,
                    state: .complete,
                    role: .tool,
                    title: "Read file",
                    text: text,
                    symbol: "task",
                    format: "plain_text",
                    tone: "neutral",
                    files: []
                ))]
            )
        }
        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object(["type": .string("frontend")])),
            blocks: [],
            preview: RenderedPreview(
                id: "/root/reviewer",
                title: "reviewer",
                subtitle: "Last 1 turn",
                pageId: "latest",
                update: .replace,
                events: [
                    event(text: "new", update: .append),
                    event(text: "er", update: .append)
                ],
                next: next
            )
        )
        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object(["type": .string("frontend")])),
            blocks: [],
            preview: RenderedPreview(
                id: "/root/reviewer",
                title: "reviewer",
                subtitle: "Last 1 turn",
                pageId: "before-12",
                update: .prepend,
                events: [event(text: "old ", update: .replace)],
                next: next
            )
        )

        XCTAssertEqual(model.previews.first?.entries.map(\.text), ["old newer"])

        let requestCount = await recorder.requestCount()
        model.loadPreviewPage(next)
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit("chat-1", _) = request else { return false }
            return true
        }
        guard case .submit(_, let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected preview page submission")
        }
        model.reduce(
            event: AgentEventRecord(submissionId: submission.id, msg: .object([
                "type": .string("frontend")
            ])),
            blocks: [],
            preview: RenderedPreview(
                id: "/root/reviewer",
                title: "reviewer",
                subtitle: "Last 1 turn",
                pageId: "inherited-end",
                update: .prepend,
                events: [],
                next: nil
            )
        )

        XCTAssertFalse(model.isLoadingPreviewPage)
        XCTAssertEqual(model.previews.first?.entries.map(\.text), ["old newer"])
        XCTAssertNil(model.previews.first?.next)
    }

    func testPreviewIdentityDoesNotCollideForMatchingLeafNames() throws {
        let model = try model()
        for path in ["/root/a/reviewer", "/root/b/reviewer"] {
            model.reduce(
                event: AgentEventRecord(
                    submissionId: nil,
                    msg: .object(["type": .string("frontend")])
                ),
                blocks: [],
                preview: RenderedPreview(
                    id: path,
                    title: "reviewer",
                    subtitle: "No context",
                    pageId: "\(path):latest",
                    update: .replace,
                    events: [RenderedEventRecord(
                        event: testMessageEvent(text: path),
                        blocks: []
                    )],
                    next: nil
                )
            )
        }

        XCTAssertEqual(Set(model.previews.map(\.id)), ["/root/a/reviewer", "/root/b/reviewer"])
        XCTAssertEqual(model.previews.map(\.title), ["reviewer", "reviewer"])
    }

    func testRejectedPreviewPageClearsLoadingState() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.selectedSessionID = "chat-1"
        let operation = AgentOperation.capabilityCommand(
            capability: "subagents",
            command: "subagents",
            arguments: #"{"path":"/root/reviewer","before_sequence":12}"#,
            input: nil,
            target: nil
        )
        let requestCount = await recorder.requestCount()
        model.loadPreviewPage(operation)
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit("chat-1", _) = request else { return false }
            return true
        }
        guard case .submit(_, let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected preview page submission")
        }

        model.handle(.rejected(GatewayRejection(
            requestId: submission.id,
            code: "invalid_request",
            message: "Page unavailable",
            fatal: false
        )))

        XCTAssertFalse(model.isLoadingPreviewPage)
    }

    func testFrontendPickerUsesGenericPromptForAnyCapability() throws {
        let model = try model()

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("picker"),
                "title": .string("Choose a review action"),
                "options": .array([.object([
                    "label": .string("Accept"),
                    "description": .string("Accept the review result."),
                    "detail": .string("reviewer-v1"),
                    "symbol": .null,
                    "showsDetail": .bool(true),
                    "op": .object([
                        "type": .string("capability_command"),
                        "capability": .string("reviewer"),
                        "command": .string("accept"),
                        "arguments": .string(""),
                        "input": .null,
                        "target": .null
                    ])
                ])])
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.pendingPicker?.title, "Choose a review action")
        XCTAssertEqual(model.pendingPicker?.options.first?.label, "Accept")
        XCTAssertFalse(model.showsInspector)
    }

    func testFrontendOperationSubmitsEditedCapabilityInput() async throws {
        let recorder = GatewayRequestRecorder()
        let operationSent = expectation(description: "Frontend operation sent")
        let model = try model(requestSender: { request in
            await recorder.record(request)
            if case .submit = request { operationSent.fulfill() }
        })
        model.selectedSessionID = "chat-1"

        model.submitFrontendOperation(.capabilityCommand(
            capability: "notes",
            command: "edit",
            arguments: "note-1",
            input: "Use one row.",
            target: nil
        ))
        await fulfillment(of: [operationSent], timeout: 1)

        let requests = await recorder.requests()
        guard case .submit(let sessionID, let submission) = try XCTUnwrap(requests.first),
              case .capabilityCommand(
                  let capability,
                  let command,
                  let arguments,
                  let input,
                  let target
              ) = submission.op
        else { return XCTFail("Expected edited capability command") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(capability, "notes")
        XCTAssertEqual(command, "edit")
        XCTAssertEqual(arguments, "note-1")
        XCTAssertEqual(input, "Use one row.")
        XCTAssertNil(target)
    }

    func testUnifiedDiffPreservesInlinePatchAndRefreshesGatewayChanges() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.selectedSessionID = "chat-1"
        model.connectionState = .ready
        let patch = """
        --- note.txt
        +++ note.txt
        @@ -1 +1 @@
        -old
        +new
        """

        let requestCount = await recorder.requestCount()
        model.reduce(
            event: renderEvent(
                capability: "reviewer",
                text: patch,
                format: "unified_diff",
                tone: "success"
            ),
            blocks: [],
            preview: nil
        )
        let refresh = await recorder.firstRequest(after: requestCount) { request in
            guard case .getGitDiff(_, "chat-1", .unstaged) = request else { return false }
            return true
        }
        XCTAssertNotNil(refresh)
        let entry = try XCTUnwrap(model.transcript.last)
        XCTAssertEqual(entry.text, patch)
        XCTAssertEqual(entry.format, "unified_diff")
        XCTAssertEqual(entry.tone, "success")
    }

    func testDuplicateSessionIdentifiersAreRejectedWithoutReplacingTheCatalog() throws {
        let model = try model()
        let original = session(state: .idle)
        model.sessions = [original]

        model.applySessions([original, session(state: .running)])

        XCTAssertEqual(model.sessions, [original])
        XCTAssertEqual(model.toast?.tone, .error)
    }

    func testIdenticalSessionCatalogDoesNotPublishAChange() async throws {
        let model = try model()
        let catalog = [session(state: .idle)]
        model.applySessions(catalog)
        let changed = expectation(description: "sessions changed")
        changed.isInverted = true
        withObservationTracking {
            _ = model.sessions
        } onChange: {
            changed.fulfill()
        }

        model.applySessions(catalog)

        await fulfillment(of: [changed], timeout: 0.05)
    }

}
