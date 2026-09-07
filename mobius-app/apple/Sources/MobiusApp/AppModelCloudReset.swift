import Foundation

extension AppModel {
    func clearDataAndGatewayInformation() async {
        guard !isClearingLocalData else { return }
        isClearingLocalData = true
        chat.isClearingLocalData = true
        defer {
            isClearingLocalData = false
            chat.isClearingLocalData = false
        }

        cloud.invalidateInFlightOperations()
        gateway.blockAutomaticReconnect()
        chat.quiesce()
        gateway.reset(preservingDrafts: false)
        resetGatewayDependentState(preservingDrafts: false, preservingSession: false)
        let shutdown = gateway.shutdown()
        await chat.drainIO()
        await shutdown.value

        _ = try? await cloud.unregisterRemoteNotificationsForCloudSignOut()
        var localError = cloud.clearLocalCloudState()
        do {
            try await store.clearAllData()
        } catch {
            localError = localError ?? error
        }
        if let localError {
            gateway.reloadAccounts()
            restoreSessionReadState(for: gateway.selectedAccountID)
            showsPairing = gateway.accounts.isEmpty
            let message = "\(localizedString("Local data could not be fully cleared.")) \(localizedErrorDescription(localError))"
            showToast(verbatim: message, tone: .error)
            return
        }

        gateway.clearAccounts()
        restoreSessionReadState(for: gateway.selectedAccountID)
        gateway.pairingEndpoint = "wss://"
        gateway.pairingCode = ""
        gateway.pairingError = nil
        destination = .chats
        navigationPath = []
        showsPairing = true
        showToast("Local data and gateway information cleared.", tone: .success)
    }
}
