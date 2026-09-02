import Foundation
import CoreGraphics
import LocalAuthentication

enum AppDestination: Hashable {
    case chats
    case gateway
    case botDefaults
    case providers
    case extensions
    case bots
    case scratchpad
    case profile
    case contribution(String)

    var glyph: MobiusGlyph {
        switch self {
        case .chats: .note01
        case .gateway: .cellTower
        case .botDefaults: .slidersHorizontal
        case .providers: .plugsConnected
        case .extensions: .squaresFour
        case .bots: .aiScan
        case .scratchpad: .brain
        case .profile: .gear
        case .contribution: .squaresFour
        }
    }
}

/// A detail page pushed from one of the settings sections.
enum SettingsRoute: Hashable {
    case gateway(UUID)
    case provider(String)
    case extensionPackage(String)
}

/// Everything the detail column can push. Settings details join the same stack as
/// chats so the interactive back gesture pops the page instead of the column.
enum AppRoute: Hashable {
    case chat(ChatRoute)
    case bot(String)
    case botSessions(String)
    case swarm(String)
    case swarmChat(String)
    case settings(SettingsRoute)
}

struct WorkspaceSessions: Identifiable {
    let id: String
    let name: String
    let path: String
    let sessions: [SessionRecord]

    var latestUpdatedAt: Int64 {
        sessions.first?.updatedAt ?? 0
    }

    static func grouped(
        _ sessions: [SessionRecord],
        prioritizing workspaceID: String?
    ) -> [WorkspaceSessions] {
        Dictionary(grouping: sessions, by: workspaceID(for:)).map { id, sessions in
            let path = sessions.first?.sessionContext.workspaceLabel ?? "Workspace"
            return WorkspaceSessions(
                id: id,
                name: workspaceName(path),
                path: path,
                sessions: sessions.sorted(by: pinnedThenRecent)
            )
        }
        .sorted {
            if $0.id == workspaceID { return true }
            if $1.id == workspaceID { return false }
            if $0.latestUpdatedAt != $1.latestUpdatedAt {
                return $0.latestUpdatedAt > $1.latestUpdatedAt
            }
            return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
    }

    private static func workspaceID(for session: SessionRecord) -> String {
        session.sessionContext.workspaceId
            ?? session.sessionContext.workspaceLabel
            ?? "workspace"
    }

    private static func workspaceName(_ path: String) -> String {
        let name = URL(fileURLWithPath: path).lastPathComponent
        return name.isEmpty ? path : name
    }

    private static func pinnedThenRecent(_ first: SessionRecord, _ second: SessionRecord) -> Bool {
        if first.pinned != second.pinned { return first.pinned }
        if first.updatedAt != second.updatedAt { return first.updatedAt > second.updatedAt }
        return first.sessionId < second.sessionId
    }
}

enum ChatRoute: Identifiable, Hashable {
    case new
    case session(String)

    var id: String { sessionID ?? "new" }

    var sessionID: String? {
        switch self {
        case .new: nil
        case .session(let sessionID): sessionID
        }
    }
}

enum ConnectionState: Equatable {
    case disconnected
    case connecting
    case authenticating
    case loading
    case ready
    case failed(String)

    var label: LocalizedStringResource {
        switch self {
        case .disconnected: "Offline"
        case .connecting: "Connecting"
        case .authenticating: "Authenticating"
        case .loading: "Opening workspace"
        case .ready: "Ready"
        case .failed: "Needs attention"
        }
    }

    var isLoading: Bool {
        switch self {
        case .connecting, .authenticating, .loading: true
        case .disconnected, .ready, .failed: false
        }
    }

    var isReady: Bool { self == .ready }

    var tone: ToastTone {
        switch self {
        case .ready: .success
        case .connecting, .authenticating, .loading: .warning
        case .disconnected, .failed: .error
        }
    }
}

enum ApplyState: Equatable {
    case idle
    case applying
    case restarting
    case applied
    case busy(String)
    case conflict(String)
    case invalid(String)
    case failed(String)
}

enum ProviderActionState: Equatable {
    case idle
    case savingCredential(String)
    case credentialSaved(String)
    case startingLogin(String)
    case deviceCode(provider: String, url: String, code: String)
    case loginFinished(String)
    case failed(String)
}

enum ExtensionAction: Equatable {
    case installing
    case updating(String)
    case uninstalling(String)
    case trusting(String)
    case untrusting(String)
}

enum ToastTone: Equatable {
    case info
    case success
    case warning
    case error
}

struct AppToast: Identifiable {
    let id = UUID()
    let message: String
    let tone: ToastTone
    let sessionID: String?

