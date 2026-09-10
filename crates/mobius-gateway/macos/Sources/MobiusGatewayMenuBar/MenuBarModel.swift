import Foundation
import Observation

@MainActor
@Observable
final class MenuBarModel {
    private(set) var chats: [VoiceChat] = []
    private(set) var bots: [VoiceBot] = []
    private(set) var models: [VoiceModel] = []
    private(set) var selectedChatID: String?
    private(set) var isReady = false
    private(set) var isConnecting = false
    private(set) var chatIsReady = false
    private(set) var route: String?
    private(set) var voiceCall: VoiceCall?
    private(set) var approval: VoiceApproval?
    private(set) var approvalSubmissionID: String?
    var message: String?
    private(set) var draftBotID: String?
    private(set) var draftWorkspace: String?
    private(set) var voice = RealtimeVoiceSession()
    let desktop = DesktopRuntime()

    @ObservationIgnored private let client = GatewayClient()
    @ObservationIgnored private let defaults: UserDefaults
    @ObservationIgnored private var connectionTask: Task<Void, Never>?
    @ObservationIgnored private var voiceTask: Task<Void, Never>?
    @ObservationIgnored private var outgoing: Task<Void, Never>?
    @ObservationIgnored private var generation = UUID()
    private var opening: OpeningChat?

    struct VoiceCall: Equatable {
        let requestID: String
        let sessionID: String
        var voiceID: String?
    }

    private struct OpeningChat {
        let requestID: String
        var sessionID: String?
        let startVoice: Bool
    }

    init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    var selectedChat: VoiceChat? { chats.first { $0.id == selectedChatID } }
    var selectedBot: VoiceBot? {
        bots.first { $0.id == (selectedChat?.sessionContext.botId ?? draftBotID) }
    }
    var workspacePath: String {
        selectedChat?.sessionContext.workspaceLabel ?? draftWorkspace ?? "."
    }
    var workspaceName: String {
        workspacePath == "."
            ? "Default folder" : URL(fileURLWithPath: workspacePath).lastPathComponent
    }
    var workspacePaths: [String] {
        Set(chats.compactMap(\.sessionContext.workspaceLabel) + [workspacePath])
            .sorted { $0.localizedStandardCompare($1) == .orderedAscending }
    }
    var chatGroups: [(workspace: String, chats: [VoiceChat])] {
        Dictionary(
            grouping: chats.filter { $0.sessionContext.botId == selectedBot?.id }, by: \.workspace
        )
        .map { (workspace: $0.key, chats: $0.value) }
        .sorted { $0.workspace.localizedStandardCompare($1.workspace) == .orderedAscending }
    }
    var supportsVoice: Bool { models.first { $0.route == route }?.supportsRealtimeVoice == true }
    var canChooseChat: Bool { isReady && opening == nil }
    var canStartVoice: Bool {
        canChooseChat && voiceCall == nil
            && (selectedChatID == nil ? selectedBot != nil : chatIsReady && supportsVoice)
    }
    var status: String {
        if voiceCall != nil {
            if !voice.isConnected { return "Connecting voice" }
            return voice.isMuted ? "Microphone muted" : "Listening"
        }
        if isConnecting { return "Connecting gateway" }
        if !isReady { return "Gateway offline" }
        if opening != nil { return "Opening chat" }
        if selectedChatID == nil { return selectedBot == nil ? "Choose Bot" : "Start voice" }
        if !chatIsReady { return "Opening chat" }
        return supportsVoice ? "Start voice" : "Voice unavailable for this model"
    }

