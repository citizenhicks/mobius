import Foundation

extension AppModel {
    func requestID(_ prefix: String) -> String {
        "\(prefix)-\(UUID().uuidString.lowercased())"
    }

    func cacheChatCatalog(lastSessionID: String? = nil) {
        guard !isClearingLocalData, let accountID = gateway.selectedAccountID else { return }
        let catalog = CachedChatCatalog(
            bots: bots,
            sessions: chat.sessions,
            lastSessionID: lastSessionID ?? chat.selectedSessionID
        )
        chat.enqueueTranscriptIO { [store] in
            await store.saveChatCatalog(catalog, accountID: accountID)
        }
    }

    func clearCachedData() async {
        guard !isClearingLocalData else { return }
        isClearingLocalData = true
        chat.isClearingLocalData = true
        defer {
            isClearingLocalData = false
            chat.isClearingLocalData = false
        }
        chat.discardFileThumbnails()
        let previous = chat.transcriptIOTask
        await previous?.value
        do {
            try await store.clearCachedData()
            showToast("Cached data cleared.", tone: .success)
        } catch {
            showToast(verbatim: localizedErrorDescription(error), tone: .error)
        }
    }
}