    init(message: String, tone: ToastTone, sessionID: String? = nil) {
        self.message = message
        self.tone = tone
        self.sessionID = sessionID
    }
}

enum ComposerAttachmentState: Equatable {
    case queued
    case uploading
    case uploaded(SessionFileReference)
    case failed(String)
}

struct ComposerAttachment: Identifiable, Equatable {
    let id: UUID
    let name: String
    let size: Int64
    let mediaType: String
    var state: ComposerAttachmentState
}

struct PendingComposerDraft {
    let text: String
    let attachments: [SessionFileReference]
}

struct PendingWidgetEdit {
    let owner: ComposerDraftOwner
    var recovery: ComposerEditRecovery
}

struct ComposerDraftOwner: Equatable, Sendable {
    let accountID: UUID
    let sessionID: String
}

struct ReplayUserMessage {
    let sequence: UInt64
    let text: String
}

let maximumObservedReplaySubmissions = 1_024

enum SessionFileUploadRequest {
    case begin(localID: UUID)
    case chunk(localID: UUID, expectedNextOffset: Int64)
    case finish(localID: UUID)

    var localID: UUID {
        switch self {
        case .begin(let localID), .chunk(let localID, _), .finish(let localID): localID
        }
    }
}

struct ActiveSessionFileUpload {
    let localID: UUID
    let sessionID: String
    let uploadID: String
    let maxChunkBytes: Int
}

struct SessionFileDownload {
    let generation: UUID
    let file: SessionFileReference
    let sessionID: String
    let purpose: SessionFileDownloadPurpose
    var data: Data
    var requestID: String
}

enum FileThumbnailKey: Hashable, Sendable {
    case composer(UUID)
    case session(sessionID: String, fileID: String)
}

struct SessionFileThumbnailDownload {
    let file: SessionFileReference
    let sessionID: String
    var data: Data
    var requestID: String
}

enum SessionFileDownloadPurpose: Equatable {
    case preview
    case share
}

struct WorkspaceFilePreviewDownload {
    let generation: UUID
    let file: WorkspaceFileRecord
    let sessionID: String
    var data: Data
    var requestID: String
}

struct ImportedAttachmentData: Sendable {
    let name: String
    let mediaType: String
    let data: Data
    let thumbnail: CGImage?
}

struct TemporarySessionFile: Sendable {
    let directory: URL
    let url: URL
}

struct TextFilePreview: Identifiable {
    let id: UUID
    let name: String
    let originalContents: String
    var contents: String
    let workspaceSessionID: String?
    let originalWorkspacePath: String?
    var workspacePath: String?

    init(
        id: UUID,
        name: String,
        contents: String,
        workspaceSessionID: String? = nil,
        workspacePath: String? = nil
    ) {
        self.id = id
        self.name = name
        self.originalContents = contents
        self.contents = contents
        self.workspaceSessionID = workspaceSessionID
        self.originalWorkspacePath = workspacePath
        self.workspacePath = workspacePath
    }
}

struct SessionFileShareItem: Identifiable {
    let id: UUID
    let name: String
    let url: URL
}

enum AttachmentImportError: LocalizedError {
    case notAFile
    case tooLarge(Int64)
    case totalTooLarge(Int64)
    case changedWhileReading

    var localizedDescriptionResource: LocalizedStringResource {
        let byteLimit: (Int64) -> String = { bytes in
            let mebibyte: Int64 = 1024 * 1024
            return bytes.isMultiple(of: mebibyte)
                ? "\(bytes / mebibyte) MiB"
                : "\(bytes) bytes"
        }
        return switch self {
        case .notAFile: "Choose a regular file."
        case .tooLarge(let bytes):
            "Attachments are limited to \(byteLimit(bytes)) each."
        case .totalTooLarge(let bytes):
            "Attachments in one message are limited to \(byteLimit(bytes)) total."
        case .changedWhileReading: "The file changed while möbius was reading it. Try again."
        }
    }

    var errorDescription: String? { String(localized: localizedDescriptionResource) }
}

enum ThemePreference: String, CaseIterable, Identifiable {
    case system
    case dark
    case lightsOut = "lights out"
    case light

    var id: Self { self }

