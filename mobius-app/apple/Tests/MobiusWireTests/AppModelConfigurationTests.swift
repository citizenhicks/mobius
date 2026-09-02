import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testEmptyGatewayCanRegisterItsFirstProviderWithoutAChat() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.applyGatewayCatalog(ReadyPayload(
            machineName: "snowwhite.local",
            bots: [],
            sessions: [],
            swarms: [],
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
                    defaultReasoning: "high",
                    toolDiscovery: .native
                )],
                modelIdsConfigurable: false,
                webSearch: webSearchOptions(.off, .cached, .live),
                toolDiscovery: .native,
                customEndpointToolDiscovery: nil
            )],
            providerInstances: [],
            botDefaults: nil,
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

        let botDefaults = VersionedAgentConfig(revision: 1, config: composition())
        model.applyGatewayCatalog(ready(botDefaults: botDefaults))
        XCTAssertNil(model.agentDraft)
        XCTAssertEqual(model.botDefaultsDraft, botDefaults.config)
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
                bots: [],
                sessions: [],
                swarms: [],
                providers: [providerStatus(for: selection)],
                providerInstances: [],
                botDefaults: nil,
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
        model.botDefaultsDraft = AgentComposition(
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
                    defaultReasoning: "max",
                    toolDiscovery: .rebuild
                ),
                ProviderModel(
                    id: "kimi-k2.7-code",
                    label: "Kimi K2.7 Code",
                    description: "Coding model",
                    contextWindow: 262_144,
                    reasoning: [],
                    defaultReasoning: nil,
                    toolDiscovery: .rebuild
                )
            ],
            modelIdsConfigurable: false,
            webSearch: webSearchOptions(.off),
            toolDiscovery: .rebuild,
            customEndpointToolDiscovery: nil
        )]

        model.addProviderInstance("kimi")

        XCTAssertEqual(model.providerDraft?.model, "kimi-k3")
        XCTAssertEqual(model.providerDraft?.reasoningEffort, "max")
        XCTAssertEqual(model.providerDraft?.webSearch, .off)
    }

    func testProviderToolDiscoveryUsesCustomEndpointOverride() {
        let config = ProviderConfig(
            provider: "openrouter",
            model: "openai/gpt-5.6-luna",
            baseUrl: "https://openrouter.ai/api/v1",
            reasoningEffort: "high",
            webSearch: .live
        )
        let status = providerStatus(
            for: config,
            models: [ProviderModel(
                id: config.model,
                label: "Luna",
                description: "Test model",
                contextWindow: 200_000,
                reasoning: [],
                defaultReasoning: nil,
                toolDiscovery: .native
            )],
            toolDiscovery: .rebuild,
            customEndpointToolDiscovery: .rebuild
        )

        XCTAssertEqual(status.resolvedToolDiscovery(model: config.model, baseURL: nil), .native)
        XCTAssertEqual(
            status.resolvedToolDiscovery(model: config.model, baseURL: config.baseUrl),
            .native
        )
        XCTAssertEqual(
            status.resolvedToolDiscovery(
                model: config.model,
                baseURL: "https://proxy.example/v1"
            ),
            .rebuild
        )
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
            webSearch: webSearchOptions(.off),
            toolDiscovery: .rebuild,
            customEndpointToolDiscovery: nil
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
            supportsImageInput: true,
            toolDiscovery: .rebuild
        )
        let original = composition()
        model.botDefaultsSnapshot = VersionedAgentConfig(revision: 1, config: original)
        model.botDraft = original
        model.botDefaultsDraft = original
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

        model.selectBotDraftModel(choice.route)

        XCTAssertEqual(model.botDraft?.provider, target)
        XCTAssertEqual(model.botDraftModelRoute, choice.route)
        XCTAssertEqual(model.botDefaultsDraft, original)
        XCTAssertNotEqual(model.botDraft, original)

        model.botDraft = original
        model.selectBotDefaultsDraftModel(choice.route)

        XCTAssertEqual(model.botDefaultsDraft?.provider, target)
        XCTAssertEqual(model.botDefaultsDraftModelRoute, choice.route)
        XCTAssertEqual(model.botDraft, original)
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
                supportsImageInput: true,
                toolDiscovery: .native
            ),
            ModelChoice(
                route: "work-high",
                group: "Work · Sol",
                model: work.model,
                reasoningEffort: "high",
                contextWindow: nil,
                supportsImageInput: true,
                toolDiscovery: .native
            ),
            ModelChoice(
                route: "personal-medium",
                group: "Personal · Sol",
                model: personal.model,
                reasoningEffort: "medium",
                contextWindow: nil,
                supportsImageInput: true,
                toolDiscovery: .native
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
        model.botDefaultsDraft = draft
        XCTAssertEqual(model.botDefaultsDraftModelRoute, "personal-medium")
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
            supportsImageInput: true,
            toolDiscovery: .native
        )
        let canonicalChoice = ModelChoice(
            route: "openrouter-route",
            group: "OpenRouter · openai/gpt-5.6-luna",
            model: "openai/gpt-5.6-luna",
            reasoningEffort: "high",
            contextWindow: 128_000,
            supportsImageInput: true,
            toolDiscovery: .native
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
            defaultReasoning: "high",
            toolDiscovery: .native
        ), ProviderModel(
            id: "gpt-5.6-luna",
            label: "Luna",
            description: "Fast coding model",
            contextWindow: 128_000,
            reasoning: [],
            defaultReasoning: "high",
            toolDiscovery: .native
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
                supportsImageInput: false,
                toolDiscovery: .rebuild
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

    func testBotDefaultsRefreshDoesNotOverwriteActiveAgentDraft() throws {
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
        var botDefaults = active
        botDefaults.systemPrompt = "New Bot defaults"
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: active)
        model.agentDraft = edited
        model.botDefaultsSnapshot = VersionedAgentConfig(revision: 7, config: active)
        model.botDefaultsDraft = active

        model.applyGatewayCatalog(ReadyPayload(
            machineName: "snowwhite.local",
            bots: [],
            sessions: [],
            swarms: [],
            providers: [],
            providerInstances: [],
            botDefaults: VersionedAgentConfig(revision: 8, config: botDefaults),
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
        XCTAssertEqual(model.botDefaultsDraft, botDefaults)
        XCTAssertEqual(
            model.botDefaultsSnapshot,
            VersionedAgentConfig(revision: 8, config: botDefaults)
        )
    }

    func testClearingCurrentChatPreservesBotDefaultsSettings() throws {
        let model = try model()
        let active = composition(systemPrompt: "Active chat")
        let botDefaults = composition(systemPrompt: "New chats")
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.sessions = [session(state: .idle)]
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: active)
        model.agentDraft = active
        model.botDefaultsSnapshot = VersionedAgentConfig(revision: 8, config: botDefaults)
        model.botDefaultsDraft = botDefaults
        model.botDefaultsApplyState = .failed("Bot defaults failure")

        model.applySessions([])

        XCTAssertNil(model.selectedSessionID)
        XCTAssertNil(model.agentSnapshot)
        XCTAssertNil(model.agentDraft)
        XCTAssertEqual(
            model.botDefaultsSnapshot,
            VersionedAgentConfig(revision: 8, config: botDefaults)
        )
        XCTAssertEqual(model.botDefaultsDraft, botDefaults)
        XCTAssertEqual(model.botDefaultsApplyState, .failed("Bot defaults failure"))
    }

    func testSavingBotDraftUsesBotIdentityRevisionAndConfiguration() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        var edited = composition()
        edited.middleware.setSetting(
            .string("safe"),
            middleware: "example",
            setting: "access"
        )
        let helper = bot(config: VersionedAgentConfig(revision: 3, config: composition()))
        model.connectionState = .ready
        model.bots = [helper]
        model.beginEditingBot(helper)
        model.botNameDraft = "Durable Helper"
        model.botDescriptionDraft = "Reviews durable changes."
        model.botTintDraft = .purple
        model.botDraft = edited

        model.saveBotDraft()
        let request = await recorder.firstRequest(after: 0) {
            if case .updateBot = $0 { return true }
            return false
        }
        guard case .updateBot(
            let requestID,
            let id,
            let expectedRevision,
            let name,
            let description,
            let tint,
            let config
        ) = try XCTUnwrap(request) else {
            return XCTFail("Expected the Bot draft to be saved")
        }
        XCTAssertEqual(id, "bot-1")
        XCTAssertEqual(expectedRevision, 3)
        XCTAssertEqual(name, "Durable Helper")
        XCTAssertEqual(description, "Reviews durable changes.")
        XCTAssertEqual(tint, .purple)
        XCTAssertEqual(
            config.middleware.settings["example"]?["access"],
            .string("safe")
        )
        XCTAssertEqual(model.botMutationRequestID, requestID)
        XCTAssertEqual(model.botApplyState, .applying)
    }

    func testBotDraftSaveKeepsTheRevisionCapturedWhenEditingBegan() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let original = bot(config: VersionedAgentConfig(revision: 3, config: composition()))
        model.connectionState = .ready
        model.bots = [original]
        model.beginEditingBot(original)
        model.botNameDraft = "Locally edited"

        let external = bot(
            name: "Externally edited",
            config: VersionedAgentConfig(revision: 8, config: composition())
        )
        model.handle(.bots(requestID: nil, bots: [external]))
        model.saveBotDraft()

        let request = await recorder.firstRequest(after: 0) {
            if case .updateBot = $0 { return true }
            return false
        }
        guard case .updateBot(_, _, let expectedRevision, _, _, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected a Bot update") }
        XCTAssertEqual(expectedRevision, 3)
    }

    func testBotDraftCannotSaveWhileAnotherChatForTheBotIsRunning() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let helper = bot()
        model.connectionState = .ready
        model.bots = [helper]
        model.sessions = [
            session(sessionID: "chat-1", state: .idle, botID: helper.id),
            session(sessionID: "chat-2", state: .running, botID: helper.id),
        ]
        model.selectedSessionID = "chat-1"
        model.beginEditingBot(helper)
        model.botNameDraft = "Blocked edit"

        XCTAssertFalse(model.canMutateSelectedBot)
        model.saveBotDraft()
        let requestCount = await recorder.requestCount()
        XCTAssertEqual(requestCount, 0)
    }

    func testBotMutationRejectionsUseConfigurationApplyStates() throws {
        let cases: [(String, ApplyState)] = [
            ("revision_conflict", .conflict("Rejected")),
            ("agent_busy", .busy("Rejected")),
            ("invalid_config", .invalid("Rejected")),
        ]
        for (code, expected) in cases {
            let model = try model()
            model.botMutationRequestID = "bot-update"
            model.botApplyState = .applying

            model.handle(.rejected(GatewayRejection(
                requestId: "bot-update",
                code: code,
                message: "Rejected",
                fatal: false
            )))

            XCTAssertEqual(model.botApplyState, expected)
        }
    }

    func testComposerSettingUpdatesTheSelectedBotProfile() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let helper = bot(config: VersionedAgentConfig(revision: 7, config: composition()))
        model.connectionState = .ready
        model.bots = [helper]
        model.sessions = [session(state: .idle)]
        model.selectedSessionID = "chat-1"

        model.setSelectedBotSetting(
            .string("full_access"),
            middleware: "sandbox",
            setting: "approval_policy"
        )

        let request = await recorder.firstRequest(after: 0) {
            if case .updateBot = $0 { return true }
            return false
        }
        guard case .updateBot(
            _,
            let id,
            let expectedRevision,
            let name,
            let description,
            let tint,
            let config
        ) = try XCTUnwrap(request) else {
            return XCTFail("Expected a durable Bot update")
        }
        XCTAssertEqual(id, helper.id)
        XCTAssertEqual(expectedRevision, 7)
        XCTAssertEqual(name, helper.name)
        XCTAssertEqual(description, helper.description)
        XCTAssertEqual(tint, helper.tint)
        XCTAssertEqual(
            config.middleware.settings["sandbox"]?["approval_policy"],
            .string("full_access")
        )
    }

    func testBotCatalogResponseCompletesSaveAndRefreshesDraft() throws {
        let model = try model()
        let original = bot(config: VersionedAgentConfig(revision: 4, config: composition()))
        model.connectionState = .ready
        model.bots = [original]
        model.sessions = [session(state: .idle)]
        model.selectedSessionID = "chat-1"
        model.agentSnapshot = original.config
        model.agentDraft = original.config.config
        model.beginEditingBot(original)
        model.botMutationRequestID = "bot-update-1"
        model.botApplyState = .applying

        var savedConfig = composition(systemPrompt: "Durable Bot")
        savedConfig.middleware.setSetting(
            .string("allow"),
            middleware: "sandbox",
            setting: "approval_policy"
        )
        let saved = bot(
            name: "Durable Helper",
            description: "Reviews durable changes.",
            tint: .purple,
            config: VersionedAgentConfig(revision: 5, config: savedConfig)
        )
        model.handle(.bots(requestID: "bot-update-1", bots: [saved]))

        XCTAssertNil(model.botMutationRequestID)
        XCTAssertEqual(model.botApplyState, .applied)
        XCTAssertFalse(model.isApplyingConfiguration)
        XCTAssertEqual(model.botNameDraft, "Durable Helper")
        XCTAssertEqual(model.botDescriptionDraft, "Reviews durable changes.")
        XCTAssertEqual(model.botTintDraft, .purple)
        XCTAssertEqual(model.botDraft, savedConfig)
        XCTAssertEqual(model.agentSnapshot, saved.config)
        XCTAssertEqual(model.agentDraft, savedConfig)
    }

    func testSeededMobiusBotCannotBeDeleted() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let mobius = bot(handle: "mobius", name: "Mobius")
        model.connectionState = .ready
        model.bots = [mobius]

        model.deleteBot(mobius)
        try await Task.sleep(for: .milliseconds(20))

        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { request in
            if case .deleteBot = request { return true }
            return false
        })
    }

    func testDeletingBotDropsItsRoutineStateWhenTheCatalogConfirms() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let mobius = bot(handle: "mobius", name: "Mobius")
        let helper = bot(
            id: "bot-2",
            handle: "helper",
            name: "Helper",
            config: VersionedAgentConfig(revision: 7, config: composition())
        )
        let helperRoutine = Routine(
            id: "routine-1",
            botId: helper.id,
            workspace: "/srv/mobius",
            instructions: "Review nightly",
            schedule: .interval(seconds: 120),
            endsAt: nil,
            enabled: true,
            finished: false,
            nextRunAt: nil
        )
        model.connectionState = .ready
        model.bots = [mobius, helper]
        model.routines = [helperRoutine]
        model.routineRuns = [RoutineRun(
            id: "run-1",
            routineId: helperRoutine.id,
            botId: helper.id,
            startedAt: 100,
            finishedAt: 101,
            status: .succeeded,
            sessionId: nil,
            message: nil
        )]

        model.deleteBot(helper)
        let request = await recorder.firstRequest(after: 0) {
            if case .deleteBot = $0 { return true }
            return false
        }
        guard case .deleteBot(let requestID, let id, let revision) = try XCTUnwrap(request) else {
            return XCTFail("Expected Bot deletion")
        }
        XCTAssertEqual(id, helper.id)
        XCTAssertEqual(revision, 7)

        model.handle(.bots(requestID: requestID, bots: [mobius]))

        XCTAssertTrue(model.routines.isEmpty)
        XCTAssertTrue(model.routineRuns.isEmpty)
    }

    func testProviderRegistrationDoesNotConfigureBotDefaults() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        let draft = composition(systemPrompt: "New Bot defaults")
        let previousBotDefaults = composition(systemPrompt: "Previous Bot defaults")
        model.selectedSessionID = "chat-1"
        model.agentSnapshot = VersionedAgentConfig(revision: 3, config: composition())
        model.agentDraft = composition(systemPrompt: "Active chat")
        model.botDefaultsSnapshot = VersionedAgentConfig(
            revision: 8,
            config: previousBotDefaults
        )
        model.botDefaultsDraft = previousBotDefaults
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
        let response = ready(botDefaults: VersionedAgentConfig(
            revision: 8,
            config: previousBotDefaults
        ))
        model.applyGatewayConfigurationResponse(requestID: registration, payload: response)
        try await Task.sleep(for: .milliseconds(20))

        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains {
            if case .configureBotDefaults = $0 { return true }
            return false
        })
        XCTAssertEqual(model.botDefaultsDraft, previousBotDefaults)
        XCTAssertEqual(model.agentDraft?.systemPrompt, "Active chat")
    }

    func testSavingBotDefaultsLeavesActiveChatUntouched() async throws {
        let recorder = GatewayRequestRecorder()
        let botDefaultsSaved = expectation(description: "Bot defaults saved")
        let model = try model { request in
            await recorder.record(request)
            if case .configureBotDefaults = request { botDefaultsSaved.fulfill() }
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
        model.botDefaultsSnapshot = VersionedAgentConfig(revision: 7, config: active)
        model.agentDraft = active
        model.botDefaultsDraft = draft

        model.saveBotDefaults()
        await fulfillment(of: [botDefaultsSaved], timeout: 1)

        let botDefaultsRequests = await recorder.requests()
        let botDefaultsRequest = try XCTUnwrap(
            botDefaultsRequests.first { request in
                if case .configureBotDefaults = request { return true }
                return false
            }
        )
        guard case .configureBotDefaults(let requestID, _, let savedDraft) = botDefaultsRequest else {
            return XCTFail("Expected Bot defaults configuration")
        }
        XCTAssertEqual(savedDraft, draft)

        let response = ready(
            botDefaults: VersionedAgentConfig(revision: 8, config: draft),
            bots: [bot(config: VersionedAgentConfig(revision: 3, config: active))]
        )
        model.handle(.ready(response))
        XCTAssertEqual(model.botDefaultsDraft, draft)
        model.applyGatewayConfigurationResponse(requestID: requestID, payload: response)
        XCTAssertEqual(model.agentDraft, active)
        XCTAssertEqual(model.botDefaultsDraft, draft)
        XCTAssertEqual(model.botDefaultsApplyState, .applied)
    }

    func testSavingBotDefaultsPreservesALaterDraft() async throws {
        let recorder = GatewayRequestRecorder()
        let botDefaultsSaved = expectation(description: "Bot defaults saved")
        let model = try model { request in
            await recorder.record(request)
            if case .configureBotDefaults = request { botDefaultsSaved.fulfill() }
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
        model.botDefaultsSnapshot = VersionedAgentConfig(revision: 7, config: active)
        model.agentDraft = active
        model.botDefaultsDraft = draft

        model.saveBotDefaults()
        await fulfillment(of: [botDefaultsSaved], timeout: 1)
        let requests = await recorder.requests()
        let requestID = try XCTUnwrap(requests.lazy.compactMap { request -> String? in
            guard case .configureBotDefaults(let requestID, _, _) = request else { return nil }
            return requestID
        }.first)

        var laterBotDefaultsDraft = draft
        laterBotDefaultsDraft.middleware.setSetting(
            .string("second"),
            middleware: "example",
            setting: "mode"
        )
        model.botDefaultsDraft = laterBotDefaultsDraft
        let response = ready(
            botDefaults: VersionedAgentConfig(revision: 8, config: draft),
            bots: [bot(config: VersionedAgentConfig(revision: 3, config: active))]
        )
        model.handle(.ready(response))
        XCTAssertEqual(model.botDefaultsDraft, laterBotDefaultsDraft)
        model.applyGatewayConfigurationResponse(requestID: requestID, payload: response)
        XCTAssertEqual(model.agentDraft, active)
        XCTAssertEqual(model.botDefaultsDraft, laterBotDefaultsDraft)
        XCTAssertEqual(model.botDefaultsApplyState, .applied)
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
            botDefaults: VersionedAgentConfig(revision: 1, config: composition()),
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
        var unsavedBotDefaults = snapshot.config
        unsavedBotDefaults.extensions = ["plugin:ponytail"]
        var unsavedChat = snapshot.config
        unsavedChat.extensions = ["plugin:ponytail"]
        model.botDefaultsSnapshot = snapshot
        model.botDefaultsDraft = unsavedBotDefaults
        model.agentDraft = unsavedChat

        model.applyGatewayCatalog(ready(botDefaults: snapshot))

        XCTAssertEqual(model.botDefaultsDraft?.extensions, ["plugin:ponytail"])
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
