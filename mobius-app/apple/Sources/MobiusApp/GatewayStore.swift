import Foundation
import Security

struct CachedTranscript: Codable, Sendable {
    static let currentSchemaVersion = 2

    private struct HistoryState: Codable, Sendable {
        let nextBeforeSequence: UInt64?
    }

    private struct Entry: Codable, Sendable {
        let id: String
        let presentationID: String?
        let text: String
        let kind: TranscriptEntry.Kind
        let capability: String?
        let role: FrontendBlockRole?
        let title: String
        let symbol: String?
        let group: String?
        let format: String
        let tone: String
        let pending: Bool
        let modelStepID: String?
        let turnID: String?
        let startsTurn: Bool?
        let turnTerminal: Bool?
        let turnElapsedMs: UInt64?
        let sourceSequence: UInt64?
        let recordedAtMs: Int64?
        let messageTarget: MessageTarget?
        let files: [SessionFileReference]

        init(_ entry: TranscriptEntry) {
            id = entry.id
            presentationID = entry.presentationID
            text = entry.text
            kind = entry.kind
            capability = entry.capability
            role = entry.role
            title = entry.title
            symbol = entry.symbol
            group = entry.group
            format = entry.format
            tone = entry.tone
            pending = entry.pending
            modelStepID = entry.modelStepID
            turnID = entry.turnID
            startsTurn = entry.startsTurn
            turnTerminal = entry.turnTerminal
            turnElapsedMs = entry.turnElapsedMs
            sourceSequence = entry.sourceSequence
            recordedAtMs = entry.recordedAtMs
            messageTarget = entry.messageTarget
            files = entry.files
        }

        var transcriptEntry: TranscriptEntry {
            TranscriptEntry(
                id: id,
                presentationID: presentationID,
                text: text,
                kind: kind,
                capability: capability,
                role: role,
                title: title,
                symbol: symbol,
                group: group,
                format: format,
                tone: tone,
                pending: pending,
                modelStepID: modelStepID,
                turnID: turnID,
                startsTurn: startsTurn ?? false,
                turnTerminal: turnTerminal ?? false,
                turnElapsedMs: turnElapsedMs,
                sourceSequence: sourceSequence,
                recordedAtMs: recordedAtMs,
                messageTarget: messageTarget,
                files: files
            )
        }
    }

    let schemaVersion: Int
    let sequence: UInt64
    let currentUsage: TokenUsage
    let lastUsage: TokenUsage
    private let history: HistoryState
    private let entries: [Entry]

    init(
        sequence: UInt64,
        nextBeforeSequence: UInt64?,
        transcript: [TranscriptEntry],
        currentUsage: TokenUsage,
        lastUsage: TokenUsage
    ) {
        schemaVersion = Self.currentSchemaVersion
        self.sequence = sequence
        self.currentUsage = currentUsage
        self.lastUsage = lastUsage
        history = HistoryState(nextBeforeSequence: nextBeforeSequence)
        entries = transcript.map(Entry.init)
    }

    var nextBeforeSequence: UInt64? { history.nextBeforeSequence }
    var transcript: [TranscriptEntry] { entries.map(\.transcriptEntry) }

    fileprivate func fitsCache(maximumEntries: Int, maximumContentBytes: Int) -> Bool {
        guard entries.count <= maximumEntries else { return false }
        var remaining = maximumContentBytes
        func consume(_ value: String?) -> Bool {
            guard let value else { return true }
            let count = value.utf8.count
            guard count <= remaining else { return false }
            remaining -= count
            return true
        }
        for entry in entries {
            guard consume(entry.id),
                  consume(entry.presentationID),
                  consume(entry.text),
                  consume(entry.capability),
                  consume(entry.title),
                  consume(entry.symbol),
                  consume(entry.group),
                  consume(entry.turnID),
                  consume(entry.format),
                  consume(entry.tone),
                  entry.files.allSatisfy({ file in
                      consume(file.id)
                          && consume(file.name)
                          && consume(file.mediaType)
                  })
            else { return false }
        }
        return true
    }
}