    var label: LocalizedStringResource {
        switch self {
        case .system: "System"
        case .dark: "Dark"
        case .lightsOut: "Lights Out"
        case .light: "Light"
        }
    }
}

enum AppLanguage: String, CaseIterable, Identifiable {
    case system
    case english = "en"
    case french = "fr"
    case german = "de"

    var id: Self { self }

    var label: LocalizedStringResource {
        switch self {
        case .system: "System"
        case .english: "English"
        case .french: "French"
        case .german: "German"
        }
    }

    var locale: Locale {
        switch self {
        case .system:
            .autoupdatingCurrent
        case .english, .french, .german:
            Locale(identifier: rawValue)
        }
    }
}

enum FilesInspectorTab: String, CaseIterable, Identifiable {
    case modified
    case allFiles
    case chatFiles

    var id: Self { self }
}

enum ModifiedFilesScope: CaseIterable, Identifiable {
    case lastTurn
    case unstaged
    case staged
    case committed

    var id: Self { self }
}

extension SessionRecord {
    var explicitTitle: String? {
        guard let title = title?.trimmingCharacters(in: .whitespacesAndNewlines),
              !title.isEmpty
        else { return nil }
        return title
    }
}

/// One entry in the workspace file tree. `children` is nil for a file, which is how
/// `List(children:)` decides a row gets no disclosure control.
struct FileTreeNode: Identifiable, Hashable, Sendable {
    let id: String
    let name: String
    let size: Int64?
    let children: [FileTreeNode]?

    var isFolder: Bool { children != nil }

    /// The gateway sends a flat list of paths; a browser needs them nested, folders first
    /// and then in the case-insensitive order Finder uses.
    static func tree(from files: [WorkspaceFileRecord]) -> [FileTreeNode] {
        nodes(
            files.map {
                (components: $0.path.split(separator: "/").map(String.init), size: Int64(clamping: $0.size))
            },
            prefix: ""
        )
    }

    private static func nodes(
        _ entries: [(components: [String], size: Int64)],
        prefix: String
    ) -> [FileTreeNode] {
        let groups = Dictionary(grouping: entries.filter { !$0.components.isEmpty }) {
            $0.components[0]
        }
        return groups.map { name, group -> FileTreeNode in
            let path = prefix.isEmpty ? name : "\(prefix)/\(name)"
            let nested = group
                .filter { $0.components.count > 1 }
                .map { (components: Array($0.components.dropFirst()), size: $0.size) }
            guard nested.isEmpty else {
                return FileTreeNode(id: path, name: name, size: nil, children: nodes(nested, prefix: path))
            }
            return FileTreeNode(id: path, name: name, size: group[0].size, children: nil)
        }
        .sorted {
            $0.isFolder == $1.isFolder
                ? $0.name.localizedStandardCompare($1.name) == .orderedAscending
                : $0.isFolder
        }
    }
}

enum AppLockAuthenticationMethod: Equatable {
    case faceID
    case touchID
    case biometrics
    case unavailable

    var settingTitle: LocalizedStringResource {
        switch self {
        case .faceID: "Require Face ID"
        case .touchID: "Require Touch ID"
        case .biometrics: "Require Biometric Authentication"
        case .unavailable: "Require Face ID or Touch ID"
        }
    }

    var unlockTitle: LocalizedStringResource {
        switch self {
        case .faceID: "Unlock with Face ID"
        case .touchID: "Unlock with Touch ID"
        case .biometrics: "Unlock with Biometrics"
        case .unavailable: "Unlock with Face ID or Touch ID"
        }
    }

    var glyph: MobiusGlyph {
        switch self {
        case .faceID: .userFocus
        case .touchID: .fingerprint
        case .biometrics, .unavailable: .fingerprint
        }
    }

    var isAvailable: Bool { self != .unavailable }
}

@MainActor
struct AppLockAuthenticator {
    private let methodProvider: () -> AppLockAuthenticationMethod
    private let evaluator: (String, String) async -> Bool

    init(
        method: @escaping () -> AppLockAuthenticationMethod,
        authenticate: @escaping (String) async -> Bool
    ) {
        methodProvider = method
        evaluator = { reason, _ in await authenticate(reason) }
    }

