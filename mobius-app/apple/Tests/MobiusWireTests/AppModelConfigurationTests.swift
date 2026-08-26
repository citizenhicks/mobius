import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testEmptyGatewayCanRegisterItsFirstProviderWithoutAChat() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.applyGatewayCatalog(ReadyPayload(
            machineName: "snowwhite.local",
            sessions: [],
            providers: [ProviderStatus(
                provider: "openai_socket",
                label: "OpenAI",
                symbol: "chat_gpt",
                description: "Persistent Responses API",
                auth: .apiKey,
                defaultBaseUrl: nil,
                defaultApiKeyEnv: "OPENAI_API_KEY",
                models: [ProviderModel(
                    id: "gpt-5.6-sol",
                    label: "Sol",
                    description: "Frontier capability",
                    contextWindow: 1_050_000,
                    reasoning: [ReasoningChoice(
                        id: "high",
                        label: "High",
                        description: "Deep reasoning"
                    )],
                    defaultReasoning: "high"
                )],
                modelIdsConfigurable: false,
                webSearch: webSearchOptions(.off, .cached, .live)
            )],
            providerInstances: [],
            defaultConfig: nil,
            models: [],
            modelProviders: [:],
            middlewareFeatures: [],
            extensions: [],
            contributions: [],
            maxActiveSessions: 4,
            sessionFileLimits: testSessionFileLimits()
        ))

        XCTAssertNil(model.selectedSessionID)
        XCTAssertNil(model.agentDraft)
        model.addProviderInstance("openai_socket")
        XCTAssertEqual(model.providerDraft?.model, "gpt-5.6-sol")
        let instance = try XCTUnwrap(model.providerDraft?.instance)
        XCTAssertFalse(instance.isEmpty)

        model.providerAPIKey = "secret"
        model.saveProviderCredential()
        model.registerProvider()
        try await Task.sleep(for: .milliseconds(20))

        let requests = await recorder.requests()
        guard case .setProviderCredential(_, let credentialInstance, _, _) = try XCTUnwrap(
            requests.first
        ) else {
            return XCTFail("Expected first-provider credential")
        }
        XCTAssertEqual(credentialInstance, instance)
        guard case .registerProvider(
            _,
            let provider,
            _,
            _,
            let modelIDs,
            let reasoningEfforts
        ) = try XCTUnwrap(requests.last) else {
            return XCTFail("Expected first-provider registration")
        }
        XCTAssertEqual(provider.instance, instance)
        XCTAssertEqual(provider, model.providerDraft)
        XCTAssertTrue(modelIDs.isEmpty)
        XCTAssertTrue(reasoningEfforts.isEmpty)

        let defaultConfig = VersionedAgentConfig(revision: 1, config: composition())
        model.applyGatewayCatalog(ready(defaultConfig: defaultConfig))
        XCTAssertNil(model.agentDraft)
        XCTAssertEqual(model.defaultAgentDraft, defaultConfig.config)
    }

    func testProviderCredentialResponseMustMatchSubmittedTarget() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let target = ProviderConfig(
            instance: "openai-new",
            provider: "openai_socket",
            model: "gpt-5.6-sol",
            baseUrl: nil,
            endpointAuth: .credentialless,
            reasoningEffort: "high",
            webSearch: .cached
        )
        let sibling = ProviderConfig(
            instance: "openai-other",
            provider: target.provider,
            model: target.model,
            baseUrl: nil,
            endpointAuth: target.endpointAuth,
            reasoningEffort: target.reasoningEffort,
            webSearch: target.webSearch
        )
        model.providerDraft = target
        model.providerLabelDraft = "Personal"
        model.providerInstances = [target, sibling].map { config in
            ProviderInstance(
                label: "Setup",
                tint: .blue,
                configured: false,
                credentialHint: "old1",
                selection: config,
                modelIds: [],
                reasoningEfforts: []
            )
        }
        model.providerAPIKey = "secret"

        model.saveProviderCredential()
        XCTAssertEqual(model.providerDraft?.endpointAuth, .providerDefault)
        let request = await recorder.firstRequest(after: 0) { request in
            if case .setProviderCredential = request { return true }
            return false
        }
        guard case .setProviderCredential(let requestID, _, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected provider credential request") }

        model.handle(.providerCredentialSaved(
            requestID: "stale",
            instance: sibling.instance,
            provider: sibling.provider
        ))
        model.handle(.providerCredentialSaved(
            requestID: requestID,
            instance: sibling.instance,
            provider: sibling.provider
        ))
        model.handle(.providerCredentialSaved(
            requestID: requestID,
            instance: target.instance,
            provider: "kimi"
        ))

        XCTAssertEqual(model.providerInstances.map(\.configured), [false, false])
        XCTAssertEqual(model.providerAPIKey, "secret")
        XCTAssertEqual(model.providerActionState, .savingCredential(target.instance))

        model.handle(.providerCredentialSaved(
            requestID: requestID,
            instance: target.instance,
            provider: target.provider
        ))

        XCTAssertEqual(model.providerInstances.map(\.configured), [true, false])
        XCTAssertEqual(model.providerInstances.map(\.credentialHint), ["cret", "old1"])
        XCTAssertEqual(model.providerAPIKey, "")
        XCTAssertEqual(model.providerActionState, .credentialSaved(target.instance))
        XCTAssertEqual(model.toast?.message, "Personal credential saved.")
    }

    func testProviderRemovalKeepsDetailUntilConfirmedCatalogRefresh() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let selection = composition().provider
        let record = ProviderInstance(
            label: "Work",
            tint: .purple,
            configured: true,
            selection: selection,
            modelIds: [],
            reasoningEfforts: []
        )
        model.connectionState = .ready

        var newDraft = selection
        newDraft.instance = "new-draft"
        model.providerDraft = newDraft
        model.removeProvider(newDraft.instance)
        let draftRequestCount = await recorder.requestCount()
        XCTAssertEqual(draftRequestCount, 0)

        model.providerInstances = [record]
        model.editProviderInstance(record)
        model.navigationPath = [.settings(.provider(record.instance))]
        model.removeProvider(record.instance)

        let firstRequest = await recorder.firstRequest(after: 0) { request in
            if case .removeProvider = request { return true }
            return false
        }
        guard case .removeProvider(let firstRequestID, let instance) = try XCTUnwrap(firstRequest)
        else { return XCTFail("Expected provider removal request") }
        XCTAssertEqual(instance, record.instance)
        XCTAssertEqual(model.navigationPath, [.settings(.provider(record.instance))])
        XCTAssertTrue(model.isApplyingConfiguration)

        model.handle(.rejected(GatewayRejection(
            requestId: firstRequestID,
            code: "provider_in_use",
            message: "Provider is still in use",
            fatal: false
        )))

        XCTAssertEqual(model.navigationPath, [.settings(.provider(record.instance))])
        XCTAssertEqual(model.providerActionState, .failed("Provider is still in use"))
        XCTAssertEqual(model.toast?.message, "Provider is still in use")
        XCTAssertNil(model.pendingProviderRemoval)

        let requestCount = await recorder.requestCount()
        model.removeProvider(record.instance)
        let retry = await recorder.firstRequest(after: requestCount) { request in
            if case .removeProvider = request { return true }
            return false
        }
        guard case .removeProvider(let retryRequestID, _) = try XCTUnwrap(retry)
        else { return XCTFail("Expected provider removal retry") }

        model.handle(.gatewayConfigured(
            requestID: retryRequestID,
            payload: ReadyPayload(
                machineName: "snowwhite.local",
                sessions: [],
                providers: [providerStatus(for: selection)],
                providerInstances: [],
                defaultConfig: nil,
                models: [],
                modelProviders: [:],
                middlewareFeatures: [],
                extensions: [],
                contributions: [],
                maxActiveSessions: 4,
                sessionFileLimits: testSessionFileLimits()
            )
        ))

        XCTAssertTrue(model.navigationPath.isEmpty)
        XCTAssertTrue(model.providerInstances.isEmpty)
        XCTAssertNil(model.providerDraft)
        XCTAssertNil(model.pendingProviderRemoval)
        XCTAssertEqual(model.providerActionState, .idle)
        XCTAssertEqual(model.toast?.message, "Work removed.")
    }

    func testProviderSelectionUsesGatewayManifestDefaults() throws {
        let model = try model()
        model.defaultAgentDraft = AgentComposition(
            provider: ProviderConfig(
                provider: "old",
                model: "old-model",
                baseUrl: nil,
                reasoningEffort: nil,
                webSearch: .live
            ),
            middleware: MiddlewareConfig(
                enabled: [],
                settings: [
                    "context_offloading": ["stale_after_tokens": .integer(50_000)]
                ]
            ),
            extensions: [],
            systemPrompt: "Test",
            maxModelSteps: 256
        )
        model.providerStatuses = [ProviderStatus(
            provider: "kimi",
            label: "Kimi",
            symbol: "kimi",
            description: "Kimi Chat Completions API",
                auth: .apiKey,
            defaultBaseUrl: nil,
            defaultApiKeyEnv: "MOONSHOT_API_KEY",
            models: [
                ProviderModel(
                    id: "kimi-k3",
                    label: "Kimi K3",
                    description: "Agentic coding model",
                    contextWindow: 1_048_576,
                    reasoning: [ReasoningChoice(
                        id: "max",
                        label: "Maximum",
                        description: "Maximum reasoning"
                    )],
                    defaultReasoning: "max"
                ),
                ProviderModel(
                    id: "kimi-k2.7-code",
                    label: "Kimi K2.7 Code",
                    description: "Coding model",
                    contextWindow: 262_144,
                    reasoning: [],
                    defaultReasoning: nil
                )
            ],
            modelIdsConfigurable: false,
            webSearch: webSearchOptions(.off)
        )]

        model.addProviderInstance("kimi")

        XCTAssertEqual(model.providerDraft?.model, "kimi-k3")
        XCTAssertEqual(model.providerDraft?.reasoningEffort, "max")
        XCTAssertEqual(model.providerDraft?.webSearch, .off)
    }

    func testConfigurableProviderCanonicalizesAndSavesModelAndReasoningCatalogs() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let selection = ProviderConfig(
            instance: "responses-local",
            provider: "responses",
            model: "old-model",
            baseUrl: "http://localhost:8080/v1",
            reasoningEffort: nil,
            webSearch: .off
        )
        model.providerStatuses = [ProviderStatus(
            provider: selection.provider,
            label: "Local",
            symbol: "storage",
            description: "OpenAI-compatible endpoint",
            auth: .apiKey,
            defaultBaseUrl: "http://localhost:8080/v1",
            defaultApiKeyEnv: nil,
            models: [],
            modelIdsConfigurable: true,
            webSearch: webSearchOptions(.off)
        )]
        model.providerDraft = selection
        model.providerLabelDraft = "Local"
        model.updateProviderModelIDs(" model-a, model-b, , model-a ")
        model.updateProviderReasoningEfforts(" high, medium, , high ")

        XCTAssertEqual(model.providerModelIDs, ["model-a", "model-b"])
        XCTAssertEqual(model.providerReasoningEfforts, ["high", "medium"])
        model.registerProvider()
        try await Task.sleep(for: .milliseconds(20))

        let requests = await recorder.requests()
        let request = try XCTUnwrap(requests.first)
        guard case .registerProvider(
            _,
            let config,
            _,
            _,
            let modelIDs,
            let reasoningEfforts
        ) = request else {
            return XCTFail("Expected provider registration")
        }
        XCTAssertEqual(modelIDs, ["model-a", "model-b"])
        XCTAssertEqual(reasoningEfforts, ["high", "medium"])
        XCTAssertEqual(config.model, "model-a")
        XCTAssertEqual(config.reasoningEffort, "high")
    }

    func testScopedModelSelectionUsesGatewayProviderIdentity() throws {
        let model = try model()
        let target = ProviderConfig(
            instance: "kimi-work",
            provider: "kimi",
            model: "kimi-k3",
            baseUrl: nil,
            reasoningEffort: "max",
            webSearch: .off
        )
        let choice = ModelChoice(
            route: "opaque-route",
            group: "Kimi · K3",
            model: target.model,
            reasoningEffort: target.reasoningEffort,
            contextWindow: 1_048_576,
            supportsImageInput: true
        )
        let original = composition()
        model.agentSnapshot = VersionedAgentConfig(revision: 1, config: original)
        model.defaultAgentSnapshot = VersionedAgentConfig(revision: 1, config: original)
        model.agentDraft = original
        model.defaultAgentDraft = original
        model.modelChoices = [choice]
        model.modelProviders = [choice.route: target.instance]
        model.providerStatuses = [providerStatus(for: target)]
        model.providerInstances = [ProviderInstance(
            label: "Work",
            tint: .blue,
            configured: true,
            selection: target,
            modelIds: [],
            reasoningEfforts: []
        )]

        model.selectAgentDraftModel(choice.route)

        XCTAssertEqual(model.agentDraft?.provider, target)
        XCTAssertEqual(model.agentDraftModelRoute, choice.route)
        XCTAssertEqual(model.defaultAgentDraft, original)
        XCTAssertNotEqual(model.agentDraft, model.agentSnapshot?.config)

        model.agentDraft = original
        model.selectDefaultAgentDraftModel(choice.route)

        XCTAssertEqual(model.defaultAgentDraft?.provider, target)
        XCTAssertEqual(model.defaultAgentDraftModelRoute, choice.route)
        XCTAssertEqual(model.agentDraft, original)
    }

    func testModelChoicesKeepSiblingProviderInstancesSeparate() throws {
        let model = try model()
        let work = ProviderConfig(
            instance: "openai-work",
            provider: "openai_socket",
            model: "gpt-5.6-sol",
            baseUrl: nil,
            reasoningEffort: "medium",
            webSearch: .cached
        )
        let personal = ProviderConfig(
            instance: "openai-personal",
            provider: work.provider,
            model: work.model,
            baseUrl: nil,
            reasoningEffort: "medium",
            webSearch: .cached
        )
        let choices = [
            ModelChoice(
                route: "work-medium",
                group: "Work · Sol",
                model: work.model,
                reasoningEffort: "medium",
                contextWindow: nil,
                supportsImageInput: true
            ),
            ModelChoice(
                route: "work-high",
                group: "Work · Sol",
                model: work.model,
                reasoningEffort: "high",
                contextWindow: nil,
                supportsImageInput: true
            ),
            ModelChoice(
                route: "personal-medium",
                group: "Personal · Sol",
                model: personal.model,
                reasoningEffort: "medium",
                contextWindow: nil,
                supportsImageInput: true
            ),
        ]
        model.modelChoices = choices
        model.modelProviders = [
            "work-medium": work.instance,
            "work-high": work.instance,
            "personal-medium": personal.instance,
        ]

        XCTAssertEqual(
            model.distinctModels(in: choices).map(\.route),
            ["work-medium", "personal-medium"]
        )
        XCTAssertEqual(
            model.modelChoices(matching: choices[0], in: choices).map(\.route),
            ["work-medium", "work-high"]
        )

        var draft = composition()
        draft.provider = personal
        model.defaultAgentDraft = draft
        XCTAssertEqual(model.defaultAgentDraftModelRoute, "personal-medium")
    }

    func testModelLabelUsesProviderFriendlyName() throws {
        let model = try model()
        let config = ProviderConfig(
            instance: "openai-work",
            provider: "openai_socket",
            model: "gpt-5.6-sol",
            baseUrl: nil,
            reasoningEffort: "high",
            webSearch: .cached
        )
        let choice = ModelChoice(
            route: "opaque-route",
            group: "OpenAI · Sol",
            model: config.model,
            reasoningEffort: config.reasoningEffort,
            contextWindow: 128_000,
            supportsImageInput: true
        )
        let canonicalChoice = ModelChoice(
            route: "openrouter-route",
            group: "OpenRouter · openai/gpt-5.6-luna",
            model: "openai/gpt-5.6-luna",
            reasoningEffort: "high",
            contextWindow: 128_000,
            supportsImageInput: true
        )
        model.modelProviders = [
            choice.route: config.instance,
            canonicalChoice.route: config.instance,
        ]
        model.providerStatuses = [providerStatus(for: config, models: [ProviderModel(
            id: config.model,
            label: "Sol",
            description: "Coding model",
            contextWindow: 128_000,
            reasoning: [],
            defaultReasoning: "high"
        ), ProviderModel(
            id: "gpt-5.6-luna",
            label: "Luna",
            description: "Fast coding model",
            contextWindow: 128_000,
            reasoning: [],
            defaultReasoning: "high"
        )])]
        model.providerInstances = [ProviderInstance(
            label: "Work",
            tint: .teal,
            configured: true,
            selection: config,
            modelIds: [],
            reasoningEfforts: []
        )]

        XCTAssertEqual(model.modelLabel(for: choice), "Sol")
        XCTAssertEqual(model.modelLabel(for: canonicalChoice), "Luna")
        XCTAssertEqual(model.modelGroupLabel(for: canonicalChoice), "OpenRouter · Luna")
        XCTAssertEqual(canonicalChoice.model, "openai/gpt-5.6-luna")
        XCTAssertEqual(
            model.modelLabel(provider: config.instance, modelID: "acme/custom-model"),
            "acme/custom-model"
        )
        XCTAssertEqual(model.modelLabel(
            for: ModelChoice(
                route: "custom-route",
                group: "Custom",
                model: "custom-model",
                reasoningEffort: nil,
                contextWindow: nil,
                supportsImageInput: false
            )
        ), "custom-model")
    }

    func testProviderLabelsUseAdvertisedNames() throws {
        let model = try model()
        let codex = ProviderConfig(
            provider: "openai_codex",
            model: "gpt-5.4",
            baseUrl: nil,
            reasoningEffort: "high",
            webSearch: .off
        )
        model.providerStatuses = [
            providerStatus(for: codex, label: "Codex"),
            providerStatus(for: ProviderConfig(
                provider: "openai_socket",
                model: "gpt-5.6-sol",
                baseUrl: nil,
                reasoningEffort: "high",
                webSearch: .cached
            ), label: "OpenAI"),
            providerStatus(for: ProviderConfig(
                provider: "responses",
                model: "local-model",
                baseUrl: "http://localhost:8080/v1",
                reasoningEffort: nil,
                webSearch: .off
            ), label: "Local")
        ]

        XCTAssertEqual(model.providerLabel(for: "openai_codex"), "Codex")
        XCTAssertEqual(model.providerLabel(for: "openai_socket"), "OpenAI")
        XCTAssertEqual(model.providerLabel(for: "responses"), "Local")

        var newSetup = codex
        newSetup.instance = "new-setup-uuid"
        model.providerDraft = newSetup
        model.providerLabelDraft = "Personal"
        XCTAssertEqual(model.providerLabel(for: newSetup.instance), "Personal")
        XCTAssertEqual(model.providerLabel(for: newSetup.provider), "Codex")
    }

    func testMiddlewareSettingsSetAndClearWithoutCapabilityLogic() {
        var middleware = MiddlewareConfig(enabled: ["example"], settings: [:])

        middleware.setSetting(.string("route-a"), middleware: "example", setting: "route")
        XCTAssertEqual(middleware.settings["example"]?["route"], .string("route-a"))

        middleware.setSetting(nil, middleware: "example", setting: "route")
        XCTAssertNil(middleware.settings["example"])
    }

    func testGatewayDefaultRefreshDoesNotOverwriteActiveAgentDraft() throws {
        let model = try model()
        let active = AgentComposition(
            provider: ProviderConfig(
                provider: "openai_socket",
                model: "gpt-5.6-sol",
                baseUrl: nil,
                reasoningEffort: "medium",
                webSearch: .cached
            ),
            middleware: MiddlewareConfig(
                enabled: ["extensions"],
                settings: [
                    "context_offloading": ["stale_after_tokens": .integer(50_000)]
                ]
            ),
            extensions: [],
            systemPrompt: "Active",
            maxModelSteps: 256
        )
        var edited = active
        edited.systemPrompt = "Unsaved active edit"
        var gatewayDefault = active
        gatewayDefault.systemPrompt = "New chat default"
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: active)
        model.agentDraft = edited
        model.defaultAgentSnapshot = VersionedAgentConfig(revision: 7, config: active)
        model.defaultAgentDraft = active

        model.applyGatewayCatalog(ReadyPayload(
            machineName: "snowwhite.local",
            sessions: [],
            providers: [],
            providerInstances: [],
            defaultConfig: VersionedAgentConfig(revision: 8, config: gatewayDefault),
            models: [],
            modelProviders: [:],
            middlewareFeatures: [],
            extensions: [],
            contributions: [],
            maxActiveSessions: 4,
            sessionFileLimits: testSessionFileLimits()
        ))

        XCTAssertEqual(model.agentSnapshot, VersionedAgentConfig(revision: 3, config: active))
        XCTAssertEqual(model.agentDraft, edited)
        XCTAssertEqual(model.defaultAgentDraft, gatewayDefault)
        XCTAssertEqual(
            model.defaultAgentSnapshot,
            VersionedAgentConfig(revision: 8, config: gatewayDefault)
        )
    }

    func testClearingCurrentChatPreservesGatewayDefaultSettings() throws {
        let model = try model()
        let active = composition(systemPrompt: "Active chat")
        let gatewayDefault = composition(systemPrompt: "New chats")
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.sessions = [session(state: .idle)]
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: active)
        model.agentDraft = active
        model.chatAgentApplyState = .failed("Chat failure")
        model.defaultAgentSnapshot = VersionedAgentConfig(revision: 8, config: gatewayDefault)
        model.defaultAgentDraft = gatewayDefault
        model.defaultAgentApplyState = .failed("Default failure")

        model.applySessions([])

        XCTAssertNil(model.selectedSessionID)
        XCTAssertNil(model.agentSnapshot)
        XCTAssertNil(model.agentDraft)
        XCTAssertEqual(model.chatAgentApplyState, .idle)
        XCTAssertEqual(
            model.defaultAgentSnapshot,
            VersionedAgentConfig(revision: 8, config: gatewayDefault)
        )
        XCTAssertEqual(model.defaultAgentDraft, gatewayDefault)
        XCTAssertEqual(model.defaultAgentApplyState, .failed("Default failure"))
    }

    func testComposerSettingConfiguresTheActiveChatWithoutCapabilityLogic() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        var active = composition()
        active.middleware.setSetting(
            .string("safe"),
            middleware: "example",
            setting: "access"
        )
        model.selectedSessionID = "chat-1"
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: active)
        model.agentDraft = active

        model.setAgentSettingForCurrentChat(
            .string("broader"),
            middleware: "example",
            setting: "access"
        )
        try await Task.sleep(for: .milliseconds(20))

        let requests = await recorder.requests()
        let request = try XCTUnwrap(requests.first)
        guard case .configureSession(_, _, let expectedRevision, let config) = request else {
            return XCTFail("Expected composer setting to configure the active chat")
        }
        XCTAssertEqual(expectedRevision, 3)
        XCTAssertEqual(
            config.middleware.settings["example"]?["access"],
            .string("broader")
        )
    }

    func testProviderRegistrationDoesNotConfigureDefaultAgent() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        let draft = composition(systemPrompt: "New default")
        let previousDefault = composition(systemPrompt: "Previous default")
        model.selectedSessionID = "chat-1"
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: composition())
        model.agentDraft = composition(systemPrompt: "Active chat")
        model.defaultAgentSnapshot = VersionedAgentConfig(
            revision: 8,
            config: previousDefault
        )
        model.defaultAgentDraft = previousDefault
        model.providerStatuses = [providerStatus(for: draft.provider)]
        model.providerDraft = draft.provider
        model.providerLabelDraft = "Work"

        model.registerProvider()
        try await Task.sleep(for: .milliseconds(20))

        let registrationRequests = await recorder.requests()
        let registration = try XCTUnwrap(
            registrationRequests.lazy.compactMap { request -> String? in
                guard case .registerProvider(let requestID, _, _, _, _, _) = request else {
                    return nil
                }
                return requestID
            }.first
        )
        let response = ready(defaultConfig: VersionedAgentConfig(
            revision: 8,
            config: previousDefault
        ))
        model.applyGatewayConfigurationResponse(requestID: registration, payload: response)
        try await Task.sleep(for: .milliseconds(20))

        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains {
            if case .configureDefaultAgent = $0 { return true }
            return false
        })
        XCTAssertEqual(model.defaultAgentDraft, previousDefault)
        XCTAssertEqual(model.agentDraft?.systemPrompt, "Active chat")
    }

    func testSavingDefaultLeavesActiveChatUntouched() async throws {
        let recorder = GatewayRequestRecorder()
        let defaultSaved = expectation(description: "Default agent saved")
        let sessionConfigured = expectation(description: "Active chat configured")
        sessionConfigured.isInverted = true
        let model = try model { request in
            await recorder.record(request)
            if case .configureDefaultAgent = request { defaultSaved.fulfill() }
            if case .configureSession = request { sessionConfigured.fulfill() }
        }
        let active = composition()
        var draft = active
        draft.middleware.setSetting(
            .string("first"),
            middleware: "example",
            setting: "mode"
        )
        model.selectedSessionID = "chat-1"
        model.sessions = [session(state: .idle)]
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: active)
        model.defaultAgentSnapshot = VersionedAgentConfig(revision: 7, config: active)
        model.agentDraft = active
        model.defaultAgentDraft = draft

        model.saveAgentAsDefault()
        await fulfillment(of: [defaultSaved], timeout: 1)

        let defaultRequests = await recorder.requests()
        let defaultRequest = try XCTUnwrap(
            defaultRequests.first { request in
                if case .configureDefaultAgent = request { return true }
                return false
            }
        )
        guard case .configureDefaultAgent(let requestID, _, let savedDraft) = defaultRequest else {
            return XCTFail("Expected default-agent configuration")
        }
        XCTAssertEqual(savedDraft, draft)

        let response = ready(
            defaultConfig: VersionedAgentConfig(revision: 8, config: draft)
        )
        model.handle(.ready(response))
        XCTAssertEqual(model.defaultAgentDraft, draft)
        model.applyGatewayConfigurationResponse(requestID: requestID, payload: response)
        await fulfillment(of: [sessionConfigured], timeout: 0.05)

        let sessionRequests = await recorder.requests()
        XCTAssertFalse(sessionRequests.contains {
            if case .configureSession = $0 { return true }
            return false
        })
        XCTAssertEqual(model.agentDraft, active)
        XCTAssertEqual(model.defaultAgentDraft, draft)
        XCTAssertEqual(model.defaultAgentApplyState, .applied)
        XCTAssertEqual(model.chatAgentApplyState, .idle)
    }

    func testSavingDefaultPreservesALaterDefaultDraft() async throws {
        let recorder = GatewayRequestRecorder()
        let defaultSaved = expectation(description: "Default agent saved")
        let sessionConfigured = expectation(description: "Active chat configured")
        sessionConfigured.isInverted = true
        let model = try model { request in
            await recorder.record(request)
            if case .configureDefaultAgent = request { defaultSaved.fulfill() }
            if case .configureSession = request { sessionConfigured.fulfill() }
        }
        let active = composition()
        var draft = active
        draft.middleware.setSetting(
            .string("first"),
            middleware: "example",
            setting: "mode"
        )
        model.selectedSessionID = "chat-1"
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: active)
        model.defaultAgentSnapshot = VersionedAgentConfig(revision: 7, config: active)
        model.agentDraft = active
        model.defaultAgentDraft = draft

        model.saveAgentAsDefault()
        await fulfillment(of: [defaultSaved], timeout: 1)
        let requests = await recorder.requests()
        let requestID = try XCTUnwrap(requests.lazy.compactMap { request -> String? in
            guard case .configureDefaultAgent(let requestID, _, _) = request else { return nil }
            return requestID
        }.first)

        var laterDefaultDraft = draft
        laterDefaultDraft.middleware.setSetting(
            .string("second"),
            middleware: "example",
            setting: "mode"
        )
        model.defaultAgentDraft = laterDefaultDraft
        let response = ready(
            defaultConfig: VersionedAgentConfig(revision: 8, config: draft)
        )
        model.handle(.ready(response))
        XCTAssertEqual(model.defaultAgentDraft, laterDefaultDraft)
        model.applyGatewayConfigurationResponse(requestID: requestID, payload: response)
        await fulfillment(of: [sessionConfigured], timeout: 0.05)

        let configuredSessions = await recorder.requests().filter { request in
            if case .configureSession = request { return true }
            return false
        }
        XCTAssertTrue(configuredSessions.isEmpty)
        XCTAssertEqual(model.agentDraft, active)
        XCTAssertEqual(model.defaultAgentDraft, laterDefaultDraft)
        XCTAssertEqual(model.defaultAgentApplyState, .applied)
        XCTAssertEqual(model.chatAgentApplyState, .idle)
    }

    func testInstallingAnExtensionUsesTheGatewayRefreshAsItsResult() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready

        model.extensionInstallSource = " https://github.com/DietrichGebert/ponytail.git "
        model.installExtension()

        let request = await recorder.firstRequest(after: 0) {
            if case .installExtension = $0 { return true }
            return false
        }
        guard case .installExtension(
            let requestID,
            let source,
            let reference,
            let subdirectory
        ) = try XCTUnwrap(request) else {
            return XCTFail("Expected an extension install request")
        }
        XCTAssertEqual(source, "https://github.com/DietrichGebert/ponytail.git")
        XCTAssertNil(reference)
        XCTAssertNil(subdirectory)
        XCTAssertEqual(model.extensionAction, .installing)

        let installed = extensionRecord()
        let response = ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: composition()),
            extensions: [installed]
        )
        model.applyGatewayConfigurationResponse(requestID: requestID, payload: response)

        XCTAssertEqual(model.extensions, [installed])
        XCTAssertNil(model.extensionAction)
        XCTAssertTrue(model.extensionInstallSource.isEmpty)
        XCTAssertEqual(model.toast?.message, "Extension installed.")
    }

    func testInstallingCatalogExtensionRelaysItsGenericSourceFields() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        let item = MobiusCloudExtensionCatalogItem(
            id: "ponytail",
            name: "Ponytail",
            description: "Prefer the smallest correct implementation.",
            source: MobiusCloudExtensionSource(
                url: "https://github.com/DietrichGebert/ponytail.git",
                reference: "v4.9.0",
                subdirectory: nil
            )
        )
        model.availableExtensions = [item]

        model.installExtension(item)

        let request = await recorder.firstRequest(after: 0) {
            if case .installExtension = $0 { return true }
            return false
        }
        guard case .installExtension(
            _,
            let source,
            let reference,
            let subdirectory
        ) = try XCTUnwrap(request) else {
            return XCTFail("Expected a catalog extension install request")
        }
        XCTAssertEqual(source, item.source.url)
        XCTAssertEqual(reference, item.source.reference)
        XCTAssertEqual(subdirectory, item.source.subdirectory)
    }

    func testHookTrustChangesAreBoundToTheInstalledDigest() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready

        let untrusted = extensionRecord(hooksTrusted: false)
        model.trustHooks(for: untrusted)

        let trust = await recorder.firstRequest(after: 0) {
            if case .trustExtensionHooks = $0 { return true }
            return false
        }
        guard case .trustExtensionHooks(
            let trustRequestID,
            let trustID,
            let trustDigest
        ) = try XCTUnwrap(trust) else {
            return XCTFail("Expected a hook trust request")
        }
        XCTAssertEqual(trustID, untrusted.id)
        XCTAssertEqual(trustDigest, untrusted.digest)
        XCTAssertEqual(model.extensionAction, .trusting(untrusted.name))

        model.completeExtensionAction(requestID: trustRequestID)
        XCTAssertEqual(model.toast?.message, "\(untrusted.name) hooks trusted.")
        let trusted = extensionRecord()
        model.untrustHooks(for: trusted)

        let untrust = await recorder.firstRequest(after: 1) {
            if case .revokeExtensionHooksTrust = $0 { return true }
            return false
        }
        guard case .revokeExtensionHooksTrust(
            _,
            let untrustID,
            let untrustDigest
        ) = try XCTUnwrap(untrust) else {
            return XCTFail("Expected a hook trust revocation request")
        }
        XCTAssertEqual(untrustID, trusted.id)
        XCTAssertEqual(untrustDigest, trusted.digest)
        XCTAssertEqual(model.extensionAction, .untrusting(trusted.name))
    }

    func testCatalogRefreshPreservesStableMissingExtensionReferences() throws {
        let model = try model()
        let snapshot = VersionedAgentConfig(revision: 1, config: composition())
        var unsavedDefault = snapshot.config
        unsavedDefault.extensions = ["plugin:ponytail"]
        var unsavedChat = snapshot.config
        unsavedChat.extensions = ["plugin:ponytail"]
        model.defaultAgentSnapshot = snapshot
        model.defaultAgentDraft = unsavedDefault
        model.agentDraft = unsavedChat

        model.applyGatewayCatalog(ready(defaultConfig: snapshot))

        XCTAssertEqual(model.defaultAgentDraft?.extensions, ["plugin:ponytail"])
        XCTAssertEqual(model.agentDraft?.extensions, ["plugin:ponytail"])
    }

    func testFatalGatewayErrorClearsAnExtensionAction() throws {
        let model = try model { _ in }
        model.connectionState = .ready
        model.extensionInstallSource = "https://github.com/DietrichGebert/ponytail.git"
        model.installExtension()
        XCTAssertEqual(model.extensionAction, .installing)

        model.handle(.error(GatewayFailure(
            code: "internal",
            message: "Gateway failed.",
            fatal: true
        )))

        XCTAssertNil(model.extensionAction)
        XCTAssertNil(model.extensionRequestID)
        XCTAssertEqual(
            model.extensionInstallSource,
            "https://github.com/DietrichGebert/ponytail.git"
        )
    }

}
