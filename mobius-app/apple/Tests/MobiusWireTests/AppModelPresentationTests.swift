import Foundation
import Observation
import SwiftUI
@testable import Mobius
import XCTest

@MainActor
extension AppModelTests {
    func testCreationFormsReopenWithEmptyDrafts() async throws {
        let recorder = GatewayRequestRecorder()
        let app = try model { await recorder.record($0) }
        app.gateway.connectionState = .ready
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 402, height: 874)
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        let host = UIHostingController(
            rootView: AnyView(NavigationStack { BotsView() }.environment(app)))
        window.rootViewController = host
        window.makeKeyAndVisible()

        func activate(_ label: String) async throws {
            let activated = await eventually {
                testAccessibilityElements(host.view).contains {
                    $0.accessibilityLabel == label && $0.accessibilityActivate()
                }
            }
            XCTAssertTrue(activated, "Missing action: \(label)")
            try await Task.sleep(for: .milliseconds(350))
        }
        func fields() -> [UIView] {
            testAccessibilityElements(host.view).compactMap { $0 as? UIView }.filter {
                guard $0 is UITextField || $0 is UITextView else { return false }
                let center = CGPoint(x: $0.bounds.midX, y: $0.bounds.midY)
                return window.hitTest($0.convert(center, to: window), with: nil)?.isDescendant(
                    of: $0) == true
            }
        }
        try await activate("New Bot")
        let name = try XCTUnwrap(fields().compactMap { $0 as? UITextField }.first)
        let description = try XCTUnwrap(fields().compactMap { $0 as? UITextView }.first)
        name.becomeFirstResponder()
        name.insertText("First Bot")
        description.becomeFirstResponder()
        description.insertText("First Bot instructions")
        description.resignFirstResponder()
        try await activate("Create")
        let request = await recorder.firstRequest(after: 0) {
            if case .createBot = $0 { return true }
            return false
        }
        guard case .createBot(let id, let sentName, let sentDescription) = try XCTUnwrap(request)
        else { return XCTFail("Expected Bot creation") }
        XCTAssertEqual(sentName, "First Bot")
        XCTAssertEqual(sentDescription, "First Bot instructions")
        app.gateway.handle(.bots(requestID: id, bots: app.bots + [bot(id: "bot-2")]))
        try await Task.sleep(for: .milliseconds(500))
        try await activate("New Bot")
        XCTAssertEqual(fields().compactMap { $0 as? UITextField }.first?.text, "")
        XCTAssertEqual(fields().compactMap { $0 as? UITextView }.first?.text, "")
        host.rootView = AnyView(NavigationStack { BotsView() }.environment(app).id(UUID()))