    init() {
        methodProvider = {
            let context = LAContext()
            var error: NSError?
            guard context.canEvaluatePolicy(
                .deviceOwnerAuthenticationWithBiometrics,
                error: &error
            ) else {
                return .unavailable
            }
            return switch context.biometryType {
            case .faceID: .faceID
            case .touchID: .touchID
            case .opticID: .biometrics
            case .none: .unavailable
            @unknown default: .biometrics
            }
        }
        evaluator = { reason, cancelTitle in
            let context = LAContext()
            context.localizedCancelTitle = cancelTitle
            context.localizedFallbackTitle = ""
            var error: NSError?
            guard context.canEvaluatePolicy(
                .deviceOwnerAuthenticationWithBiometrics,
                error: &error
            ) else {
                return false
            }
            return (try? await context.evaluatePolicy(
                .deviceOwnerAuthenticationWithBiometrics,
                localizedReason: reason
            )) == true
        }
    }

    var method: AppLockAuthenticationMethod { methodProvider() }

    func authenticate(reason: String, cancelTitle: String) async -> Bool {
        await evaluator(reason, cancelTitle)
    }
}

let appLockEnabledKey = "app-lock-enabled"
let maximumClientAttachmentBytes = 50 * 1024 * 1024
let maximumClientComposerAttachmentBytes: Int64 = 100 * 1024 * 1024
let maximumClientUploadChunkBytes = 256 * 1024
let maximumPresentedFileBytes = 50 * 1024 * 1024
let maximumWorkspaceTextFileBytes = 1024 * 1024
let maximumHighlightedPreviewBytes = 1024 * 1024
let transcriptTurnsPerPage = 1

struct TranscriptProjectionKey: Equatable {
    let version: Int
    let count: Int
    let boundaryID: String?
    let firstID: String?
    let lastID: String?
    let waitingPhrase: TranscriptWaitingPhrase?
}

struct TranscriptWindowCache {
    let entries: [TranscriptEntry]
    let turnCount: Int
    let hasEarlierEntries: Bool
}

enum TranscriptWindowAnchor: Equatable {
    case tail
    case visibleTurns(Int)
}

struct TranscriptHistoryTurnState {
    var turnID: String?
    var unassignedEntryStart: Int?
    var awaitingInitialMessageTurnID: String?
}

struct BufferedAgentEvent {
    let record: RecordedEvent
}

struct ApprovalCall: Identifiable, Equatable {
    let id: String
    let name: String
    let arguments: String
}

struct PendingApproval: Equatable {
    let id: String
    let reason: String
    let calls: [ApprovalCall]
}

struct PairingCodeInfo: Equatable {
    let code: String
    let expiresAt: Date
}

struct MountedWidget: Identifiable, Sendable {
    let capability: String
    let widget: FrontendWidget

    var id: String { "\(capability)\u{0}\(widget.id)" }
    var title: String { widget.content?.title ?? widget.text }
}

struct MountedReference: Identifiable, Sendable {
    let capability: String
    let reference: FrontendReference
    let replacement: String

    init(
        capability: String,
        reference: FrontendReference,
        replacement: String? = nil
    ) {
        self.capability = capability
        self.reference = reference
        self.replacement = replacement ?? "\(reference.trigger)\(reference.value)"
    }

    var id: String { "\(capability)\u{0}\(reference.trigger)\u{0}\(reference.value)" }
    var label: String { "\(reference.trigger)\(reference.value)" }
}

struct ReferenceSuggestions: Sendable {
    let source: String
    let range: Range<String.Index>
    let matches: [MountedReference]
}

struct ReferenceMatchScore: Comparable {
    let tier: Int
    let gaps: Int
    let length: Int

    static func < (lhs: Self, rhs: Self) -> Bool {
        if lhs.tier != rhs.tier { return lhs.tier < rhs.tier }
        if lhs.gaps != rhs.gaps { return lhs.gaps < rhs.gaps }
        return lhs.length < rhs.length
    }
}

struct TranscriptPreview: Identifiable {
    let id: String
    let title: String
    let context: String
    let status: String?
    let model: String?
    let entries: [TranscriptEntry]
    let next: AgentOperation?
}

struct FrontendPickerPrompt: Sendable {
    let title: String
    let options: [FrontendPickerOption]
}

struct ChatTitleAttempt: Hashable {
    let accountID: UUID
    let sessionID: String
    let submissionID: String
    let prompt: String
}

struct PendingChatTitle {
    let attempt: ChatTitleAttempt
    let previewTitle: String
    var generatedTitle: String?
    var renameRequestID: String?
    var submissionConfirmed: Bool

    var displayTitle: String { generatedTitle ?? previewTitle }
}
