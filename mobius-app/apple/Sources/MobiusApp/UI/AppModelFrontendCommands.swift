import Foundation
import Observation

extension AppModel {
    var providerUsage: [ProviderUsage] {
        (profile?.providerUsage ?? []).filter { usage in
            providerInstances.contains { $0.configured && $0.provider == usage.provider }
        }
    }

    var codexWeeklyUsage: UsageLimit? {
        providerUsage.lazy.compactMap { usage in
            usage.limits?.first { $0.id.hasPrefix("codex:") && $0.windowSeconds == 604_800 }
        }.first
    }

    var isLoadingCodexWeeklyUsage: Bool {
        profileRequestID != nil && codexWeeklyUsage == nil
    }

    func providerUsageError(for provider: String) -> String? {
        providerUsage.first { $0.provider == provider }?.error
    }

    func refreshProfile() {
        guard gateway.connectionState.isReady else { return }
        let id = requestID("profile")
        profileRequestID = id
        gateway.transmit(.getProfile(requestID: id)) { [weak self] _ in
            guard let self, self.profileRequestID == id else { return }
            self.invalidateProviderUsage()
        }
    }

    func refreshProfileWhileVisible() async {
        while !Task.isCancelled, gateway.connectionState.isReady {
            refreshProfile()
            do {
                try await Task.sleep(for: .seconds(60))
            } catch {
                return
            }
        }
    }

    func invalidateProviderUsage() {
        profileRequestID = nil
        let unavailable = (profile?.providerUsage ?? []).map {
            ProviderUsage(provider: $0.provider, limits: nil, error: nil)
        }
        profile?.providerUsage = unavailable
    }

    func submitWidget(_ mounted: MountedWidget) {
        guard canSubmitFrontendAction(capability: mounted.capability),
            let sessionID = chat.selectedSessionID,
            let action = mounted.widget.action
        else { return }
        let id = requestID("widget")
        chat.previewWidgetRequestID = id
        gateway.transmit(.submit(sessionID: sessionID, submission: Submission(id: id, op: action)))
        { [weak self] _ in
            if self?.chat.previewWidgetRequestID == id { self?.chat.previewWidgetRequestID = nil }
        }
    }

    func submitMessageAction(_ mounted: MountedWidget, target: MessageTarget) {
        guard canSubmitFrontendAction(capability: mounted.capability),
            let sessionID = chat.selectedSessionID,
            let action = mounted.widget.action
        else { return }
        let submittedAction =
            switch action {
            case .capabilityCommand(let capability, let command, let arguments, let input, _):
                AgentOperation.capabilityCommand(
                    capability: capability,
                    command: command,
                    arguments: arguments,
                    input: input,
                    target: target
                )
            default:
                action
            }
        gateway.transmit(
            .submit(
                sessionID: sessionID,
                submission: Submission(id: requestID("widget"), op: submittedAction)
            ))
    }

    func submitFrontendOperation(_ operation: AgentOperation) {
        let capability: String? =
            if case .capabilityCommand(let capability, _, _, _, _) = operation {
                capability
            } else {
                nil
            }
        guard canSubmitFrontendAction(capability: capability),
            let sessionID = chat.selectedSessionID
        else { return }
        gateway.transmit(
            .submit(
                sessionID: sessionID,
                submission: Submission(id: requestID("widget-action"), op: operation)
            ))
    }

    func submitContributionOperation(_ operation: AgentOperation, scope: ContributionScope) {
        guard gateway.connectionState.isReady else { return }
        if case .swarm(let id) = scope,
            !swarms.contains(where: { $0.id == id })
        {
            return
        }
        gateway.transmit(
            .submitContribution(
                requestID: requestID("contribution"),
                scope: scope,
                operation: operation
            ))
    }

    func refreshContributions(scope: ContributionScope) {
        guard gateway.connectionState.isReady else { return }
        if case .swarm(let id) = scope, !swarms.contains(where: { $0.id == id }) { return }
        gateway.transmit(.getContributions(requestID: requestID("contributions"), scope: scope))
    }

    func loadPreviewPage(_ operation: AgentOperation) {
        let capability: String? =
            if case .capabilityCommand(let capability, _, _, _, _) = operation {
                capability
            } else {
                nil
            }
        guard canSubmitFrontendAction(capability: capability),
            let sessionID = chat.selectedSessionID,
            !chat.isLoadingPreviewPage
        else { return }
        let id = requestID("preview-page")
        chat.previewPageRequestID = id
        chat.isLoadingPreviewPage = true
        gateway.transmit(
            .submit(
                sessionID: sessionID,
                submission: Submission(id: id, op: operation)
            )
        ) { [weak self] _ in
            guard self?.chat.previewPageRequestID == id else { return }
            self?.chat.previewPageRequestID = nil
            self?.chat.isLoadingPreviewPage = false
        }
    }

    func loadPreviewPageAndWait(_ operation: AgentOperation) async {
        guard !Task.isCancelled, chat.previewPageRequestID == nil else { return }
        loadPreviewPage(operation)
        guard chat.previewPageRequestID != nil else { return }
        for await loading in Observations({ self.chat.isLoadingPreviewPage }) {
            if !loading { return }
        }
    }

    func submitPickerOption(_ option: FrontendPickerOption) {
        let capability: String? =
            if case .capabilityCommand(let capability, _, _, _, _) = option.op {
                capability
            } else {
                nil
            }
        guard canSubmitFrontendAction(capability: capability),
            let sessionID = chat.selectedSessionID
        else { return }
        let id = requestID("picker")
        chat.pendingPicker = nil
        if case .capabilityCommand = option.op { chat.previewSelections[id] = option }
        gateway.transmit(
            .submit(
                sessionID: sessionID,
                submission: Submission(id: id, op: option.op)
            )
        ) { [weak self] _ in
            self?.chat.previewSelections.removeValue(forKey: id)
        }
    }

    private func canSubmitFrontendAction(capability: String?) -> Bool {
        guard gateway.connectionState.isReady,
            chat.sessionRequestID == nil,
            chat.selectedSessionID != nil
        else { return false }
        guard let capability,
            middlewareFeatures.contains(where: { $0.id == capability })
        else { return true }
        return isCapabilityEnabled(capability)
    }
}
