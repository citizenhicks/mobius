import Foundation
import SwiftUI
import XCTest
@preconcurrency import AVFoundation
@preconcurrency import WebRTC
@testable import Mobius

@MainActor
extension AppModelTests {
    private func voiceModel(recorder: GatewayRequestRecorder = GatewayRequestRecorder()) throws
        -> AppModel
    {
        let model = try model { await recorder.record($0) }
        let config = composition()
        var status = providerStatus(for: config.provider)
        status.realtimeVoices = ["marin", "cedar"]
        model.providerStatuses = [status]
        model.providerInstances = [
            ProviderInstance(
                label: "Work", tint: .blue, configured: true, selection: config.provider,
                modelIds: [], reasoningEfforts: []
            )
        ]
        model.modelChoices = [
            ModelChoice(
                route: "voice-route", group: "Work", model: config.provider.model,
                reasoningEffort: config.provider.reasoningEffort, contextWindow: nil,
                supportsImageInput: true, supportsRealtimeVoice: true, toolDiscovery: .native
            )
        ]
        model.modelProviders = ["voice-route": config.provider.instance]
        model.botDefaultsSnapshot = VersionedAgentConfig(revision: 1, config: config)
        model.chat.selectedModelRoute = "voice-route"
        model.gateway.connectionState = .ready
        return model
    }