    func connect(
        loadConnection: @escaping @Sendable () async throws -> LocalGatewayConnection = {
            try await GatewayInstallation().connection()
        }
    ) {
        let previous = connectionTask
        previous?.cancel()
        resetConnection()
        isConnecting = true
        let generation = generation
        connectionTask = Task { [weak self] in
            guard let self else { return }
            await self.client.disconnect()
            await previous?.value
            guard self.generation == generation else { return }
            do {
                let connection = try await loadConnection()
                try Task.checkCancellation()
                guard self.generation == generation else { return }
                let events = try await self.client.connect(to: connection.endpoint)
                try await self.client.send(
                    GatewayRequest(
                        "authenticate",
                        [
                            "token": .string(connection.token), "clientKind": .string("macos"),
                        ]))
                for try await event in events {
                    guard !Task.isCancelled, self.generation == generation else { return }
                    try self.receive(event)
                }
                throw GatewayWireError.disconnected
            } catch {
                guard self.generation == generation else { return }
                self.resetConnection()
                self.message = error.localizedDescription
                await self.client.disconnect()
            }
        }
    }

    func openChat(_ chat: VoiceChat) {
        guard isReady else { return }
        if selectedChatID == chat.id, chatIsReady { return }
        let startVoice = voiceCall != nil
        stopVoice()
        draftBotID = nil
        draftWorkspace = nil
        selectedChatID = chat.id
        defaults.set(chat.id, forKey: "voiceChatID")
        prepareChat(requestID: UUID().uuidString, sessionID: chat.id, startVoice: startVoice)
        guard let opening else { return }
        send(
            GatewayRequest(
                "open_session",
                [
                    "requestId": .string(opening.requestID), "sessionId": .string(chat.id),
                    "lastSequence": .null,
                ]))
    }

    func beginNewChat(botID: String? = nil, workspace: String? = nil) {
        guard canChooseChat else { return }
        let botID = botID ?? selectedBot?.id ?? bots.first?.id
        guard botID == nil || bots.contains(where: { $0.id == botID }) else { return }
        let workspace = workspace ?? workspacePath
        stopVoice()
        draftBotID = botID
        draftWorkspace = workspace
        selectedChatID = nil
        chatIsReady = false
        route = nil
        approval = nil
        approvalSubmissionID = nil
        message = nil
    }

    private func createChat(workspace: String, botID: String) {
        guard isReady else { return }
        stopVoice()
        selectedChatID = nil
        prepareChat(requestID: UUID().uuidString, sessionID: nil, startVoice: true)
        guard let opening else { return }
        send(
            GatewayRequest(
                "create_session",
                [
                    "requestId": .string(opening.requestID), "workspace": .string(workspace),
                    "botId": .string(botID),
                ]))
    }

    func refreshChats() {
        guard isReady else { return }
        send(GatewayRequest("list_sessions", ["requestId": .string(UUID().uuidString)]))
    }

    func startVoice() {
        guard canStartVoice else { return }
        guard let sessionID = selectedChatID else {
            if let bot = selectedBot { createChat(workspace: workspacePath, botID: bot.id) }
            return
        }
        message = nil
        let call = VoiceCall(requestID: UUID().uuidString, sessionID: sessionID)
        voiceCall = call
        voice = RealtimeVoiceSession { [weak self] message in
            guard self?.voiceCall?.requestID == call.requestID else { return }
            self?.stopVoice()
            self?.message = message
        }
        let voice = voice
        voiceTask = Task { [weak self] in
            do {
                let offer = try await voice.offer()
                guard let self, self.voiceCall?.requestID == call.requestID else { return }
                self.send(
                    GatewayRequest(
                        "start_realtime_voice",
                        [
                            "requestId": .string(call.requestID), "sessionId": .string(sessionID),
                            "offerSdp": .string(offer),
                        ]))
                try await Task.sleep(for: .seconds(45))
                guard self.voiceCall?.requestID == call.requestID else { return }
                self.stopVoice()
                self.message = "Voice could not connect. Try again."
            } catch is CancellationError {
            } catch {
                guard self?.voiceCall?.requestID == call.requestID else { return }
                self?.stopVoice()
                self?.message = error.localizedDescription
            }
        }
    }

    func toggleVoice() {
        if voiceCall == nil { startVoice() } else { stopVoice() }
    }

    func toggleMicrophone() {
        guard voiceCall != nil else { return }
        voice.isMuted.toggle()
    }