struct ComposerEditRecovery: Codable, Equatable, Sendable {
    enum Phase: String, Codable, Sendable {
        case removingQueuedInput
        case editing
        case submitting
        case completed
    }

    let capability: String
    let widgetID: String
    let originalInput: String
    let displacedDraft: String
    var editedInput: String
    var requestID: String
    var submissionBaselineSequence: UInt64?
    var phase: Phase

    fileprivate var fitsRecoveryBounds: Bool {
        capability.utf8.count <= 256
            && widgetID.utf8.count <= 1_024
            && requestID.utf8.count <= 1_024
            && originalInput.utf8.count <= maximumComposerBytes
            && displacedDraft.utf8.count <= maximumComposerBytes
            && editedInput.utf8.count <= maximumComposerBytes
    }
}

private actor GatewayDiskStore {
    private let maximumCachedTranscriptsPerAccount = 20
    private let maximumCachedTranscriptBytes = 4 * 1024 * 1024
    private let maximumCachedTranscriptContentBytes = 3 * 1024 * 1024
    private let maximumCachedTranscriptEntries = 10_000
    private let transcriptDirectory: URL
    private let draftDirectory: URL
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init(transcriptDirectory: URL, draftDirectory: URL) {
        self.transcriptDirectory = transcriptDirectory
        self.draftDirectory = draftDirectory
    }

    func loadTranscript(accountID: UUID, sessionID: String) -> CachedTranscript? {
        let url = transcriptURL(accountID: accountID, sessionID: sessionID)
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
              let size = (attributes[.size] as? NSNumber)?.intValue
        else { return nil }
        guard size <= maximumCachedTranscriptBytes,
              let data = try? Data(contentsOf: url),
              let cached = try? decoder.decode(CachedTranscript.self, from: data),
              cached.schemaVersion == CachedTranscript.currentSchemaVersion
        else {
            try? FileManager.default.removeItem(at: url)
            return nil
        }
        return cached
    }

    func saveTranscript(
        _ transcript: CachedTranscript,
        accountID: UUID,
        sessionID: String
    ) {
        let url = transcriptURL(accountID: accountID, sessionID: sessionID)
        guard transcript.fitsCache(
            maximumEntries: maximumCachedTranscriptEntries,
            maximumContentBytes: maximumCachedTranscriptContentBytes
        ) else {
            try? FileManager.default.removeItem(at: url)
            return
        }
        guard let data = try? encoder.encode(transcript) else { return }
        guard data.count <= maximumCachedTranscriptBytes else {
            try? FileManager.default.removeItem(at: url)
            return
        }
        let directory = accountTranscriptDirectory(accountID)
        try? FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        guard (try? data.write(to: url, options: protectedWriteOptions)) != nil else { return }
        trimTranscriptCache(in: directory)
    }

    func removeTranscript(accountID: UUID, sessionID: String) {
        try? FileManager.default.removeItem(
            at: transcriptURL(accountID: accountID, sessionID: sessionID)
        )
    }

    func loadComposerDraft(accountID: UUID, sessionID: String) -> String {
        let url = draftURL(accountID: accountID, sessionID: sessionID)
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
              let size = (attributes[.size] as? NSNumber)?.intValue
        else { return "" }
        guard size <= maximumComposerBytes,
              let data = try? Data(contentsOf: url),
              let draft = String(data: data, encoding: .utf8),
              !draft.isEmpty
        else {
            try? FileManager.default.removeItem(at: url)
            return ""
        }
        return draft
    }

    func saveComposerDraft(_ draft: String, accountID: UUID, sessionID: String) {
        let url = draftURL(accountID: accountID, sessionID: sessionID)
        guard !draft.isEmpty else {
            try? FileManager.default.removeItem(at: url)
            return
        }
        let data = Data(draft.utf8)
        guard data.count <= maximumComposerBytes else {
            try? FileManager.default.removeItem(at: url)
            return
        }
        try? FileManager.default.createDirectory(
            at: accountDraftDirectory(accountID),
            withIntermediateDirectories: true
        )
        try? data.write(to: url, options: protectedWriteOptions)
    }

    func loadComposerEditRecovery(
        accountID: UUID,
        sessionID: String
    ) -> ComposerEditRecovery? {
        let url = editRecoveryURL(accountID: accountID, sessionID: sessionID)
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
              let size = (attributes[.size] as? NSNumber)?.intValue
        else { return nil }
        guard size <= maximumComposerBytes * 3 + 16_384,
              let data = try? Data(contentsOf: url),
              let recovery = try? decoder.decode(ComposerEditRecovery.self, from: data),
              recovery.fitsRecoveryBounds
        else {
            try? FileManager.default.removeItem(at: url)
            return nil
        }
        return recovery.phase == .completed ? nil : recovery
    }

    func saveComposerEditRecovery(
        _ recovery: ComposerEditRecovery,
        accountID: UUID,
        sessionID: String
    ) throws {
        guard recovery.fitsRecoveryBounds else { throw GatewayStore.StoreError.invalidEditRecovery }
        let data = try encoder.encode(recovery)
        guard data.count <= maximumComposerBytes * 3 + 16_384 else {
            throw GatewayStore.StoreError.invalidEditRecovery
        }
        try FileManager.default.createDirectory(
            at: accountDraftDirectory(accountID),
            withIntermediateDirectories: true
        )
        try data.write(
            to: editRecoveryURL(accountID: accountID, sessionID: sessionID),
            options: protectedWriteOptions
        )
    }

    func removeComposerEditRecovery(accountID: UUID, sessionID: String) throws {
        let url = editRecoveryURL(accountID: accountID, sessionID: sessionID)
        guard FileManager.default.fileExists(atPath: url.path) else { return }
        try FileManager.default.removeItem(at: url)
    }

    func removeAccount(_ accountID: UUID) {
        try? FileManager.default.removeItem(at: accountTranscriptDirectory(accountID))
        try? FileManager.default.removeItem(at: accountDraftDirectory(accountID))
    }

    private var protectedWriteOptions: Data.WritingOptions {
        [.atomic, .completeFileProtection]
    }

    private func accountTranscriptDirectory(_ accountID: UUID) -> URL {
        transcriptDirectory.appendingPathComponent(accountID.uuidString, isDirectory: true)
    }

    private func accountDraftDirectory(_ accountID: UUID) -> URL {
        draftDirectory.appendingPathComponent(accountID.uuidString, isDirectory: true)
    }

    private func transcriptURL(accountID: UUID, sessionID: String) -> URL {
        accountTranscriptDirectory(accountID)
            .appendingPathComponent(filename(for: sessionID))
            .appendingPathExtension("json")
    }

    private func draftURL(accountID: UUID, sessionID: String) -> URL {
        accountDraftDirectory(accountID)
            .appendingPathComponent(filename(for: sessionID))
            .appendingPathExtension("txt")
    }

    private func editRecoveryURL(accountID: UUID, sessionID: String) -> URL {
        accountDraftDirectory(accountID)
            .appendingPathComponent(filename(for: sessionID))
            .appendingPathExtension("edit.json")
    }

    private func filename(for sessionID: String) -> String {
        Data(sessionID.utf8).base64EncodedString()
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "+", with: "-")
    }

    private func trimTranscriptCache(in directory: URL) {
        let cached = ((try? FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: [.contentModificationDateKey],
            options: [.skipsHiddenFiles]
        )) ?? [])
            .filter { $0.pathExtension == "json" }
            .map { candidate in
                let date = (try? candidate.resourceValues(
                    forKeys: [.contentModificationDateKey]
                ).contentModificationDate) ?? .distantPast
                return (url: candidate, date: date)
            }
            .sorted { $0.date > $1.date }
        for stale in cached.dropFirst(maximumCachedTranscriptsPerAccount) {
            try? FileManager.default.removeItem(at: stale.url)
        }
    }
}