    func testCatalogSearchExpandsInHeaderAndCollapsesOnDismissal() async throws {
        let model = try voiceModel()
        model.showsWelcome = false
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready
        model.destination = .chats
        model.showsPairing = false
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        let host = UIHostingController(
            rootView:
                AppShell().mobiusTheme().environment(model)
                .environment(\.horizontalSizeClass, .compact))
        window.rootViewController = host
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        func navigation(_ c: UIViewController) -> UINavigationController? {
            if let c = c as? UINavigationController,
                c.navigationBar.topItem?.searchController != nil
            {
                return c
            }
            return c.children.lazy.compactMap { navigation($0) }.first
        }
        let ready = await eventually(timeout: .seconds(5)) {
            navigation(host)?.navigationBar.topItem?.searchController != nil
        }
        XCTAssertTrue(ready)
        func snapshot(_ name: String) {
            let attachment = XCTAttachment(
                image: UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
                    window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
                })
            attachment.name = name
            attachment.lifetime = .keepAlways
            add(attachment)
        }
        try await Task.sleep(for: .milliseconds(600))
        try activatePresentationToolbarButton("Hide sidebar", in: host)
        try await Task.sleep(for: .milliseconds(600))
        snapshot("Before search")
        for query in ["", "test", ""] {
            let search = try XCTUnwrap(navigation(host)?.navigationBar.topItem?.searchController)
            func controls(in view: UIView) -> [UIControl] {
                (view as? UIControl).map { [$0] } ?? view.subviews.flatMap { controls(in: $0) }
            }
            let bar = try XCTUnwrap(navigation(host)?.navigationBar)
            let open = try XCTUnwrap(
                controls(in: bar).filter {
                    $0.allControlEvents.contains(.primaryActionTriggered)
                }.sorted {
                    $0.convert($0.bounds, to: window).midX < $1.convert($1.bounds, to: window).midX
                }.dropLast().last)
            open.sendActions(for: .primaryActionTriggered)
            let active = await eventually(timeout: .seconds(5)) { search.isActive }
            XCTAssertTrue(active)
            try await Task.sleep(for: .milliseconds(600))
            func searchFields(in view: UIView) -> [UISearchTextField] {
                (view as? UISearchTextField).map { [$0] }
                    ?? view.subviews.flatMap { searchFields(in: $0) }
            }
            let field = try XCTUnwrap(searchFields(in: window).first { $0.bounds.width > 100 })
            let frame = field.convert(field.bounds, to: window)
            XCTAssertGreaterThan(frame.height, 0)
            XCTAssertLessThan(frame.maxY, window.bounds.midY)
            field.text = query
            field.sendActions(for: .editingChanged)
            try await Task.sleep(for: .milliseconds(600))
            snapshot("Search at top without composer")
            let cancel = try XCTUnwrap(
                controls(in: window).first {
                    $0 is UIButton && $0.allControlEvents.contains(.touchUpInside)
                })
            cancel.sendActions(for: .touchUpInside)
            let dismissed = await eventually(timeout: .seconds(5)) { !search.isActive }
            XCTAssertTrue(dismissed)
            try await Task.sleep(for: .seconds(2))
            XCTAssertFalse(search.isActive, "Search reopened after dismissal")
            XCTAssertFalse(
                navigation(host)?.navigationBar.topItem?.searchController?.isActive ?? false)
            snapshot("After dismissing search")
        }
    }

    func testRealtimeEligibilityUsesSelectedRouteAndConfiguredInstance() throws {
        let model = try voiceModel()
        XCTAssertTrue(model.selectedRouteSupportsRealtimeVoice)
        model.chat.selectedSessionID = "chat-1"
        XCTAssertTrue(model.selectedRouteSupportsRealtimeVoice)
        model.providerInstances[0].configured = false
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        model.providerInstances[0].configured = true
        model.providerStatuses[0].realtimeVoices = []
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        model.providerStatuses[0].realtimeVoices = ["marin", "cedar"]
        model.chat.selectedModelRoute = "unknown"
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        XCTAssertTrue(model.newChatRouteSupportsRealtimeVoice)
        model.chat.selectedModelRoute = "voice-route"
        model.modelProviders["voice-route"] = "other-instance"
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
    }

    func testCatalogComposerReopensFocusedDraftAndVoiceSkipsFolderSheet() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                .first { $0.activationState == .foregroundActive })
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        let host = UIHostingController(
            rootView: NavigationStack(
                path: Binding(get: { model.navigationPath }, set: { model.navigationPath = $0 })
            ) {
                ChatsView()
                    .navigationDestination(for: AppRoute.self) { _ in ChatView() }
            }
            .mobiusTheme()
            .environment(model))
        window.rootViewController = host
        window.makeKeyAndVisible()
        defer {
            model.cancelVoiceChatIntent()
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }

        func capture(_ name: String) {
            let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
                window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
            }
            let attachment = XCTAttachment(image: image)
            attachment.name = name
            attachment.lifetime = .keepAlways
            add(attachment)
        }

        func navigationController(in controller: UIViewController) -> UINavigationController? {
            if let navigation = controller as? UINavigationController { return navigation }
            return controller.children.lazy.compactMap { navigationController(in: $0) }.first
        }

        for attempt in 1...2 {
            model.navigationPath = []
            let appeared = await eventually(timeout: .seconds(3)) {
                navigationController(in: host)?.viewControllers.count == 1
                    && navigationController(in: host)?.transitionCoordinator == nil
                    && testAccessibilityElements(window).contains {
                        $0.accessibilityLabel == "New chat" && $0.accessibilityFrame.width > 100
                    }
            }
            XCTAssertTrue(appeared)
            try await Task.sleep(for: .milliseconds(400))
            capture("Catalog closed composer \(attempt)")
            let launch = try XCTUnwrap(
                testAccessibilityElements(window).first {
                    $0.accessibilityLabel == "New chat" && $0.accessibilityFrame.width > 100
                })
            XCTAssertTrue(launch.accessibilityActivate())
            let focused = await eventually(timeout: .seconds(3)) {
                model.navigationPath == [.chat(.new)] && !model.chat.composerIsCompact
                    && testAccessibilityElements(window).contains {
                        $0.accessibilityLabel == "Message"
                    }
            }
            XCTAssertTrue(focused, "Entry \(attempt) did not focus the composer")
            XCTAssertTrue(model.currentSessionTitle.isEmpty)
            XCTAssertNil(model.chat.sessionRequestID)
            try await Task.sleep(for: .milliseconds(400))
            XCTAssertFalse(
                model.chat.composerIsCompact, "Entry \(attempt) lost focus after navigation")
            XCTAssertTrue(
                testAccessibilityElements(window).contains {
                    ($0 as? UIView)?.isFirstResponder == true
                })
            capture("Focused untitled draft \(attempt)")
        }

        model.navigationPath = []
        let returned = await eventually(timeout: .seconds(3)) {
            testAccessibilityElements(window).contains { $0.accessibilityLabel == "New chat" }
        }
        XCTAssertTrue(returned)
        try await Task.sleep(for: .milliseconds(400))
        let voice = try XCTUnwrap(
            testAccessibilityElements(window).first {
                $0.accessibilityLabel == "Start voice chat"
            })
        XCTAssertTrue(voice.accessibilityActivate())
        let requested = await eventually { model.chat.sessionRequestID != nil }
        XCTAssertTrue(requested)
        XCTAssertFalse(model.showsWorkspaceBrowser)
        XCTAssertTrue(model.currentSessionTitle.isEmpty)
        XCTAssertEqual(model.navigationPath, [.chat(.new)])
        try await Task.sleep(for: .milliseconds(400))
        capture("Voice uses current workspace")
        _ = await recorder.firstRequest(after: 0) {
            if case .createSession = $0 { true } else { false }
        }
        let requests = await recorder.requests()
        XCTAssertEqual(
            requests.filter { if case .createSession = $0 { true } else { false } }.count, 1)
    }

    func testReasoningGaugeIncludesAllSixPhases() throws {
        let glyphs = (0...5).map { MobiusGlyph.reasoning(Double($0) / 5) }
        XCTAssertEqual(Set(glyphs).count, 6)
        for glyph in glyphs {
            XCTAssertNotNil(UIImage(named: glyph.asset))
        }
        let renderer = ImageRenderer(
            content: HStack(spacing: 24) {
                ForEach(glyphs, id: \.self) { glyph in
                    MobiusIcon(glyph, size: 44)
                }
            }.foregroundStyle(.white).padding(24).background(.black))
        renderer.scale = 3
        let attachment = XCTAttachment(image: try XCTUnwrap(renderer.uiImage))
        attachment.name = "Six reasoning speedometer phases"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testExpandedComposerFocusesNativeTextInputAndPreservesDraft() async throws {
        let model = try voiceModel()
        model.chooseWorkspace("/srv/project")
        model.chat.composer = "First line\nSecond line\nThird line"
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                .first { $0.activationState == .foregroundActive })
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        window.rootViewController = UIHostingController(
            rootView: ComposerView().mobiusTheme().environment(model))
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        for _ in 0..<2 {
            let appeared = await eventually {
                testAccessibilityElements(window).contains {
                    $0.accessibilityLabel == "Expand composer"
                }
            }
            XCTAssertTrue(appeared)
            let expand = try XCTUnwrap(
                testAccessibilityElements(window).first {
                    $0.accessibilityLabel == "Expand composer"
                })
            XCTAssertTrue(expand.accessibilityActivate())
            let focused = await eventually(timeout: .seconds(3)) {
                testAccessibilityElements(window).contains {
                    ($0 as? UIView)?.isFirstResponder == true
                }
            }
            let attachment = XCTAttachment(
                image: UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
                    window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
                })
            attachment.name = "Expanded editor keyboard check"
            attachment.lifetime = .keepAlways
            add(attachment)
            XCTAssertTrue(focused, "Expanded editor must open with the keyboard")
            let responder = try XCTUnwrap(
                testAccessibilityElements(window).first {
                    ($0 as? UIView)?.isFirstResponder == true
                })
            let editor = try XCTUnwrap(responder as? any UITextInput)
            XCTAssertEqual(editor.autocorrectionType, .yes)
            XCTAssertEqual(editor.autocapitalizationType, .sentences)
            XCTAssertEqual(editor.keyboardType, .default)
            XCTAssertEqual(
                editor.text(
                    in: try XCTUnwrap(
                        editor.textRange(
                            from: editor.beginningOfDocument, to: editor.endOfDocument))),
                model.chat.composer)
            editor.selectedTextRange = editor.textRange(
                from: editor.endOfDocument, to: editor.endOfDocument)
            editor.insertText(" more")
            let updated = await eventually { model.chat.composer.hasSuffix(" more") }
            XCTAssertTrue(updated, model.chat.composer)
            let collapse = try XCTUnwrap(
                testAccessibilityElements(window).first {
                    $0.accessibilityLabel == "Collapse composer"
                })
            XCTAssertTrue(collapse.accessibilityActivate())
            let inlineFocused = await eventually {
                testAccessibilityElements(window).contains {
                    $0.accessibilityLabel == "Expand composer"
                }
                    && testAccessibilityElements(window).contains {
                        ($0 as? UIView)?.isFirstResponder == true
                    }
            }
            XCTAssertTrue(
                inlineFocused, "Collapsing must return keyboard focus to the inline composer")
            try await Task.sleep(for: .milliseconds(500))
            XCTAssertTrue(model.chat.composer.hasSuffix(" more"))
        }

        let expand = try XCTUnwrap(
            testAccessibilityElements(window).first {
                $0.accessibilityLabel == "Expand composer"
            })
        XCTAssertTrue(expand.accessibilityActivate())
        let expanded = await eventually {
            testAccessibilityElements(window).contains {
                $0.accessibilityLabel == "Collapse composer"
            }
        }
        XCTAssertTrue(expanded)
        let send = try XCTUnwrap(
            testAccessibilityElements(window).first { $0.accessibilityLabel == "Send" })
        XCTAssertTrue(send.accessibilityActivate())
        let sent = await eventually(timeout: .seconds(3)) {
            model.chat.sessionRequestID != nil
                && !testAccessibilityElements(window).contains {
                    $0.accessibilityLabel == "Collapse composer"
                }
        }
        XCTAssertTrue(sent)
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertFalse(
            testAccessibilityElements(window).contains {
                ($0 as? UIView)?.isFirstResponder == true
            }, "Sending from the expanded editor must leave the keyboard dismissed")
    }

    func testCatalogComposerMatchesSidebarButtons() async throws {
        let model = try voiceModel()
        model.showsWelcome = false
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready
        model.destination = .chats
        model.showsPairing = false
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                .first { $0.activationState == .foregroundActive })
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        window.rootViewController = UIHostingController(
            rootView: AppShell().mobiusTheme().environment(model)
                .environment(\.horizontalSizeClass, .compact))
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        let appeared = await eventually(timeout: .seconds(3)) {
            testAccessibilityElements(window, maxDepth: 30).contains {
                $0.accessibilityLabel == "New chat" && $0.accessibilityFrame.width > 100
            }
        }
        XCTAssertTrue(appeared)
        try await Task.sleep(for: .milliseconds(500))
        let attachment = XCTAttachment(
            image: UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
                window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
            })
        attachment.name = "Catalog composer aligned with Settings"
        attachment.lifetime = .keepAlways
        add(attachment)
        let elements = testAccessibilityElements(window, maxDepth: 30)
        let composer = try XCTUnwrap(
            elements.first {
                $0.accessibilityLabel == "New chat" && $0.accessibilityFrame.width > 100
            }
        ).accessibilityFrame
        let settings = try XCTUnwrap(
            elements.first {
                $0.accessibilityLabel == "Settings" && $0.accessibilityTraits.contains(.button)
            }
        ).accessibilityFrame
        XCTAssertEqual(composer.height, MobiusStyle.toolbarButtonSize, accuracy: 1)
        XCTAssertEqual(composer.height, settings.height, accuracy: 1)
        XCTAssertEqual(composer.midY, settings.midY, accuracy: 1)
        for title in ["Hide sidebar", "Show sidebar", "Hide sidebar", "Show sidebar"] {
            try activatePresentationToolbarButton(
                title, in: try XCTUnwrap(window.rootViewController))
            try await Task.sleep(for: .milliseconds(500))
            let current = try XCTUnwrap(
                testAccessibilityElements(window, maxDepth: 30).first {
                    $0.accessibilityLabel == "New chat" && $0.accessibilityFrame.width > 100
                }
            ).accessibilityFrame
            XCTAssertEqual(current.height, composer.height, accuracy: 1)
            XCTAssertEqual(
                current.midY, composer.midY, accuracy: 1,
                "Opening the sidebar must not move the composer vertically")
        }
    }

    func testComposerSwitchesDictationToSendAndShowsVoiceForRealtimeModels() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        model.chat.selectedSessionID = "chat-1"
        model.modelChoices.append(
            ModelChoice(
                route: "text-route", group: "Work", model: "text-model",
                reasoningEffort: nil, contextWindow: nil,
                supportsImageInput: true, toolDiscovery: .native
            ))
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes
                .compactMap { $0 as? UIWindowScene }
                .first { $0.activationState == .foregroundActive }
        )
        let previous = scene.keyWindow
        previous?.isHidden = true
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        let host = UIHostingController(
            rootView: ComposerView(showBotSettings: {})
                .frame(width: 402)
                .background { MobiusBackdrop() }
                .modifier(MobiusTheme())
                .environment(model))
        window.rootViewController = host
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.isHidden = false
            previous?.makeKeyAndVisible()
        }

        func checkActions(voice: Bool, send: Bool, dictate: Bool, name: String) async throws {
            let updated = await eventually {
                window.layoutIfNeeded()
                host.view.layoutIfNeeded()
                let labels = testAccessibilityElements(window).compactMap(\.accessibilityLabel)
                return labels.contains("Send") == send
                    && labels.contains("Start voice chat") == voice
                    && labels.contains("Dictate") == dictate
            }
            XCTAssertTrue(
                updated,
                "\(name): \(testAccessibilityElements(window).compactMap(\.accessibilityLabel))")
            if dictate {
                XCTAssertFalse(
                    testAccessibilityElements(window).contains {
                        $0.accessibilityLabel == "Expand composer"
                    })
            }
            for element in testAccessibilityElements(window)
            where element.accessibilityLabel == "Send"
                || element.accessibilityLabel == "Start voice chat"
                || element.accessibilityLabel == "Dictate"
            {
                XCTAssertGreaterThanOrEqual(element.accessibilityFrame.width, MobiusStyle.rowTouch)
                XCTAssertGreaterThanOrEqual(element.accessibilityFrame.height, MobiusStyle.rowTouch)
            }
            try await Task.sleep(for: .milliseconds(350))
            let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
                window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
            }
            let attachment = XCTAttachment(image: image)
            attachment.name = name
            attachment.lifetime = .keepAlways
            add(attachment)
        }
        for route in ["voice-route", "text-route"] {
            model.chat.selectedModelRoute = route
            try await checkActions(
                voice: route == "voice-route", send: false, dictate: true, name: route)
        }
        model.chooseWorkspace("/srv/project")
        XCTAssertTrue(model.canStartRealtimeVoice)
        for draft in ["", "Hello", ""] {
            model.chat.composer = draft
            try await checkActions(
                voice: true, send: !draft.isEmpty, dictate: draft.isEmpty,
                name: "New chat: \(draft)")
        }
        model.chat.composerReply = MessageReply(
            target: MessageTarget(checkpointSequence: 1, batchItemCount: 1), text: "Reply"
        )
        try await checkActions(voice: true, send: true, dictate: false, name: "Reply context")
        model.chat.composerReply = nil
        try await checkActions(voice: true, send: false, dictate: true, name: "New voice chat")
        let voice = try XCTUnwrap(
            testAccessibilityElements(window).first {
                $0.accessibilityLabel == "Start voice chat"
            })
        XCTAssertTrue(voice.accessibilityActivate())
        let request = await recorder.firstRequest(after: 0) {
            if case .createSession = $0 { true } else { false }
        }
        guard case .createSession(let requestID, "/srv/project", let botIDs) = request,
            botIDs == "bot-1"
        else {
            return XCTFail("Primary voice action should create a voice chat")
        }
        XCTAssertEqual(model.newVoiceChatIntent, .openingSession(requestID))
        XCTAssertNil(model.chat.realtimeVoiceCall)
    }

    func testComposerVoicePickerValidatesAndSavesThroughBotConfiguration() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        model.bots = [bot()]
        model.chooseWorkspace("/srv/project")

        model.setSelectedBotVoice("unknown")
        XCTAssertNil(model.botMutationRequestID)
        model.setSelectedBotVoice("cedar")
        let request = await recorder.firstRequest(after: 0) {
            if case .updateBot = $0 { return true }
            return false
        }
        XCTAssertNotNil(request)
        XCTAssertEqual(model.botDraft?.realtimeVoice, "cedar")
        XCTAssertEqual(model.botDraft?.provider, model.selectedBot?.config.config.provider)
    }

    func testBotVoiceSelectionUsesEligibleCatalogAndSurvivesConfigurationCoding() throws {
        let model = try voiceModel()
        var config = composition()
        config.realtimeVoice = "cedar"
        XCTAssertEqual(model.realtimeVoices(for: config), ["marin", "cedar"])
        XCTAssertEqual(
            model.draft(config, selectingModelRoute: "voice-route")?.realtimeVoice, "cedar")

        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let data = try encoder.encode(config)
        XCTAssertEqual(try decoder.decode(AgentComposition.self, from: data), config)

        model.providerStatuses[0].realtimeVoices = ["marin"]
        XCTAssertNil(model.draft(config, selectingModelRoute: "voice-route")?.realtimeVoice)
        model.modelChoices = [
            ModelChoice(
                route: "voice-route", group: "Work", model: config.provider.model,
                reasoningEffort: config.provider.reasoningEffort, contextWindow: nil,
                supportsImageInput: true, toolDiscovery: .native
            )
        ]
        XCTAssertTrue(model.realtimeVoices(for: config).isEmpty)
    }

    func testVoicePillOpensLiveIsolatedTranscriptWithoutEndingCall() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready
        let call = RealtimeVoiceCall(requestID: "call", sessionID: "chat-1")
        model.chat.realtimeVoiceCall = call
        let widget = MountedWidget(
            capability: "messages",
            widget: FrontendWidget(
                id: "voice", slot: .composerFooter, text: "Voice", tone: "neutral", symbol: "voice",
                iconOnly: true, progress: nil, content: nil,
                action: .capabilityCommand(
                    capability: "messages", command: "voice", arguments: "", input: nil, target: nil
                )
            ))
        model.submitWidget(widget)
        let request = await recorder.firstRequest(after: 0) {
            if case .submit = $0 { true } else { false }
        }
        guard case .submit("chat-1", let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected the existing capability command flow")
        }
        func preview(_ events: [RenderedEventRecord]) -> RenderedPreview {
            RenderedPreview(
                id: "voice-chat", title: "voice agent", subtitle: "", pageId: "latest",
                update: .replace, events: events, next: nil
            )
        }
        let draft = RenderedEventRecord(
            event: .object(["type": .string("message_delta"), "text": .string("Hello")]),
            blocks: [], submissionId: "spoken-1"
        )
        model.chat.reduce(
            event: AgentEventRecord(
                submissionId: submission.id, msg: .object(["type": .string("frontend")])),
            blocks: [], preview: preview([draft])
        )
        let draftID = try XCTUnwrap(model.chat.presentedPreview?.entries.first?.id)
        XCTAssertEqual(model.chat.presentedPreview?.entries.first?.pending, true)
        XCTAssertEqual(model.chat.presentedPreview?.entries.first?.text, "Hello")
        XCTAssertNil(model.chat.previewWidgetRequestID)
        model.chat.apply(
            RenderedPreview(
                id: "voice-chat", title: "voice agent", subtitle: "", pageId: "earlier",
                update: .prepend,
                events: [
                    RenderedEventRecord(
                        event: testMessageEvent(text: "Earlier discussion"), blocks: [],
                        submissionId: "spoken-0"
                    )
                ], next: nil
            ), selection: nil)

        let finals = [
            RenderedEventRecord(
                event: testMessageEvent(text: "Hello!"), blocks: [], submissionId: "spoken-1"),
            RenderedEventRecord(
                event: testAssistantMessage(
                    turnID: "spoken-reply", modelStepID: "spoken-reply", text: "Hi there!"
                ), blocks: []),
        ]
        model.chat.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object(["type": .string("frontend")])),
            blocks: [], preview: preview(finals)
        )
        XCTAssertEqual(
            model.chat.presentedPreview?.entries.map(\.text),
            ["Earlier discussion", "Hello!", "Hi there!"])
        XCTAssertEqual(model.chat.presentedPreview?.entries[1].id, draftID)
        XCTAssertNil(model.chat.presentedPreview?.next)
        XCTAssertTrue(try XCTUnwrap(model.chat.presentedPreview).entries.allSatisfy { !$0.pending })
        XCTAssertTrue(model.chat.transcript.isEmpty)
        XCTAssertEqual(model.sessionRunCount, 0)
        XCTAssertEqual(model.chat.realtimeVoiceCall, call)
        model.chat.stopRealtimeVoice(notifyGateway: false)
        XCTAssertEqual(
            model.chat.presentedPreview?.entries.map(\.text),
            ["Earlier discussion", "Hello!", "Hi there!"])
    }

    func testNewVoiceChatUsesLatestWorkspaceAndBotWithoutSelection() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        let first = bot()
        let second = bot(id: "bot-2", handle: "reviewer", name: "Reviewer")
        model.bots = [first, second]
        model.chat.sessions = [
            session(state: .idle, updatedAt: 200, workspaceLabel: "/srv/project", botID: second.id),
            session(sessionID: "older", state: .idle, updatedAt: 100, workspaceLabel: "/aaa"),
        ]
        model.openNewVoiceChat()
        XCTAssertFalse(model.showsWorkspaceBrowser)
        XCTAssertEqual(model.chat.pendingNewChatWorkspace, "/srv/project")
        XCTAssertEqual(model.chat.pendingNewChatBotID, second.id)
        XCTAssertNil(model.chat.realtimeVoiceCall)
        let request = await recorder.firstRequest(after: 0) {
            if case .createSession = $0 { true } else { false }
        }
        guard
            case .createSession(let requestID, "/srv/project", let botIDs) = try XCTUnwrap(request),
            botIDs == "bot-2"
        else {
            return XCTFail("Expected the selected workspace and Bot")
        }
        XCTAssertEqual(model.newVoiceChatIntent, .openingSession(requestID))
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.completePendingVoiceChat(requestID: "unrelated-replay")
        XCTAssertEqual(model.newVoiceChatIntent, .openingSession(requestID))
        model.cancelVoiceChatIntent()
        model.completePendingVoiceChat(requestID: requestID)
        XCTAssertNil(model.chat.realtimeVoiceCall)
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains { if case .startRealtimeVoice = $0 { true } else { false } })
    }

    func testOpeningAnExistingChatCancelsPendingVoiceSetup() throws {
        let model = try voiceModel()
        model.chat.sessions = [session(sessionID: "chat-1", state: .idle)]
        model.newVoiceChatIntent = .selectingBot

        model.openChat("chat-1")

        XCTAssertNil(model.newVoiceChatIntent)
    }

    func testConfirmedVoiceOnlyChatGetsADurableTitleWithoutACatalogRace() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.chat.selectedSessionID = "chat-1"
        model.chat.prepareChatTitle(for: "chat-1")
        model.chat.sessionMutationRequestID = "busy"
        model.chat.realtimeVoiceCall = RealtimeVoiceCall(requestID: "voice-1", sessionID: "chat-1")
        model.gateway.handle(
            .realtimeVoiceStarted(
                requestID: "voice-1", sessionID: "chat-1", voiceID: "call-1", answerSDP: "answer"))
        model.chat.realtimeVoiceTask?.cancel()
        XCTAssertEqual(model.currentSessionTitle, "New voice chat")
        XCTAssertNotNil(model.chat.pendingChatTitles["chat-1"])

        model.chat.sessionMutationRequestID = nil
        model.applySessions([session(state: .idle, firstUserMessage: nil)])
        let rename = await recorder.firstRequest(after: 0) {
            if case .renameSession(_, "chat-1", "New voice chat") = $0 { true } else { false }
        }
        XCTAssertNotNil(rename)
        model.applySessions([session(state: .idle, firstUserMessage: nil, title: "New voice chat")])
        model.chat.stopRealtimeVoice(notifyGateway: false)
        XCTAssertNil(model.chat.pendingChatTitles["chat-1"])
        XCTAssertEqual(model.currentSessionTitle, "New voice chat")
    }

    func testVoiceTitlePreservesExistingTextAndManualTitles() throws {
        let model = try voiceModel()
        model.gateway.selectedAccountID = UUID()
        for (prompt, title) in [("Existing message", nil), (nil, "My chosen title")] {
            model.chat.sessions = [session(state: .idle, firstUserMessage: prompt, title: title)]
            model.chat.startVoiceChatTitle(sessionID: "chat-1", requestID: "voice-1")
            XCTAssertNil(model.chat.pendingChatTitles["chat-1"])
            XCTAssertNil(model.chat.sessionMutationRequestID)
        }
        let complete =
            "Understanding unexpectedly interrupted background audio during on-device dictation"
        model.chat.sessions = [session(state: .idle, title: complete)]
        XCTAssertEqual(model.sessionTitle("chat-1"), complete, "Native text layout owns truncation")
    }

    func testCanceledVoiceStartAndLateAnswerEndOnlyThatCall() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        model.chat.selectedSessionID = "chat-1"
        model.chat.realtimeVoiceCall = RealtimeVoiceCall(
            requestID: "old", sessionID: "chat-1"
        )
        model.chat.stopRealtimeVoice()
        XCTAssertNil(model.chat.realtimeVoiceCall)
        let ended = await recorder.firstRequest(after: 0) {
            if case .endRealtimeVoice("chat-1", "old") = $0 { true } else { false }
        }
        XCTAssertNotNil(ended)
        model.chat.realtimeVoiceCall = RealtimeVoiceCall(
            requestID: "new", sessionID: "chat-1"
        )
        model.gateway.handle(
            .realtimeVoiceStarted(
                requestID: "old", sessionID: "chat-1", voiceID: "old", answerSDP: "late answer"
            ))
        model.gateway.handle(
            .realtimeVoiceFailed(requestID: "old", sessionID: "chat-1", message: "late error"))
        model.gateway.handle(.realtimeVoiceEnded(sessionID: "chat-1", voiceID: "old", reason: nil))
        XCTAssertEqual(model.chat.realtimeVoiceCall?.requestID, "new")
        model.gateway.handle(
            .realtimeVoiceFailed(requestID: "new", sessionID: "chat-1", message: "start failed"))
        XCTAssertNil(model.chat.realtimeVoiceCall)
    }

    func testBackgroundAndForegroundKeepVoiceAndGatewayConnected() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.startedAccountID = account.id
        model.chat.selectedSessionID = "chat-1"
        let call = RealtimeVoiceCall(requestID: "voice", sessionID: "chat-1")
        let startup = Task<Void, Never> { try? await Task.sleep(for: .seconds(3_600)) }
        model.chat.realtimeVoiceCall = call
        model.chat.realtimeVoiceTask = startup
        let oldGeneration = model.gateway.connectionGeneration
        model.newVoiceChatIntent = .openingSession("pending-session")
        model.appDidEnterBackground()
        XCTAssertEqual(model.chat.realtimeVoiceCall, call)
        XCTAssertNotNil(model.chat.realtimeVoiceTask)
        XCTAssertNil(model.newVoiceChatIntent)
        XCTAssertFalse(startup.isCancelled)
        XCTAssertFalse(model.chat.realtimeVoice.isConnected)
        XCTAssertEqual(model.gateway.connectionGeneration, oldGeneration)
        await model.appDidBecomeActive()
        XCTAssertEqual(model.chat.realtimeVoiceCall, call)
        XCTAssertFalse(startup.isCancelled)
        XCTAssertEqual(model.gateway.connectionGeneration, oldGeneration)
        let ended = await recorder.firstRequest(after: 0) {
            if case .endRealtimeVoice("chat-1", call.requestID) = $0 { true } else { false }
        }
        XCTAssertNil(ended)
        model.chat.stopRealtimeVoice()
    }

    func testForegroundRestartsGatewayWithoutVoiceCall() async throws {
        let model = try voiceModel()
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.startedAccountID = account.id
        let oldGeneration = model.gateway.connectionGeneration

        await model.appDidBecomeActive()

        XCTAssertNotEqual(model.gateway.connectionGeneration, oldGeneration)
    }

    func testSceneTeardownEndsVoiceBeforeDisconnect() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        let call = RealtimeVoiceCall(requestID: "voice", sessionID: "chat-1")
        let startup = Task<Void, Never> { try? await Task.sleep(for: .seconds(3_600)) }
        model.chat.realtimeVoiceCall = call
        model.chat.realtimeVoiceTask = startup
        let oldGeneration = model.gateway.connectionGeneration

        model.appDidEnterBackground(preservingVoiceCall: false)

        XCTAssertNil(model.chat.realtimeVoiceCall)
        XCTAssertNil(model.chat.realtimeVoiceTask)
        XCTAssertTrue(startup.isCancelled)
        XCTAssertNotEqual(model.gateway.connectionGeneration, oldGeneration)
        let ended = await recorder.firstRequest(after: 0) {
            if case .endRealtimeVoice("chat-1", "voice") = $0 { true } else { false }
        }
        XCTAssertNotNil(ended)
    }

    func testVoiceClosesOnSessionAndRouteChangesButNotBackground() throws {
        let model = try voiceModel()
        model.chat.selectedSessionID = "chat-1"
        let call = RealtimeVoiceCall(requestID: "voice", sessionID: "chat-1")
        model.chat.realtimeVoiceCall = call
        model.chat.selectedSessionID = "chat-2"
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.chat.realtimeVoiceCall = call
        model.chat.selectedModelRoute = "other-route"
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.chat.realtimeVoiceCall = call
        model.newVoiceChatIntent = .selectingBot
        model.appDidEnterBackground()
        XCTAssertEqual(model.chat.realtimeVoiceCall, call)
        XCTAssertNil(model.newVoiceChatIntent)
        XCTAssertFalse(model.chat.realtimeVoice.isConnected)
        model.chat.stopRealtimeVoice()
    }
}