    func stopVoice(notifyGateway: Bool = true) {
        let call = voiceCall
        voiceCall = nil
        voiceTask?.cancel()
        voiceTask = nil
        voice.close()
        if let call, notifyGateway {
            endCall(sessionID: call.sessionID, voiceID: call.voiceID ?? call.requestID)
        }
    }

    func resolveApproval(approve: Bool) {
        guard let approval, let selectedChatID, approvalSubmissionID == nil, chatIsReady else {
            return
        }
        let id = UUID().uuidString
        approvalSubmissionID = id
        let decision: JSONValue =
            approve
            ? .string("approved")
            : .object([
                "denied": .object(["rejection": .string("Declined from the voice menu.")])
            ])
        send(
            GatewayRequest(
                "submit",
                [
                    "sessionId": .string(selectedChatID),
                    "submission": .object([
                        "id": .string(id),
                        "op": .object([
                            "type": .string("exec_approval"), "id": .string(approval.id),
                            "decision": decision,
                        ]),
                    ]),
                ]))
    }

    func shutdown() async {
        stopVoice()
        await outgoing?.value
        connectionTask?.cancel()
        resetConnection()
        await client.disconnect()
    }

    private func prepareChat(requestID: String, sessionID: String?, startVoice: Bool) {
        opening = OpeningChat(requestID: requestID, sessionID: sessionID, startVoice: startVoice)
        chatIsReady = false
        route = nil
        approval = nil
        approvalSubmissionID = nil
        message = nil
    }

    private func resetConnection() {
        generation = UUID()
        desktop.disconnected()
        stopVoice(notifyGateway: false)
        outgoing?.cancel()
        outgoing = nil
        opening = nil
        isReady = false
        isConnecting = false
        chatIsReady = false
        approval = nil
        approvalSubmissionID = nil
    }

    private func send(_ request: GatewayRequest) {
        let previous = outgoing
        let generation = generation
        outgoing = Task { [weak self] in
            await previous?.value
            guard let self, !Task.isCancelled, self.generation == generation else { return }
            do { try await self.client.send(request) } catch {
                guard self.generation == generation else { return }
                self.resetConnection()
                self.message = error.localizedDescription
                await self.client.disconnect()
            }
        }
    }

    private func endCall(sessionID: String, voiceID: String) {
        send(
            GatewayRequest(
                "end_realtime_voice",
                [
                    "sessionId": .string(sessionID), "voiceId": .string(voiceID),
                ]))
    }
}

extension MenuBarModel {
    func receive(_ envelope: GatewayEnvelope) throws {
        let body = envelope.body
        try desktop.receive(envelope)
        switch envelope.type {
        case "ready":
            guard let payload = body["payload"] else {
                throw GatewayWireError.invalidFrame("Missing gateway catalog.")
            }
            let catalog = try payload.decode(VoiceCatalog.self)
            bots = catalog.bots
            models = catalog.models
            updateChats(catalog.sessions)
            isReady = true
            isConnecting = false
            desktop.connected { [weak self] request in self?.send(request) }
            if let chat = chats.first(where: { $0.id == defaults.string(forKey: "voiceChatID") })
                ?? chats.first
            {
                openChat(chat)
            } else {
                beginNewChat()
            }
        case "sessions":
            if let sessions = body["sessions"] {
                updateChats(try sessions.decode([VoiceChat].self))
            }
        case "bots":
            if let items = body["bots"] { bots = try items.decode([VoiceBot].self) }
        case "session_opened", "session_changed", "session_replay_complete":
            try receiveSession(envelope)
        case "realtime_voice_started", "realtime_voice_failed", "realtime_voice_ended":
            try receiveVoice(envelope)
        case "agent_event":
            if body["sessionId"]?.stringValue == selectedChatID,
                let event = body["record"]?["event"]?["msg"]
            {
                try receiveAgent(event)
            }
        case "accepted":
            if let approvalSubmissionID,
                try body.requiredString("requestId") == approvalSubmissionID
            {
                approval = nil
                self.approvalSubmissionID = nil
            }
        case "rejected":
            if body["requestId"]?.stringValue == opening?.requestID {
                opening = nil; chatIsReady = false
            }
            if body["requestId"]?.stringValue == approvalSubmissionID { approvalSubmissionID = nil }
            message = try body.requiredString("message")
        case "error":
            let message = try body.requiredString("message")
            if body["fatal"]?.boolValue == true { throw GatewayWireError.invalidFrame(message) }
            self.message = message
        default: break
        }
    }

