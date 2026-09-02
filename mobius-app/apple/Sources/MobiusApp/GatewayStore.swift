import CryptoKit
import Foundation
import Security

struct CachedTranscript: Codable, Sendable {
    static let currentSchemaVersion = 4

    private struct HistoryState: Codable, Sendable {
        let nextBeforeSequence: UInt64?
    }

    private enum EntryIdentity: Codable, Sendable {
        case message(TranscriptMessageMetadata)
        case narrative(TranscriptEntry.Kind)

        init(_ entry: TranscriptEntry) {
            self = entry.messageMetadata.map(Self.message) ?? .narrative(entry.kind)
        }

        var kind: TranscriptEntry.Kind {
            switch self {
            case .message(let metadata): metadata.kind
            case .narrative(let kind): kind
            }
        }

        var messageMetadata: TranscriptMessageMetadata? {
            guard case .message(let metadata) = self else { return nil }
            return metadata
        }
    }

    private struct Entry: Codable, Sendable {
        let id: String
        let presentationID: String?
        let text: String
        let identity: EntryIdentity
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
        let annotations: [JSONValue]?

        init(_ entry: TranscriptEntry) {
            id = entry.id
            presentationID = entry.presentationID
            text = entry.text
            identity = EntryIdentity(entry)
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
            annotations = entry.annotations.isEmpty ? nil : entry.annotations
        }