@MainActor
extension AppModelTests {
    func testNewVoiceChatOpensMicrophoneOnlyAfterCreatedSessionReplay() throws {
        let model = try voiceModel()
        model.bots = [bot(id: "bot-2"), bot()]
        model.chat.sessions = [session(sessionID: "previous-chat", state: .idle)]
        model.chat.selectedSessionID = "previous-chat"
        model.openNewVoiceChat()
        XCTAssertFalse(model.showsWorkspaceBrowser)
        guard case .openingSession(let requestID) = model.newVoiceChatIntent else {
            return XCTFail(
                "Expected session creation using the current workspace and last Bot")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: requestID,
                payload: sessionReady(latestSequence: 0, modelRoute: "voice-route")
            ))
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.gateway.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))
        XCTAssertEqual(model.chat.realtimeVoiceCall?.sessionID, "chat-1")
        XCTAssertNil(model.newVoiceChatIntent)
        // Cancel before the asynchronous permission request runs.
        model.chat.stopRealtimeVoice()
    }

    func testOpeningEmptyTextComposerDoesNotCreateSessionOrTitle() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        model.bots = [bot(), bot(id: "default", handle: "mobius")]
        model.openNewSession()
        XCTAssertEqual(model.chat.pendingNewChatBotID, "default")
        XCTAssertNil(model.newVoiceChatIntent)
        XCTAssertNil(model.chat.sessionRequestID)
        XCTAssertNil(model.chat.realtimeVoiceCall)
        XCTAssertTrue(model.currentSessionTitle.isEmpty)
        model.chat.composer = "Unsent draft"
        XCTAssertTrue(model.currentSessionTitle.isEmpty)
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { if case .createSession = $0 { true } else { false } })
    }
}