        app.chat.sessions = [session(state: .idle)]
        host.rootView = AnyView(NavigationStack { BotDetailView(botID: "bot-1") }.environment(app))
        try await activate("New routine")
        let instructions = try XCTUnwrap(fields().compactMap { $0 as? UITextView }.first)
        instructions.becomeFirstResponder()
        instructions.insertText("Previous routine")
        instructions.resignFirstResponder()
        try await activate("Create")
        let routineRequest = await recorder.firstRequest(after: 0) {
            if case .createRoutine = $0 { return true }
            return false
        }
        XCTAssertNotNil(routineRequest)
        app.routineRequestIDs.removeAll()
        try await Task.sleep(for: .milliseconds(500))
        try await activate("New routine")
        XCTAssertEqual(fields().compactMap { $0 as? UITextView }.first?.text, "")
    }

    func testSettingsInformationLinksUseTheAppLocaleAndPublicCloudPages() {
        for (identifier, language) in [
            ("en_US", "en"), ("fr_CA", "fr"), ("de_CH", "de"), ("ja_JP", "en"), ("", "en"),
        ] {
            let paths = SettingsInformationPage.allCases.map {
                $0.url(locale: Locale(identifier: identifier)).absoluteString
            }
            XCTAssertEqual(
                paths,
                ["acceptable-use", "terms", "privacy", "licenses", "support"].map {
                    "https://mobius.thinkingsand.dev/legal/\(language)/\($0)"
                }
            )
        }
    }

    func testPromptCardsKeepTheirRowHeightWhenEditingOverflowingText() async throws {
        let suite = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let app = Mobius.AppModel(
            store: Mobius.GatewayStore(defaults: defaults), settingsDefaults: defaults,
            appLockAuthenticator: Mobius.AppLockAuthenticator(
                method: { .unavailable }, authenticate: { _ in false }),
            requestSender: { _ in }
        )
        var config = composition()
        config.systemPrompt = String(repeating: "System prompt line.\n", count: 40)
        let fixture = bot(
            description: String(repeating: "Description line.\n", count: 24),
            config: VersionedAgentConfig(revision: 1, config: config)
        )
        app.bots = [
            try JSONDecoder().decode(Mobius.BotRecord.self, from: JSONEncoder().encode(fixture))
        ]
        app.beginEditingBot(app.bots[0])
        app.gateway.connectionState = .ready
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        previous?.isHidden = true
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.isHidden = false
            previous?.makeKeyAndVisible()
        }
        let host = UIHostingController(
            rootView:
                NavigationStack {
                    Mobius.AgentSettingsView(scope: .bot(fixture.id))
                }
                .modifier(Mobius.MobiusTheme())
                .environment(app)
                .environment(\.colorScheme, .light)
        )
        window.rootViewController = host
        window.makeKeyAndVisible()
        func fields(in view: UIView) -> [UITextView] {
            (view as? UITextView).map { [$0] } ?? view.subviews.flatMap { fields(in: $0) }
        }
        func checkRow(_ field: UITextView) throws {
            var ancestor = field.superview
            while let view = ancestor, !(view is UICollectionViewCell) { ancestor = view.superview }
            let row = try XCTUnwrap(ancestor as? UICollectionViewCell)
            // The card's padding may surround the visible lines, never the hidden text.
            XCTAssertLessThanOrEqual(row.bounds.height - field.bounds.height, 40)
            XCTAssertGreaterThanOrEqual(row.bounds.height, field.bounds.height)
        }
        let appeared = await eventually {
            let fields = fields(in: host.view)
            return fields.count == 2 && fields.allSatisfy { $0.bounds.height > 0 }
        }
        XCTAssertTrue(appeared)
        for index in 0..<2 {
            let field = try XCTUnwrap(fields(in: host.view).dropFirst(index).first)
            let maximumHeight = field.bounds.height
            try checkRow(field)
            XCTAssertTrue(field.becomeFirstResponder())
            for text in [
                String(
                    repeating:
                        "A long wrapped prompt with enough text to overflow the visible rows. ",
                    count: 100),
                String(repeating: "Explicit line break.\n", count: 30),
                "Short description 🌿",
                "",
            ] {
                field.selectAll(nil)
                field.insertText(text)
                try await Task.sleep(for: .milliseconds(600))
                try checkRow(field)
                XCTAssertEqual(field.text, text)
                XCTAssertLessThanOrEqual(field.bounds.height, maximumHeight + 1)
                if text.count > 100 {
                    XCTAssertTrue(field.isScrollEnabled)
                    XCTAssertGreaterThan(field.contentSize.height, field.bounds.height)
                    field.setContentOffset(
                        CGPoint(x: 0, y: field.contentSize.height - field.bounds.height),
                        animated: false)
                    try await Task.sleep(for: .milliseconds(100))
                    XCTAssertGreaterThan(field.contentOffset.y, 0)
                    try checkRow(field)
                } else {
                    XCTAssertLessThan(field.bounds.height, maximumHeight)
                }
            }
            field.resignFirstResponder()
            try await Task.sleep(for: .milliseconds(600))
            try checkRow(field)
        }
        XCTAssertEqual(app.botDescriptionDraft, "")
        XCTAssertEqual(app.botDraft?.systemPrompt, "")
        let image = UIGraphicsImageRenderer(bounds: host.view.bounds).image { _ in
            host.view.drawHierarchy(in: host.view.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image)
        attachment.name = "prompt-cards-after-editing-and-clearing"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testLoadingStatusChangesItsMarkWithoutMovingTheLabel() throws {
        var images: [UIImage] = []
        for isLoading in [false, true] {
            let renderer = ImageRenderer(
                content:
                    HStack {
                        Mobius.MobiusStatusIndicator(color: .black, isLoading: isLoading)
                        Text(verbatim: "Gateway")
                    }
                    .foregroundStyle(.black)
                    .padding()
                    .background(.white)
                    .environment(\.scenePhase, .inactive)
            )
            let image = try XCTUnwrap(renderer.uiImage)
            images.append(image)
            let attachment = XCTAttachment(image: image)
            attachment.name = isLoading ? "Gateway connecting spinner" : "Gateway status dot"
            attachment.lifetime = .keepAlways
            add(attachment)
        }
        XCTAssertEqual(images[0].size, images[1].size)
        XCTAssertNotEqual(try XCTUnwrap(images[0].pngData()), try XCTUnwrap(images[1].pngData()))
    }

    func testAppLockKeepsPresentedFormAndItsDraftMounted() async throws {
        let suite = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let app = Mobius.AppModel(
            store: Mobius.GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            appLockAuthenticator: Mobius.AppLockAuthenticator(
                method: { .faceID }, authenticate: { _ in false }
            ),
            requestSender: { _ in }
        )
        app.appLockEnabled = true
        app.isAppLocked = false
        let state = LockSheetTestState()
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 390, height: 740)
        let host = UIHostingController(
            rootView: LockSheetTestPresenter(state: state).environment(app))
        window.rootViewController = host
        previous?.isHidden = true
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.isHidden = false
            previous?.makeKeyAndVisible()
        }
        state.isPresented = true
        let appeared = await eventually(timeout: .seconds(3)) {
            host.presentedViewController?.viewIfLoaded?.window != nil
        }
        XCTAssertTrue(appeared)
        let sheet = try XCTUnwrap(host.presentedViewController)
        func fields(in view: UIView) -> [UITextField] {
            (view as? UITextField).map { [$0] } ?? view.subviews.flatMap { fields(in: $0) }
        }
        let field = try XCTUnwrap(fields(in: sheet.view).first)
        field.text = "Unsaved setup credential"
        field.sendActions(for: .editingChanged)
        XCTAssertTrue(field.becomeFirstResponder())
        for phase in [ScenePhase.inactive, .background, .active] {
            app.isAppLocked = phase == .active
            state.phase = phase
            let concealed = await eventually { window.alpha == 0 && !field.isFirstResponder }
            XCTAssertTrue(concealed)
            XCTAssertFalse(field.isFirstResponder)
            XCTAssertFalse(window.isUserInteractionEnabled)
            XCTAssertTrue(window.accessibilityElementsHidden)
            let cover = try XCTUnwrap(scene.keyWindow)
            XCTAssertFalse(cover === window)
            XCTAssertGreaterThan(cover.windowLevel, .alert)
            XCTAssertEqual(cover.frame, scene.effectiveGeometry.coordinateSpace.bounds)
            XCTAssertTrue(cover.isUserInteractionEnabled)
            XCTAssertTrue(cover.accessibilityViewIsModal)
            XCTAssertTrue(host.presentedViewController === sheet)
            XCTAssertEqual(field.text, "Unsaved setup credential")
        }
        let cover = try XCTUnwrap(scene.keyWindow)
        let lockedImage = UIGraphicsImageRenderer(bounds: cover.bounds).image { _ in
            cover.drawHierarchy(in: cover.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: lockedImage)
        attachment.name = "locked-sheet-preserves-hidden-form"
        attachment.lifetime = .keepAlways
        add(attachment)
        app.isAppLocked = false
        let revealed = await eventually { window.alpha == 1 && window.isKeyWindow }
        XCTAssertTrue(revealed)
        XCTAssertTrue(window.isUserInteractionEnabled)
        XCTAssertFalse(window.accessibilityElementsHidden)
        XCTAssertTrue(host.presentedViewController === sheet)
        XCTAssertEqual(field.text, "Unsaved setup credential")

        let alert = UIAlertController(
            title: "Private setup code", message: "ABCD-1234", preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Continue", style: .default))
        sheet.present(alert, animated: false)
        let alertAppeared = await eventually { alert.viewIfLoaded?.window != nil }
        XCTAssertTrue(alertAppeared)
        app.isAppLocked = true
        let alertConcealed = await eventually { window.alpha == 0 && scene.keyWindow !== window }
        XCTAssertTrue(alertConcealed)
        XCTAssertTrue(sheet.presentedViewController === alert)
        XCTAssertGreaterThan(try XCTUnwrap(scene.keyWindow).windowLevel, .alert)
        app.isAppLocked = false
        let alertRevealed = await eventually { window.alpha == 1 && window.isKeyWindow }
        XCTAssertTrue(alertRevealed)
        XCTAssertTrue(sheet.presentedViewController === alert)
        XCTAssertEqual(field.text, "Unsaved setup credential")
        await withCheckedContinuation { continuation in
            alert.dismiss(animated: false) { continuation.resume() }
        }
        let share = UIActivityViewController(
            activityItems: ["ABCD-1234"], applicationActivities: nil)
        share.popoverPresentationController?.sourceView = sheet.view
        share.popoverPresentationController?.sourceRect = sheet.view.bounds
        await withCheckedContinuation { continuation in
            sheet.present(share, animated: false) { continuation.resume() }
        }
        let shareAppeared = await eventually { share.viewIfLoaded?.window != nil }
        XCTAssertTrue(shareAppeared)
        app.isAppLocked = true
        let shareConcealed = await eventually { window.alpha == 0 && scene.keyWindow !== window }
        XCTAssertTrue(shareConcealed)
        XCTAssertTrue(sheet.presentedViewController === share)
        app.isAppLocked = false
        let shareRevealed = await eventually { window.alpha == 1 && window.isKeyWindow }
        XCTAssertTrue(shareRevealed)
        XCTAssertTrue(sheet.presentedViewController === share)
        XCTAssertEqual(field.text, "Unsaved setup credential")
        share.dismiss(animated: false)
        app.isAppLocked = true
        let coveredBeforeRemoval = await eventually { window.alpha == 0 }
        XCTAssertTrue(coveredBeforeRemoval)
        window.rootViewController = nil
        let restoredOnRemoval = await eventually { window.alpha == 1 && window.isKeyWindow }
        XCTAssertTrue(restoredOnRemoval)
        XCTAssertTrue(window.isUserInteractionEnabled)
        XCTAssertFalse(window.accessibilityElementsHidden)
    }

    func testWorkspaceSessionGroupingKeepsProjectOrderAndRecentChats() {
        let sessions = [
            session(
                sessionID: "older-current",
                state: .idle,
                updatedAt: 100,
                workspaceID: "current",
                workspaceLabel: "/srv/current"
            ),
            session(
                sessionID: "other",
                state: .idle,
                updatedAt: 300,
                workspaceID: "other",
                workspaceLabel: "/srv/other"
            ),
            session(
                sessionID: "newer-current",
                state: .idle,
                updatedAt: 200,
                workspaceID: "current",
                workspaceLabel: "/srv/current"
            ),
        ]

        let groups = WorkspaceSessions.grouped(sessions, prioritizing: "current")

        XCTAssertEqual(groups.map(\.id), ["current", "other"])
        XCTAssertEqual(groups[0].sessions.map(\.sessionId), ["newer-current", "older-current"])
    }

    func testChatBotFilterShowsAllOrAnySelectedBots() throws {
        let model = try model()
        model.bots = [
            bot(id: "bot-a", handle: "alpha", name: "Alpha"),
            bot(id: "bot-b", handle: "beta", name: "Beta"),
            bot(id: "bot-c", handle: "gamma", name: "Gamma"),
        ]
        model.chat.sessions = [
            session(sessionID: "chat-a", state: .idle, botID: "bot-a"),
            session(sessionID: "chat-b", state: .idle, botID: "bot-b"),
            session(sessionID: "chat-c", state: .idle, botID: "bot-c"),
        ]

        XCTAssertEqual(
            model.chat.chatCatalogSessions.map(\.sessionId), ["chat-a", "chat-b", "chat-c"])

        model.chat.chatBotFilterIDs = ["bot-a", "bot-c"]
        XCTAssertEqual(model.chat.chatCatalogSessions.map(\.sessionId), ["chat-a", "chat-c"])
    }

    func testCollapsedDurationUsesOneCompactNaturalUnit() {
        let locale = Locale(identifier: "en_US")
        XCTAssertEqual(formatCompactDuration(59, locale: locale), "59 sec")
        XCTAssertEqual(formatCompactDuration(60, locale: locale), "1 min")
        XCTAssertEqual(formatCompactDuration(3_600, locale: locale), "1 hr")
        XCTAssertEqual(formatCompactDuration(3_600, locale: Locale(identifier: "fr_FR")), "1 h")
        XCTAssertEqual(formatCompactDuration(3_600, locale: Locale(identifier: "de_DE")), "1 Std.")
        XCTAssertEqual(formatDuration(3_600), "60:00")
    }

    func testRoutineDateUsesWeeklyWeekday() {
        let timeZone = TimeZone(secondsFromGMT: 0)!
        let date = routineDate(
            for: Mobius.SimpleRoutineSchedule(minute: 0, hour: 9, weekday: 1),
            timeZone: timeZone,
            from: Date(timeIntervalSince1970: 0)
        )
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = timeZone

        XCTAssertEqual(calendar.component(.weekday, from: date), 2)
        XCTAssertEqual(calendar.component(.hour, from: date), 9)
        XCTAssertEqual(calendar.component(.minute, from: date), 0)
    }

    func testMessageDeliverySymbolsUseTheirRequestedGlyphs() {
        XCTAssertEqual(MobiusSymbol.knownGlyph(for: "steer"), .arrowUpRight01)
        XCTAssertEqual(MobiusSymbol.knownGlyph(for: "queue"), .queue01)
    }

    func testToolLoadUsesTheStandardToolTranscriptPresentation() throws {
        let app = try model()
        let event = AgentEventRecord(
            submissionId: "input-1",
            msg: .object([
                "type": .string("tool_load"),
                "turnId": .string("turn-1"),
                "loadId": .string("step-1"),
                "catalogRevision": .string("catalog-1"),
                "tools": .array([.string("search_history"), .string("read_history")]),
            ]))
        try AgentEventRecord.validate(event.msg)
        app.chat.reduce(
            record: RecordedEvent(
                sequence: 1,
                recordedAtMs: 1_000,
                event: event,
                streamMetrics: [],
                blocks: [
                    RenderedBlock(
                        capability: "tools",
                        block: FrontendBlock(
                            id: "turn-1/step-1/load",
                            group: nil,
                            update: .replace,
                            state: .complete,
                            role: .tool,
                            title: "Loaded tools",
                            text: "search_history\nread_history",
                            symbol: nil,
                            format: "plain_text",
                            tone: "success",
                            files: []
                        ))
                ],
                preview: nil
            ))

        let entry = try XCTUnwrap(app.chat.transcript.first)
        XCTAssertEqual(entry.role, .tool)
        XCTAssertEqual(entry.title, "Loaded tools")
        XCTAssertEqual(entry.text, "search_history\nread_history")
        XCTAssertEqual(entry.turnID, "turn-1")
    }

    func testPeerCoordinationUsesCanonicalEventAcrossLiveReplayPreviewAndCache() throws {
        let text = "  Check the parser boundary.\nKeep this second line.\n"
        let target = MessageTarget(checkpointSequence: 3, batchItemCount: 1)
        let reply = MessageReply(
            target: MessageTarget(checkpointSequence: 1, batchItemCount: 1),
            text: "Original decision"
        )
        let records = [
            recorded(
                1,
                .object([
                    "type": .string("turn_started"),
                    "turnId": .string("peer-turn"),
                ])),
            recordedPeerMessage(2, text: text, reply: reply, messageTarget: target),
            recorded(
                3,
                testAssistantMessage(
                    turnID: "peer-turn", modelStepID: "step-1", phase: "commentary",
                    text: "Checking"
                )),
            recorded(
                4,
                testAssistantMessage(
                    turnID: "peer-turn", modelStepID: "step-2", text: "Done"
                )),
            recorded(
                5, .object(["type": .string("turn_complete"), "turnId": .string("peer-turn")])),
        ]
        let live = try model()
        for record in records { live.chat.reduce(record: record) }
        let replay = try model()
        replay.chat.mergeHistory(records)
        replay.chat.apply(
            RenderedPreview(
                id: "reviewer",
                title: "reviewer",
                subtitle: "",
                pageId: "latest",
                update: .replace,
                events: records.map { record in
                    RenderedEventRecord(
                        event: record.event.msg,
                        blocks: record.blocks,
                        recordedAtMs: record.recordedAtMs
                    )
                },
                next: nil
            ), selection: nil)
        let preview = try XCTUnwrap(replay.chat.previews.first)
        let cached = CachedTranscript(
            sequence: 5,
            nextBeforeSequence: nil,
            transcript: live.chat.transcript,
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let restored = try JSONDecoder().decode(
            CachedTranscript.self,
            from: JSONEncoder().encode(cached)
        ).transcript

        for entries in [live.chat.transcript, replay.chat.transcript, preview.entries, restored] {
            XCTAssertEqual(entries.count, 3)
            XCTAssertEqual(entries.map(\.turnID), Array(repeating: "peer-turn", count: 3))
            XCTAssertEqual(entries.map(\.startsTurn), [true, false, false])
            let entry = try XCTUnwrap(entries.first)
            XCTAssertEqual(entry.kind, .event)
            XCTAssertEqual(entry.role, .activity)
            XCTAssertEqual(entry.capability, "messages")
            XCTAssertEqual(entry.headline, "Message received from @reviewer")
            XCTAssertEqual(entry.eventDetail, text)
            XCTAssertEqual(entry.symbol, "chat")
            XCTAssertEqual(entry.messageMetadata?.author.peerFields?.handle, "reviewer")
            XCTAssertEqual(entry.messageMetadata?.delivery, .turn)
            XCTAssertEqual(entry.messageTarget, target)
            XCTAssertEqual(entry.reply, reply)
            XCTAssertEqual(
                TranscriptProjection(entries: entries).rows.map(\.kind), [.workedGroup, .narrative])
        }
    }

    func testBlockAppendsSeparateChunksWithoutRemovingWhitespace() {
        XCTAssertEqual(appendingBlockText("contents", to: "note.txt"), "note.txt\ncontents")
        XCTAssertEqual(appendingBlockText("\n\ncontents", to: "note.txt"), "note.txt\n\ncontents")
        XCTAssertEqual(appendingBlockText("contents", to: "note.txt\n"), "note.txt\ncontents")
        XCTAssertEqual(appendingBlockText("\ncontents", to: ""), "\ncontents")
        XCTAssertEqual(appendingBlockText("", to: "note.txt"), "note.txt")
    }

    func testFrontendPresentationMetadataUsesTheAppLanguageCatalog() {
        let french = Locale(identifier: "fr")
        let values = [
            "Scratchpad", "Global Scratchpad", "Chat Scratchpad", "Promote", "Edit", "Delete",
            "Plugin title outside the app catalog",
        ].map { value in
            MobiusText.localized(frontendPresentationText(value)).resolved(locale: french)
        }

        XCTAssertEqual(
            values,
            [
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
        let started = RenderedBlock(
            capability: "tools",
            block: FrontendBlock(
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
        let finished = RenderedBlock(
            capability: "tools",
            block: FrontendBlock(
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
        app.chat.reduce(record: firstRecord)
        app.chat.reduce(
            record: RecordedEvent(
                sequence: 2,
                recordedAtMs: 1_001,
                event: finishedEvent,
                streamMetrics: [],
                blocks: [finished],
                preview: nil
            ))

        let entry = try XCTUnwrap(app.chat.transcript.first)
        XCTAssertEqual(app.chat.transcript.count, 1)
        XCTAssertEqual(entry.id, "block:5:toolsresult")
        XCTAssertEqual(entry.group, "turn")
        XCTAssertEqual(entry.text, "Started\n and finished")
        XCTAssertEqual(entry.tone, "success")
        XCTAssertFalse(entry.pending)

        let replay = try model()
        replay.chat.reduce(record: firstRecord)
        XCTAssertEqual(replay.chat.transcript.first?.id, "block:5:toolsresult")
        XCTAssertEqual(replay.chat.transcript.first?.tone, "warning")
    }

    func testRenderedBlocksPreserveCapabilityAndGroup() throws {
        let app = try model()
        for (sequence, capability) in [(UInt64(1), "tools"), (UInt64(2), "review")] {
            app.chat.reduce(
                record: RecordedEvent(
                    sequence: sequence,
                    recordedAtMs: Int64(sequence),
                    event: renderEvent(group: "turn", text: capability),
                    streamMetrics: [],
                    blocks: [
                        RenderedBlock(
                            capability: capability,
                            block: FrontendBlock(
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
                            ))
                    ],
                    preview: nil
                ))
        }

        XCTAssertEqual(app.chat.transcript.map(\.group), ["turn", "turn"])
        XCTAssertEqual(app.chat.transcript.compactMap(\.capability), ["tools", "review"])
    }

    func testFrontendRenderCarriesFilesThroughReplacementAndAppend() throws {
        let model = try model()
        let file = SessionFileReference(
            id: "file-1",
            name: "report.xlsx",
            size: 4,
            mediaType: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        )

        model.chat.reduce(
            event: renderEvent(pending: true, text: "Creating report"),
            blocks: [],
            preview: nil
        )
        model.chat.reduce(
            event: renderEvent(text: "Report ready", files: [file]),
            blocks: [],
            preview: nil
        )

        let completed = try XCTUnwrap(model.chat.transcript.first)
        XCTAssertEqual(completed.text, "Report ready")
        XCTAssertEqual(completed.files, [file])
        XCTAssertFalse(completed.pending)

        model.chat.reduce(
            event: renderEvent(group: nil, append: true, text: "\nOpen it below."),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.chat.transcript.first?.text, "Report ready\nOpen it below.")
        XCTAssertEqual(model.chat.transcript.first?.files, [file])
    }

    func testProjectionRecomputesWhenAFileOnlyActivityBlockGainsText() throws {
        let model = try model()
        let file = SessionFileReference(
            id: "file-1",
            name: "report.txt",
            size: 4,
            mediaType: "text/plain"
        )
        let phrase = TranscriptWaitingPhrase(
            startedAt: Date(timeIntervalSince1970: 1),
            order: [
                "Waiting"
            ])

        model.chat.reduce(
            event: renderEvent(title: "", text: "", files: [file]),
            blocks: [],
            preview: nil
        )
        // The run owns the phrase from the moment it exists. It used to start as a line of
        // its own and move into the row once text landed, which grew the transcript by a row
        // and shrank it again — one arrival, two corrections, a visible bump at the tail.
        XCTAssertEqual(
            model.chat.transcriptProjection(breakBefore: nil, waitingPhrase: phrase).waiting,
            .row("block:5:toolsresult", phrase)
        )

        model.chat.reduce(
            event: renderEvent(title: "", text: "Ready", files: [file]),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(
            model.chat.transcriptProjection(breakBefore: nil, waitingPhrase: phrase).waiting,
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
                    blocks: [
                        RenderedBlock(
                            capability: "reviewer",
                            block: FrontendBlock(
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
                            ))
                    ]
                ),
            ],
            next: nil
        )

        model.chat.reduce(
            event: AgentEventRecord(
                submissionId: nil,
                msg: .object([
                    "type": .string("frontend"),
                    "frontendType": .string("preview"),
                    "title": .string("worker"),
                    "events": .array([]),
                ])),
            blocks: [],
            preview: preview
        )

        let snapshot = try XCTUnwrap(model.chat.previews.first)
        XCTAssertEqual(snapshot.title, "worker")
        XCTAssertEqual(snapshot.context, "full")
        XCTAssertEqual(snapshot.entries.map(\.text), ["Read file", "@@ -1 +1 @@"])
        XCTAssertEqual(snapshot.entries.last?.group, "work")
        XCTAssertEqual(snapshot.entries.last?.format, "unified_diff")
        XCTAssertEqual(snapshot.entries.last?.tone, "success")
        XCTAssertNil(model.chat.presentedPreview)
        XCTAssertFalse(model.showsInspector)
    }

    func testSubagentPreviewProjectsOneCompleteWorkedTurn() throws {
        let model = try model()
        let turnID = "turn-1"
        let compacted = RenderedBlock(
            capability: "agent",
            block: FrontendBlock(
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

        model.chat.reduce(
            event: AgentEventRecord(
                submissionId: nil,
                msg: .object([
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

        let preview = try XCTUnwrap(model.chat.previews.first)
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
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready
        let requestCount = await recorder.requestCount()
        model.submitPickerOption(
            try FrontendPickerOption(
                json: .object([
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
                        "target": .null,
                    ]),
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
        model.chat.reduce(
            event: AgentEventRecord(
                submissionId: submission.id,
                msg: .object([
                    "type": .string("frontend"),
                    "frontendType": .string("preview"),
                    "title": .string("reviewer"),
                    "events": .array([]),
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

        XCTAssertEqual(model.chat.presentedPreview?.title, "reviewer")
        XCTAssertEqual(model.chat.presentedPreview?.status, "running")
        XCTAssertEqual(model.chat.presentedPreview?.model, "gpt-5.6-sol")
        XCTAssertEqual(model.chat.presentedPreview?.context, "none")
        XCTAssertEqual(model.chat.presentedPreview?.entries.map(\.text), ["Done"])
        XCTAssertFalse(model.showsInspector)
    }

    func testPreviewPaginationPrependsOlderBlocksAndClearsLoadingState() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready
        let next = AgentOperation.capabilityCommand(
            capability: "subagents",
            command: "subagents",
            arguments: #"{"path":"/root/reviewer","before_sequence":12}"#,
            input: nil,
            target: nil
        )
        func block(_ text: String) -> RenderedBlock {
            RenderedBlock(
                capability: "agent",
                block: FrontendBlock(
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
        model.chat.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object(["type": .string("frontend")])),
            blocks: [],
            preview: RenderedPreview(
                id: "/root/reviewer",
                title: "reviewer",
                subtitle: "full",
                pageId: "latest",
                update: .replace,
                events: [
                    RenderedEventRecord(
                        event: testAssistantMessage(
                            turnID: "turn-1",
                            modelStepID: "worker-step",
                            text: ""
                        ),
                        blocks: [block("new")]
                    )
                ],
                next: next
            )
        )
        XCTAssertEqual(model.chat.previews.first?.entries.map(\.text), ["new"])

        let requestCount = await recorder.requestCount()
        model.loadPreviewPage(next)
        XCTAssertTrue(model.chat.isLoadingPreviewPage)
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit("chat-1", _) = request else { return false }
            return true
        }
        guard case .submit(_, let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected preview page submission")
        }
        model.chat.reduce(
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
                events: [
                    RenderedEventRecord(
                        event: testMessageEvent(text: ""),
                        blocks: [block("old")]
                    )
                ],
                next: nil
            )
        )

        XCTAssertFalse(model.chat.isLoadingPreviewPage)
        XCTAssertEqual(model.chat.previews.first?.entries.map(\.text), ["old", "new"])
        XCTAssertNil(model.chat.previews.first?.next)
    }

    func testPreviewPaginationComposesCrossPageAppendAndAcceptsAnEmptyTerminalPage() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready
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
                blocks: [
                    RenderedBlock(
                        capability: "tools",
                        block: FrontendBlock(
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
                        ))
                ]
            )
        }
        model.chat.reduce(
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
                    event(text: "er", update: .append),
                ],
                next: next
            )
        )
        model.chat.reduce(
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

        XCTAssertEqual(model.chat.previews.first?.entries.map(\.text), ["old \nnew\ner"])

        let requestCount = await recorder.requestCount()
        model.loadPreviewPage(next)
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit("chat-1", _) = request else { return false }
            return true
        }
        guard case .submit(_, let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected preview page submission")
        }
        model.chat.reduce(
            event: AgentEventRecord(
                submissionId: submission.id,
                msg: .object([
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

        XCTAssertFalse(model.chat.isLoadingPreviewPage)
        XCTAssertEqual(model.chat.previews.first?.entries.map(\.text), ["old \nnew\ner"])
        XCTAssertNil(model.chat.previews.first?.next)
    }

    func testPreviewIdentityDoesNotCollideForMatchingLeafNames() throws {
        let model = try model()
        for path in ["/root/a/reviewer", "/root/b/reviewer"] {
            model.chat.reduce(
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
                    events: [
                        RenderedEventRecord(
                            event: testMessageEvent(text: path),
                            blocks: []
                        )
                    ],
                    next: nil
                )
            )
        }

        XCTAssertEqual(
            Set(model.chat.previews.map(\.id)), ["/root/a/reviewer", "/root/b/reviewer"])
        XCTAssertEqual(model.chat.previews.map(\.title), ["reviewer", "reviewer"])
    }

    func testRejectedPreviewPageClearsLoadingState() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready
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

        model.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: submission.id,
                    code: "invalid_request",
                    message: "Page unavailable",
                    fatal: false
                )))

        XCTAssertFalse(model.chat.isLoadingPreviewPage)
    }

    func testFrontendPickerUsesGenericPromptForAnyCapability() throws {
        let model = try model()

        model.chat.reduce(
            event: AgentEventRecord(
                submissionId: nil,
                msg: .object([
                    "type": .string("frontend"),
                    "frontendType": .string("picker"),
                    "title": .string("Choose a review action"),
                    "options": .array([
                        .object([
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
                                "target": .null,
                            ]),
                        ])
                    ]),
                ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.chat.pendingPicker?.title, "Choose a review action")
        XCTAssertEqual(model.chat.pendingPicker?.options.first?.label, "Accept")
        XCTAssertFalse(model.showsInspector)
    }

    func testFrontendOperationSubmitsEditedCapabilityInput() async throws {
        let recorder = GatewayRequestRecorder()
        let operationSent = expectation(description: "Frontend operation sent")
        let model = try model(requestSender: { request in
            await recorder.record(request)
            if case .submit = request { operationSent.fulfill() }
        })
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready

        model.submitFrontendOperation(
            .capabilityCommand(
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
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready
        let patch = """
            --- note.txt
            +++ note.txt
            @@ -1 +1 @@
            -old
            +new
            """

        let requestCount = await recorder.requestCount()
        model.chat.reduce(
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
        let entry = try XCTUnwrap(model.chat.transcript.last)
        XCTAssertEqual(entry.text, patch)
        XCTAssertEqual(entry.format, "unified_diff")
        XCTAssertEqual(entry.tone, "success")
    }

    func testDuplicateSessionIdentifiersAreRejectedWithoutReplacingTheCatalog() throws {
        let model = try model()
        let original = session(state: .idle)
        model.chat.sessions = [original]

        model.applySessions([original, session(state: .running)])

        XCTAssertEqual(model.chat.sessions, [original])
        XCTAssertEqual(model.toast?.tone, .error)
    }

    func testSessionCatalogRejectsChatsOwnedByUnknownBots() throws {
        let model = try model()
        let helper = bot()
        let original = session(state: .idle, botID: helper.id)
        model.bots = [helper]
        model.chat.sessions = [original]

        model.gateway.handle(
            .sessions(
                requestID: nil,
                sessions: [session(state: .running, botID: "missing-bot")]
            ))

        XCTAssertEqual(model.chat.sessions, [original])
        XCTAssertEqual(model.toast?.message, "The gateway returned a chat with an unknown Bot.")
    }

    func testAssistantAttributionResolvesCurrentBotCatalogIdentity() throws {
        let model = try model()
        model.chat.sessions = [session(state: .idle, botID: "bot-1")]
        model.bots = [
            bot(
                id: "bot-1",
                handle: "reviewer",
                name: "Current Reviewer",
                tint: .purple
            )
        ]

        let bot = try XCTUnwrap(model.bot(forSessionID: "chat-1"))
        XCTAssertEqual(bot.name, "Current Reviewer")
        XCTAssertEqual(bot.tint, .purple)
        XCTAssertNil(model.bot(forSessionID: "missing-chat"))
    }

    func testIdenticalSessionCatalogDoesNotPublishAChange() async throws {
        let model = try model()
        let catalog = [session(state: .idle)]
        model.applySessions(catalog)
        let changed = expectation(description: "sessions changed")
        changed.isInverted = true
        withObservationTracking {
            _ = model.chat.sessions
        } onChange: {
            changed.fulfill()
        }

        model.applySessions(catalog)

        await fulfillment(of: [changed], timeout: 0.05)
    }

}

@MainActor
@Observable
private final class LockSheetTestState {
    var isPresented = false
    var phase = ScenePhase.active
}

private struct LockSheetTestPresenter: View {
    @Environment(Mobius.AppModel.self) private var app
    @Bindable var state: LockSheetTestState

    var body: some View {
        Color.clear
            .sheet(isPresented: $state.isPresented) {
                LockSheetTestForm().presentationDetents([.large])
            }
            .background {
                Mobius.MobiusAppLockPresenter(
                    isCovered: app.isAppLocked || app.appLockEnabled && state.phase != .active
                ) {
                    Mobius.AppLockView().environment(app)
                }
            }
    }
}

private struct LockSheetTestForm: View {
    @State private var credential = ""

    var body: some View {
        NavigationStack {
            Form { TextField("Credential", text: $credential) }
                .navigationTitle("Setup draft")
        }
    }
}