@MainActor
final class GatewayStore {
    private let defaults: UserDefaults
    private let diskStore: GatewayDiskStore
    private let accountsKey = "paired-gateways"
    private let selectedAccountKey = "selected-gateway"
    private let keychainService = "app.mobius.gateway"
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init(
        defaults: UserDefaults = .standard,
        transcriptDirectory: URL? = nil,
        draftDirectory: URL? = nil
    ) {
        self.defaults = defaults
        diskStore = GatewayDiskStore(
            transcriptDirectory: transcriptDirectory
                ?? URL.cachesDirectory
                    .appendingPathComponent("mobius", isDirectory: true)
                    .appendingPathComponent("Transcripts", isDirectory: true),
            draftDirectory: draftDirectory
                ?? URL.applicationSupportDirectory
                    .appendingPathComponent("mobius", isDirectory: true)
                    .appendingPathComponent("Drafts", isDirectory: true)
        )
    }

    func loadAccounts() -> [GatewayAccount] {
        guard let data = defaults.data(forKey: accountsKey),
              let accounts = try? decoder.decode([GatewayAccount].self, from: data)
        else { return [] }
        return accounts
    }

    func save(_ account: GatewayAccount, token: String) throws {
        try saveToken(token, accountID: account.id)
        var accounts = loadAccounts()
        if let index = accounts.firstIndex(where: { $0.id == account.id }) {
            accounts[index] = account
        } else {
            accounts.append(account)
        }
        defaults.set(try encoder.encode(accounts), forKey: accountsKey)
        defaults.set(account.id.uuidString, forKey: selectedAccountKey)
    }

