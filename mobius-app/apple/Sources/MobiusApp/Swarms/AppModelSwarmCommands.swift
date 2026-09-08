import Foundation
import Observation

extension AppModel {
    func openSwarm(_ swarmID: String) {
        guard swarms.contains(where: { $0.id == swarmID }) else { return }
        destination = .bots
        navigationPath = [.swarm(swarmID)]
    }

    func openSwarmChat(_ swarmID: String) {
        guard swarms.contains(where: { $0.id == swarmID }) else { return }
        destination = .bots
        navigationPath = [.swarm(swarmID), .swarmChat(swarmID)]
    }

    func swarm(containingBot botID: String) -> SwarmRecord? {
        swarms.first { swarm in
            swarm.leaderBotId == botID
                || swarm.members.contains { $0.botId == botID }
        }
    }

    func availableBotsForSwarm(excluding botID: String? = nil) -> [BotRecord] {
        bots.filter { bot in
            bot.collaborationEnabled && bot.id != botID && swarm(containingBot: bot.id) == nil
        }.sorted {
            $0.name.localizedStandardCompare($1.name) == .orderedAscending
        }
    }

    func beginCreatingSwarm() -> Bool {
        guard canMutateSwarm else { return false }
        guard bots.contains(where: \.collaborationEnabled) else {
            showToast("Swarm is off")
            return false
        }
        return availableBotsForSwarm().count >= 2
    }

    func createSwarm(title rawTitle: String, leaderBotID: String, memberBotIDs: Set<String>) {
        let title = rawTitle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canMutateSwarm, !title.isEmpty else { return }
        let allowed = Set(availableBotsForSwarm().map(\.id))
        guard allowed.contains(leaderBotID),
            !memberBotIDs.isEmpty,
            !memberBotIDs.contains(leaderBotID),
            memberBotIDs.isSubset(of: allowed)
        else {
            showToast("Choose available Bots with Swarm collaboration enabled.", tone: .warning)
            return
        }
        let selectedCoworkers = bots.compactMap { bot in
            memberBotIDs.contains(bot.id) ? bot.id : nil
        }
        sendSwarmMutation("swarm-create") { requestID in
            .createSwarm(
                requestID: requestID,
                title: title,
                leaderBotID: leaderBotID,
                memberBotIDs: selectedCoworkers
            )
        }
    }

    func addSwarmMember(_ bot: BotRecord, to swarm: SwarmRecord) {
        guard availableBotsForSwarm().contains(where: { $0.id == bot.id }) else {
            showToast("Choose an available Bot with Swarm collaboration enabled.", tone: .warning)
            return
        }
        sendSwarmMutation("swarm-add") { requestID in
            .addSwarmMember(
                requestID: requestID,
                swarmID: swarm.id,
                botID: bot.id
            )
        }
    }

    func leaveSwarm(_ swarm: SwarmRecord, botID: String) {
        guard swarm.leaderBotId != botID,
            self.swarm(containingBot: botID)?.id == swarm.id
        else { return }
        sendSwarmMutation("swarm-leave") { requestID in
            .leaveSwarm(requestID: requestID, swarmID: swarm.id, botID: botID)
        }
    }

    func renameSwarm(_ swarm: SwarmRecord, title rawTitle: String) {
        let title = rawTitle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty, title != swarm.title else { return }
        sendSwarmMutation("swarm-rename") { requestID in
            .renameSwarm(requestID: requestID, swarmID: swarm.id, title: title)
        }
    }

    func disbandSwarm(_ swarm: SwarmRecord) {
        sendSwarmMutation("swarm-disband") { requestID in
            .disbandSwarm(requestID: requestID, swarmID: swarm.id)
        }
    }

    @discardableResult
    func postSwarmMessage(
        to swarmID: String,
        text rawText: String
    ) -> String? {
        let text = rawText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canPostSwarmMessage,
            !text.isEmpty,
            swarms.contains(where: { $0.id == swarmID })
        else { return nil }
        let id = requestID("swarm-message")
        swarmMessageRequestID = id
        gateway.transmit(
            .postSwarmMessage(
                requestID: id,
                swarmID: swarmID,
                text: text
            )
        ) { [weak self] _ in
            if self?.swarmMessageRequestID == id { self?.swarmMessageRequestID = nil }
        }
        return id
    }

    private func sendSwarmMutation(
        _ requestPrefix: String,
        request: (String) -> GatewayRequest
    ) {
        guard canMutateSwarm else { return }
        let id = requestID(requestPrefix)
        swarmMutationRequestID = id
        swarmApplyState = .applying
        gateway.transmit(request(id)) { [weak self] message in
            guard self?.swarmMutationRequestID == id else { return }
            self?.swarmMutationRequestID = nil
            self?.swarmApplyState = .failed(message)
        }
    }
}