        var transcriptEntry: TranscriptEntry {
            TranscriptEntry(
                id: id,
                presentationID: presentationID,
                text: text,
                kind: identity.kind,
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
                files: files,
                annotations: annotations ?? [],
                messageMetadata: identity.messageMetadata
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
                  consume(entry.identity.messageMetadata?.author.peerFields?.messageID),
                  consume(entry.identity.messageMetadata?.author.peerFields?.sessionID),
                  consume(entry.identity.messageMetadata?.author.peerFields?.handle),
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

struct CachedChatCatalog: Codable, Equatable, Sendable {
    static let currentSchemaVersion = 3

    let schemaVersion: Int
    let bots: [BotRecord]
    let sessions: [SessionRecord]
    let swarms: [SwarmRecord]
    let lastSessionID: String?

    init(
        bots: [BotRecord],
        sessions: [SessionRecord],
        swarms: [SwarmRecord],
        lastSessionID: String?
    ) {
        schemaVersion = Self.currentSchemaVersion
        self.bots = bots
        self.sessions = sessions.map { session in
            SessionRecord(
                sessionId: session.sessionId,
                sessionContext: session.sessionContext,
                parentSessionId: session.parentSessionId,
                parentSequence: session.parentSequence,
                sequence: session.sequence,
                firstUserMessage: session.firstUserMessage,
                executionStats: session.executionStats,
                title: session.title,
                pinned: session.pinned,
                activity: SessionActivity(
                    state: .idle,
                    turnId: nil,
                    startedAt: nil,
                    lastOutcome: session.activity.lastOutcome,
                    message: nil
                ),
                createdAt: session.createdAt,
                updatedAt: session.updatedAt
            )
        }
        self.swarms = swarms.map { swarm in
            SwarmRecord(
                id: swarm.id,
                title: swarm.title,
                leaderBotId: swarm.leaderBotId,
                members: swarm.members,
                messages: [],
                updatedAtMs: swarm.updatedAtMs
            )
        }
        self.lastSessionID = lastSessionID.flatMap { sessionID in
            sessions.contains { $0.sessionId == sessionID } ? sessionID : nil
        }
    }

    fileprivate var isValid: Bool {
        let botIDs = Set(bots.map(\.id))
        let botHandles = Set(bots.map(\.handle))
        let sessionIDs = Set(sessions.map(\.sessionId))
        var swarmBotIDs = Set<String>()
        return bots.count <= 100
            && botIDs.count == bots.count
            && !botIDs.contains("")
            && botHandles.count == bots.count
            && bots.allSatisfy { bot in
                bot.handle == bot.handle.trimmingCharacters(in: .whitespacesAndNewlines)
                    && !bot.handle.isEmpty
                    && !bot.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    && !bot.description.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            }
            && sessions.count <= 100
            && sessionIDs.count == sessions.count
            && !sessionIDs.contains("")
            && sessions.allSatisfy { botIDs.contains($0.sessionContext.botId) }
            && swarms.count <= 100
            && Set(swarms.map(\.id)).count == swarms.count
            && swarms.allSatisfy { swarm in
                !swarm.id.isEmpty
                    && swarm.messages.isEmpty
                    && Set(swarm.members.map(\.botId)).count == swarm.members.count
                    && swarm.members.contains { $0.botId == swarm.leaderBotId }
                    && swarm.members.allSatisfy {
                        botIDs.contains($0.botId) && swarmBotIDs.insert($0.botId).inserted
                    }
            }
            && lastSessionID.map(sessionIDs.contains) ?? true
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
    private let maximumCachedCatalogBytes = 4 * 1024 * 1024
    private let maximumCachedTranscriptsPerAccount = 20
    private let maximumCachedTranscriptBytes = 4 * 1024 * 1024
    private let maximumCachedTranscriptContentBytes = 3 * 1024 * 1024
    private let maximumCachedTranscriptEntries = 10_000
    private let maximumCachedThumbnailsPerAccount = 32
    private let maximumCachedThumbnailBytes = 1024 * 1024
    private let catalogDirectory: URL
    private let transcriptDirectory: URL
    private let thumbnailDirectory: URL
    private let draftDirectory: URL
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init(
        catalogDirectory: URL,
        transcriptDirectory: URL,
        thumbnailDirectory: URL,
        draftDirectory: URL
    ) {
        self.catalogDirectory = catalogDirectory
        self.transcriptDirectory = transcriptDirectory
        self.thumbnailDirectory = thumbnailDirectory
        self.draftDirectory = draftDirectory
    }

    func loadChatCatalog(accountID: UUID) -> CachedChatCatalog? {
        let url = catalogURL(accountID)
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
            let size = (attributes[.size] as? NSNumber)?.intValue
        else { return nil }
        guard size <= maximumCachedCatalogBytes,
            let data = try? Data(contentsOf: url),
            let cached = try? decoder.decode(CachedChatCatalog.self, from: data),
            cached.schemaVersion == CachedChatCatalog.currentSchemaVersion,
            cached.isValid
        else {
            try? FileManager.default.removeItem(at: url)
            return nil
        }
        return cached
    }

    func saveChatCatalog(_ catalog: CachedChatCatalog, accountID: UUID) {
        let url = catalogURL(accountID)
        guard catalog.isValid,
            let data = try? encoder.encode(catalog),
            data.count <= maximumCachedCatalogBytes
        else {
            try? FileManager.default.removeItem(at: url)
            return
        }
        try? FileManager.default.createDirectory(
            at: catalogDirectory,
            withIntermediateDirectories: true
        )
        try? data.write(to: url, options: protectedWriteOptions)
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

    func loadThumbnail(accountID: UUID, sessionID: String, fileID: String) -> Data? {
        let url = thumbnailURL(accountID: accountID, sessionID: sessionID, fileID: fileID)
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
            let size = (attributes[.size] as? NSNumber)?.intValue,
            size <= maximumCachedThumbnailBytes,
            let data = try? Data(contentsOf: url),
            !data.isEmpty,
            data.count <= maximumCachedThumbnailBytes
        else {
            try? FileManager.default.removeItem(at: url)
            return nil
        }
        return data
    }

    func saveThumbnail(_ data: Data, accountID: UUID, sessionID: String, fileID: String) {
        let url = thumbnailURL(accountID: accountID, sessionID: sessionID, fileID: fileID)
        guard !data.isEmpty, data.count <= maximumCachedThumbnailBytes else {
            try? FileManager.default.removeItem(at: url)
            return
        }
        let directory = accountThumbnailDirectory(accountID)
        try? FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        guard (try? data.write(to: url, options: protectedWriteOptions)) != nil else { return }
        trimThumbnailCache(in: directory)
    }

    func removeThumbnail(accountID: UUID, sessionID: String, fileID: String) {
        try? FileManager.default.removeItem(
            at: thumbnailURL(accountID: accountID, sessionID: sessionID, fileID: fileID)
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
        try? FileManager.default.removeItem(at: catalogURL(accountID))
        try? FileManager.default.removeItem(at: accountTranscriptDirectory(accountID))
        try? FileManager.default.removeItem(at: accountThumbnailDirectory(accountID))
        try? FileManager.default.removeItem(at: accountDraftDirectory(accountID))
    }

    func clearCachedData() throws {
        try removeItemIfPresent(at: catalogDirectory)
        try removeItemIfPresent(at: transcriptDirectory)
        try removeItemIfPresent(at: thumbnailDirectory)
    }

    func clearAllData() throws {
        try clearCachedData()
        try removeItemIfPresent(at: draftDirectory)
    }

    private var protectedWriteOptions: Data.WritingOptions {
        [.atomic, .completeFileProtection]
    }

    private func removeItemIfPresent(at url: URL) throws {
        do {
            try FileManager.default.removeItem(at: url)
        } catch let error as CocoaError where error.code == .fileNoSuchFile {
            return
        }
    }

    private func accountTranscriptDirectory(_ accountID: UUID) -> URL {
        transcriptDirectory.appendingPathComponent(accountID.uuidString, isDirectory: true)
    }

    private func accountThumbnailDirectory(_ accountID: UUID) -> URL {
        thumbnailDirectory.appendingPathComponent(accountID.uuidString, isDirectory: true)
    }

    private func accountDraftDirectory(_ accountID: UUID) -> URL {
        draftDirectory.appendingPathComponent(accountID.uuidString, isDirectory: true)
    }

    private func catalogURL(_ accountID: UUID) -> URL {
        catalogDirectory
            .appendingPathComponent(accountID.uuidString)
            .appendingPathExtension("json")
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

    private func thumbnailURL(accountID: UUID, sessionID: String, fileID: String) -> URL {
        let key = SHA256.hash(data: Data("\(sessionID)\0\(fileID)".utf8)).map { byte in
            let hex = String(byte, radix: 16)
            return byte < 16 ? "0\(hex)" : hex
        }.joined()
        return accountThumbnailDirectory(accountID)
            .appendingPathComponent(key)
            .appendingPathExtension("png")
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

    private func trimThumbnailCache(in directory: URL) {
        let cached =
            ((try? FileManager.default.contentsOfDirectory(
                at: directory,
                includingPropertiesForKeys: [.contentModificationDateKey],
                options: [.skipsHiddenFiles]
            )) ?? [])
            .filter { $0.pathExtension == "png" }
            .map { candidate in
                let date =
                    (try? candidate.resourceValues(
                        forKeys: [.contentModificationDateKey]
                    ).contentModificationDate) ?? .distantPast
                return (url: candidate, date: date)
            }
            .sorted { $0.date > $1.date }
        for stale in cached.dropFirst(maximumCachedThumbnailsPerAccount) {
            try? FileManager.default.removeItem(at: stale.url)
        }
    }
}

struct SessionReadCursor: Codable, Equatable {
    let sequence: UInt64
    let wasActive: Bool
}

@MainActor
final class GatewayStore {
    private let defaults: UserDefaults
    private let diskStore: GatewayDiskStore
    private let accountsKey = "paired-gateways"
    private let selectedAccountKey = "selected-gateway"
    private let sessionReadCursorsKeyPrefix = "session-read-cursors."
    private let keychainService = "app.mobius.gateway"
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init(
        defaults: UserDefaults = .standard,
        catalogDirectory: URL? = nil,
        transcriptDirectory: URL? = nil,
        thumbnailDirectory: URL? = nil,
        draftDirectory: URL? = nil
    ) {
        self.defaults = defaults
        let cacheDirectory = URL.cachesDirectory
            .appendingPathComponent("mobius", isDirectory: true)
        diskStore = GatewayDiskStore(
            catalogDirectory: catalogDirectory
                ?? cacheDirectory.appendingPathComponent("Catalogs", isDirectory: true),
            transcriptDirectory: transcriptDirectory
                ?? cacheDirectory.appendingPathComponent("Transcripts", isDirectory: true),
            thumbnailDirectory: thumbnailDirectory
                ?? cacheDirectory.appendingPathComponent("Thumbnails", isDirectory: true),
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

    func loadSessionReadCursors(accountID: UUID) -> [String: SessionReadCursor]? {
        guard let data = defaults.data(forKey: sessionReadCursorsKey(accountID)) else { return nil }
        return try? decoder.decode([String: SessionReadCursor].self, from: data)
    }

    func saveSessionReadCursors(
        _ cursors: [String: SessionReadCursor],
        accountID: UUID
    ) {
        guard let data = try? encoder.encode(cursors) else { return }
        defaults.set(data, forKey: sessionReadCursorsKey(accountID))
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
        try removeToken(accountID: account.id)
        let accounts = loadAccounts().filter { $0.id != account.id }
        defaults.set(try encoder.encode(accounts), forKey: accountsKey)
        if selectedAccountID() == account.id {
            defaults.removeObject(forKey: selectedAccountKey)
        }
        defaults.removeObject(forKey: sessionReadCursorsKey(account.id))
        await diskStore.removeAccount(account.id)
    }

    func loadChatCatalog(accountID: UUID) async -> CachedChatCatalog? {
        await diskStore.loadChatCatalog(accountID: accountID)
    }

    func saveChatCatalog(_ catalog: CachedChatCatalog, accountID: UUID) async {
        await diskStore.saveChatCatalog(catalog, accountID: accountID)
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

    func loadThumbnail(accountID: UUID, sessionID: String, fileID: String) async -> Data? {
        await diskStore.loadThumbnail(
            accountID: accountID,
            sessionID: sessionID,
            fileID: fileID
        )
    }

    func saveThumbnail(
        _ data: Data,
        accountID: UUID,
        sessionID: String,
        fileID: String
    ) async {
        await diskStore.saveThumbnail(
            data,
            accountID: accountID,
            sessionID: sessionID,
            fileID: fileID
        )
    }

    func removeThumbnail(accountID: UUID, sessionID: String, fileID: String) async {
        await diskStore.removeThumbnail(
            accountID: accountID,
            sessionID: sessionID,
            fileID: fileID
        )
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

    func clearCachedData() async throws {
        try await diskStore.clearCachedData()
    }

    func clearAllData() async throws {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: keychainService,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw StoreError.keychain(status)
        }
        for key in defaults.dictionaryRepresentation().keys
        where key.hasPrefix(sessionReadCursorsKeyPrefix) {
            defaults.removeObject(forKey: key)
        }
        defaults.removeObject(forKey: accountsKey)
        defaults.removeObject(forKey: selectedAccountKey)
        try await diskStore.clearAllData()
    }

    private func sessionReadCursorsKey(_ accountID: UUID) -> String {
        sessionReadCursorsKeyPrefix + accountID.uuidString
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

    private func removeToken(accountID: UUID) throws {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: keychainService,
            kSecAttrAccount: accountID.uuidString,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw StoreError.keychain(status)
        }
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

        var localizedDescriptionResource: LocalizedStringResource? {
            switch self {
            case .invalidDisplayName: "Use a gateway name between 1 and 128 characters."
            case .missingAccount: "This gateway is no longer saved."
            case .invalidToken: "The gateway token is invalid."
            case .invalidEditRecovery: "The queued message edit is too large to save safely."
            case .missingToken: "This gateway needs to be paired again."
            case .keychain: nil
            }
        }

        var errorDescription: String? {
            if let localizedDescriptionResource {
                return String(localized: localizedDescriptionResource)
            }
            guard case .keychain(let status) = self else { return nil }
            return SecCopyErrorMessageString(status, nil) as String?
                ?? String(localized: "Keychain operation failed (\(status)).")
        }
    }
}