    func selectedAccountID() -> UUID? {
        defaults.string(forKey: selectedAccountKey).flatMap(UUID.init(uuidString:))
    }

    func select(_ account: GatewayAccount) {
        defaults.set(account.id.uuidString, forKey: selectedAccountKey)
    }

    func recordMachineName(_ machineName: String, for account: GatewayAccount) throws {
        var accounts = loadAccounts()
        guard let index = accounts.firstIndex(where: { $0.id == account.id }) else {
            throw StoreError.missingAccount
        }
        guard accounts[index].machineName != machineName else { return }
        accounts[index].machineName = machineName
        defaults.set(try encoder.encode(accounts), forKey: accountsKey)
    }

    func recordCloudUserID(_ userID: UUID, for account: GatewayAccount) throws {
        var accounts = loadAccounts()
        guard let index = accounts.firstIndex(where: { $0.id == account.id }) else {
            throw StoreError.missingAccount
        }
        guard accounts[index].cloudUserID != userID else { return }
        accounts[index].cloudUserID = userID
        defaults.set(try encoder.encode(accounts), forKey: accountsKey)
    }

    func rename(_ account: GatewayAccount, to rawName: String) throws -> GatewayAccount {
        let name = rawName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty,
              name.utf8.count <= 128,
              !name.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        else { throw StoreError.invalidDisplayName }

        var accounts = loadAccounts()
        guard let index = accounts.firstIndex(where: { $0.id == account.id }) else {
            throw StoreError.missingAccount
        }
        var renamed = account
        renamed.displayName = name
        accounts[index] = renamed
        defaults.set(try encoder.encode(accounts), forKey: accountsKey)
        return renamed
    }

    func token(for account: GatewayAccount) throws -> String {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: keychainService,
            kSecAttrAccount: account.id.uuidString,
            kSecReturnData: true,
            kSecMatchLimit: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status != errSecItemNotFound else { throw StoreError.missingToken }
        guard status == errSecSuccess,
              let data = result as? Data,
              let token = String(data: data, encoding: .utf8)
        else {
            throw StoreError.keychain(status)
        }
        return token
    }