@MainActor
extension AppModelTests {
    func testCallKitVoiceDoesNotSubscribeToGenericAudioInterruptions() throws {
        let model = try voiceModel()
        XCTAssertNil((model.chat.realtimeVoice as Any) as? RTCAudioSessionDelegate)
    }

    func testReadAloudStopCancelsPendingSpeech() async {
        let synthesizer = RecordingSpeechSynthesizer()
        let speaker = MessageSpeaker(synthesizer: synthesizer)
        let spoken = expectation(description: "Current speech delivered")
        speaker.speak("Old speech")
        speaker.stop()
        synthesizer.onSpeak = { spoken.fulfill() }
        speaker.speak("**Current speech**")
        await fulfillment(of: [spoken], timeout: 1)
        XCTAssertEqual(synthesizer.spoken, ["Current speech"])
        speaker.stop()
    }

    func testReleasingOwnerCancelsSuspendedVoiceTaskAndClosesPeer() async throws {
        weak var releasedModel: AppModel?
        let cancellation = expectation(description: "Voice task canceled")
        let peer: RTCPeerConnection
        do {
            let model = try voiceModel()
            releasedModel = model
            XCTAssertTrue(RTCInitializeSSL())
            let factory = RTCPeerConnectionFactory(encoderFactory: nil, decoderFactory: nil)
            let configuration = RTCConfiguration()
            configuration.sdpSemantics = .unifiedPlan
            let constraints = RTCMediaConstraints(
                mandatoryConstraints: nil, optionalConstraints: nil)
            peer = try XCTUnwrap(
                factory.peerConnection(
                    with: configuration, constraints: constraints,
                    delegate: model.chat.realtimeVoice
                ))
            model.chat.realtimeVoice.peer = peer
            model.chat.realtimeVoiceCall = RealtimeVoiceCall(
                requestID: "voice", sessionID: "chat-1")
            model.chat.realtimeVoiceTask = Task {
                do {
                    try await Task.sleep(for: .seconds(3_600))
                } catch {
                    XCTAssertTrue(error is CancellationError)
                    cancellation.fulfill()
                }
            }
        }

        XCTAssertNil(releasedModel)
        await fulfillment(of: [cancellation], timeout: 1)
        XCTAssertEqual(peer.signalingState, .closed)
    }
}

private final class RecordingSpeechSynthesizer: AVSpeechSynthesizer {
    var spoken: [String] = []
    var onSpeak: (() -> Void)?

    override func speak(_ utterance: AVSpeechUtterance) {
        MainActor.preconditionIsolated()
        spoken.append(utterance.speechString)
        onSpeak?()
    }

    override func stopSpeaking(at boundary: AVSpeechBoundary) -> Bool { true }
}
