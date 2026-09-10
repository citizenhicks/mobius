import Foundation
import Observation

extension AppModel {
    func saveBotDefaults() {
        guard !isApplyingConfiguration,
            let draft = botDefaultsDraft,
            let snapshot = botDefaultsSnapshot
        else { return }
        let id = requestID("configure-default")
        botDefaultsApplyState = .applying
        botDefaultsRequestID = id
        submittedBotDefaultsDraft = draft
        gateway.transmit(
            .configureBotDefaults(
                requestID: id,
                expectedRevision: snapshot.revision,
                config: draft
            )
        ) { [weak self] message in
            guard self?.botDefaultsRequestID == id else { return }
            self?.botDefaultsRequestID = nil
            self?.submittedBotDefaultsDraft = nil
            self?.botDefaultsApplyState = .failed(message)
        }
    }

    func beginEditingBot(_ bot: BotRecord) {
        editingBotID = bot.id
        editingBotRevision = bot.config.revision
        botNameDraft = bot.name
        botDescriptionDraft = bot.description
        botTintDraft = bot.tint
        botDraft = bot.config.config
        botApplyState = .idle
    }

    func selectModelForSelectedBot(_ route: String) {
        guard let bot = selectedBot,
            let config = draft(bot.config.config, selectingModelRoute: route),
            config != bot.config.config
        else { return }
        saveSelectedBot(bot, config: config)
    }

    func setSelectedBotSetting(
        _ value: FrontendSettingValue?,
        middleware: String,
        setting: String
    ) {
        guard let bot = selectedBot else { return }
        var config = bot.config.config
        guard config.middleware.settings[middleware]?[setting] != value else { return }
        config.middleware.setSetting(value, middleware: middleware, setting: setting)
        config.middleware.reconcile(features: middlewareFeatures)
        saveSelectedBot(bot, config: config)
    }

    func setSelectedBotVoice(_ voice: String) {
        guard let bot = selectedBot,
            realtimeVoices(for: bot.config.config).contains(voice),
            bot.config.config.realtimeVoice != voice
        else { return }
        var config = bot.config.config
        config.realtimeVoice = voice
        saveSelectedBot(bot, config: config)
    }

    private func saveSelectedBot(_ bot: BotRecord, config: AgentComposition) {
        guard canMutateBot(bot.id) else { return }
        beginEditingBot(bot)
        botDraft = config
        saveBotDraft()
    }

    func createBot(name rawName: String, description rawDescription: String) {
        let name = rawName.trimmingCharacters(in: .whitespacesAndNewlines)
        let description = rawDescription.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canMutateBots, !name.isEmpty, !description.isEmpty else { return }
        let id = requestID("bot-create")
        botApplyState = .applying
        botMutationRequestID = id
        botMutationSuccessMessage = localizedString("Bot created.")
        gateway.transmit(
            .createBot(
                requestID: id,
                name: name,
                description: description
            )
        ) {
            [weak self] message in
            guard self?.botMutationRequestID == id else { return }
            self?.botMutationRequestID = nil
            self?.botMutationSuccessMessage = nil
            self?.botApplyState = .failed(message)
        }
    }

    func saveBotDraft() {
        let name = botNameDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        let description = botDescriptionDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let id = editingBotID else { return }
        guard canMutateBot(id) else {
            if canMutateBots {
                showToast(
                    "Bot settings can’t be changed while this Bot is running.", tone: .warning)
            }
            return
        }
        guard let expectedRevision = editingBotRevision,
            bots.contains(where: { $0.id == id }),
            let draft = botDraft,
            !name.isEmpty,
            !description.isEmpty
        else { return }
        let requestID = requestID("bot-update")
        botMutationRequestID = requestID
        botMutationSuccessMessage = localizedString("Bot saved.")
        botApplyState = .applying
        gateway.transmit(
            .updateBot(
                requestID: requestID,
                id: id,
                expectedRevision: expectedRevision,
                name: name,
                description: description,
                tint: botTintDraft,
                config: draft
            )
        ) { [weak self] message in
            guard self?.botMutationRequestID == requestID else { return }
            self?.botMutationRequestID = nil
            self?.botMutationSuccessMessage = nil
            self?.botApplyState = .failed(message)
        }
    }

    func deleteBot(_ bot: BotRecord) {
        guard canMutateBots, bot.handle != "mobius" else { return }
        let id = requestID("bot-delete")
        botMutationRequestID = id
        botMutationSuccessMessage = localizedString("Bot deleted.")
        gateway.transmit(
            .deleteBot(
                requestID: id,
                id: bot.id,
                expectedRevision: bot.config.revision
            )
        ) { [weak self] message in
            guard self?.botMutationRequestID == id else { return }
            self?.botMutationRequestID = nil
            self?.botMutationSuccessMessage = nil
            self?.botApplyState = .failed(message)
        }
    }

    func reloadBotDraft() {
        guard let editingBotID,
            let bot = bots.first(where: { $0.id == editingBotID })
        else { return }
        botNameDraft = bot.name
        botDescriptionDraft = bot.description
        botTintDraft = bot.tint
        editingBotRevision = bot.config.revision
        botDraft = bot.config.config
        botApplyState = .idle
        showToast("Bot draft reloaded.", tone: .info)
    }

    func refreshBots() {
        guard gateway.connectionState.isReady else { return }
        gateway.transmit(.listBots(requestID: requestID("bot-list")))
    }

    func reloadBotDefaultsDraft() {
        botDefaultsDraft = botDefaultsSnapshot?.config
        botDefaultsApplyState = .idle
        showToast("Bot defaults draft reloaded.", tone: .info)
    }
}