    private func updateChats(_ chats: [VoiceChat]) {
        self.chats = chats.filter { $0.parentSessionId == nil }.sorted {
            $0.updatedAt > $1.updatedAt
        }
        if let selectedChatID, !self.chats.contains(where: { $0.id == selectedChatID }),
            opening == nil
        {
            stopVoice()
            self.selectedChatID = nil
            chatIsReady = false
            approval = nil
        }
        if let approval, let chat = selectedChat, chat.activity.approvalRequestId != approval.id {
            self.approval = nil
            approvalSubmissionID = nil
        }
    }

    private func receiveSession(_ envelope: GatewayEnvelope) throws {
        let body = envelope.body
        if envelope.type == "session_replay_complete" {
            guard let opening, body["requestId"]?.stringValue == opening.requestID,
                body["sessionId"]?.stringValue == selectedChatID
            else { return }
            let resume = opening.startVoice
            self.opening = nil
            chatIsReady = true
            if resume { startVoice() }
            return
        }
        guard let payload = body["payload"], let session = payload["session"] else {
            throw GatewayWireError.invalidFrame("Missing chat state.")
        }
        let id = try session.requiredString("sessionId")
        if envelope.type == "session_opened" {
            guard let opening, body["requestId"]?.stringValue == opening.requestID,
                opening.sessionID == nil || opening.sessionID == id
            else { return }
            selectedChatID = id
            self.opening?.sessionID = id
            defaults.set(id, forKey: "voiceChatID")
            refreshChats()
        } else if id != selectedChatID {
            return
        }
        let newRoute = session["model"]?["route"]?.stringValue
        if route != nil, newRoute != route { stopVoice() }
        route = newRoute
    }

    private func receiveVoice(_ envelope: GatewayEnvelope) throws {
        let body = envelope.body
        let sessionID = try body.requiredString("sessionId")
        switch envelope.type {
        case "realtime_voice_started":
            let requestID = try body.requiredString("requestId")
            let voiceID = try body.requiredString("voiceId")
            guard voiceCall?.requestID == requestID, voiceCall?.sessionID == sessionID else {
                endCall(sessionID: sessionID, voiceID: voiceID)
                return
            }
            let answer = try body.requiredString("answerSdp")
            voiceCall?.voiceID = voiceID
            voiceTask?.cancel()
            let voice = voice
            voiceTask = Task { [weak self] in
                do { try await voice.accept(answer: answer) } catch is CancellationError {
                } catch {
                    guard self?.voiceCall?.requestID == requestID else { return }
                    self?.stopVoice()
                    self?.message = error.localizedDescription
                }
            }
        case "realtime_voice_failed":
            guard voiceCall?.requestID == body["requestId"]?.stringValue,
                voiceCall?.sessionID == sessionID
            else { return }
            stopVoice(notifyGateway: false)
            message = try body.requiredString("message")
        default:
            guard voiceCall?.voiceID == body["voiceId"]?.stringValue,
                voiceCall?.sessionID == sessionID
            else { return }
            stopVoice(notifyGateway: false)
            message = body["reason"]?.stringValue
        }
    }

    private func receiveAgent(_ event: JSONValue) throws {
        switch event["type"]?.stringValue {
        case "exec_approval_request":
            approval = try VoiceApproval(event)
            approvalSubmissionID = nil
        case "tool_call_begin", "turn_complete", "turn_aborted":
            approval = nil
            approvalSubmissionID = nil
        case "model_changed":
            route = try event.requiredString("route")
            stopVoice()
        default: break
        }
    }
}