    func remove(_ account: GatewayAccount) async throws {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: keychainService,
            kSecAttrAccount: account.id.uuidString,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw StoreError.keychain(status)
        }
        let accounts = loadAccounts().filter { $0.id != account.id }
        defaults.set(try encoder.encode(accounts), forKey: accountsKey)
        if selectedAccountID() == account.id {
            defaults.removeObject(forKey: selectedAccountKey)
        }
        await diskStore.removeAccount(account.id)
    }

    func loadTranscript(accountID: UUID, sessionID: String) async -> CachedTranscript? {
        await diskStore.loadTranscript(accountID: accountID, sessionID: sessionID)
    }

    func saveTranscript(
        _ transcript: CachedTranscript,
        accountID: UUID,
        sessionID: String
    ) async {
        await diskStore.saveTranscript(transcript, accountID: accountID, sessionID: sessionID)
    }

    func saveTranscript(
        accountID: UUID,
        sessionID: String,
        sequence: UInt64,
        nextBeforeSequence: UInt64? = nil,
        transcript: [TranscriptEntry],
        currentUsage: TokenUsage,
        lastUsage: TokenUsage
    ) async {
        guard !transcript.isEmpty else { return }
        await saveTranscript(CachedTranscript(
            sequence: sequence,
            nextBeforeSequence: nextBeforeSequence,
            transcript: transcript,
            currentUsage: currentUsage,
            lastUsage: lastUsage
        ), accountID: accountID, sessionID: sessionID)
    }

    func removeTranscript(accountID: UUID, sessionID: String) async {
        await diskStore.removeTranscript(accountID: accountID, sessionID: sessionID)
    }

    func loadComposerDraft(accountID: UUID, sessionID: String) async -> String {
        await diskStore.loadComposerDraft(accountID: accountID, sessionID: sessionID)
    }

    func saveComposerDraft(_ draft: String, accountID: UUID, sessionID: String) async {
        await diskStore.saveComposerDraft(
            draft,
            accountID: accountID,
            sessionID: sessionID
        )
    }

    func loadComposerEditRecovery(
        accountID: UUID,
        sessionID: String
    ) async -> ComposerEditRecovery? {
        await diskStore.loadComposerEditRecovery(accountID: accountID, sessionID: sessionID)
    }

    func saveComposerEditRecovery(
        _ recovery: ComposerEditRecovery,
        accountID: UUID,
        sessionID: String
    ) async throws {
        try await diskStore.saveComposerEditRecovery(
            recovery,
            accountID: accountID,
            sessionID: sessionID
        )
    }

    func removeComposerEditRecovery(accountID: UUID, sessionID: String) async throws {
        try await diskStore.removeComposerEditRecovery(
            accountID: accountID,
            sessionID: sessionID
        )
    }

    private func saveToken(_ token: String, accountID: UUID) throws {
        guard let data = token.data(using: .utf8) else { throw StoreError.invalidToken }
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: keychainService,
            kSecAttrAccount: accountID.uuidString,
        ]
        let attributes: [CFString: Any] = [
            kSecValueData: data,
            kSecAttrAccessible: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
        let updateStatus = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        guard updateStatus == errSecItemNotFound else {
            guard updateStatus == errSecSuccess else { throw StoreError.keychain(updateStatus) }
            return
        }

        let item = query.merging(attributes) { _, new in new }
        let addStatus = SecItemAdd(item as CFDictionary, nil)
        guard addStatus == errSecSuccess else { throw StoreError.keychain(addStatus) }
    }

}

extension GatewayStore {
    enum StoreError: LocalizedError {
        case invalidDisplayName
        case missingAccount
        case invalidToken
        case invalidEditRecovery
        case missingToken
        case keychain(OSStatus)

        var errorDescription: String? {
            switch self {
            case .invalidDisplayName: "Use a gateway name between 1 and 128 characters."
            case .missingAccount: "This gateway is no longer saved."
            case .invalidToken: "The gateway token is invalid."
            case .invalidEditRecovery: "The queued message edit is too large to save safely."
            case .missingToken: "This gateway needs to be paired again."
            case .keychain(let status):
                SecCopyErrorMessageString(status, nil) as String?
                    ?? "Keychain operation failed (\(status))."
            }
        }
    }
}
