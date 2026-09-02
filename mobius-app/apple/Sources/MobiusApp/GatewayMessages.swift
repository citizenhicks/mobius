import Foundation

enum GatewayRequest: Encodable, Sendable {
    case pair(code: String, clientLabel: String, clientKind: GatewayClientKind)
    case authenticate(token: String, clientKind: GatewayClientKind)
    case listClients(requestID: String)
    case unpairClient(requestID: String, clientID: String)
    case listSessions(requestID: String)
    case listBotSessions(requestID: String, botID: String)
    case createSession(requestID: String, workspace: String, botID: String)
    case openSession(
        requestID: String,
        sessionID: String,
        lastSequence: UInt64?
    )
    case getSessionHistory(
        requestID: String,
        sessionID: String,
        beforeSequence: UInt64?
    )
    case renameSession(requestID: String, sessionID: String, title: String)
    case setSessionPinned(requestID: String, sessionID: String, pinned: Bool)
    case deleteSession(requestID: String, sessionID: String)
    case createSwarm(
        requestID: String,
        title: String,
        leaderBotID: String,
        memberBotIDs: [String]
    )
    case addSwarmMember(requestID: String, swarmID: String, botID: String)
    case leaveSwarm(requestID: String, swarmID: String, botID: String)
    case renameSwarm(requestID: String, swarmID: String, title: String)
    case disbandSwarm(requestID: String, swarmID: String)
    case postSwarmMessage(
        requestID: String,
        swarmID: String,
        text: String
    )
    case submit(sessionID: String, submission: Submission)
    case submitScratchpad(
        requestID: String,
        scope: ScratchpadScope,
        operation: AgentOperation
    )
    case createBot(
        requestID: String,
        name: String,
        description: String
    )
    case listBots(requestID: String)
    case updateBot(
        requestID: String,
        id: String,
        expectedRevision: UInt64,
        name: String,
        description: String,
        tint: AccentTint,
        config: AgentComposition
    )
    case deleteBot(requestID: String, id: String, expectedRevision: UInt64)
    case configureBotDefaults(
        requestID: String,
        expectedRevision: UInt64,
        config: AgentComposition
    )
    case installExtension(
        requestID: String,
        source: String,
        reference: String?,
        subdirectory: String?
    )
    case updateExtension(requestID: String, id: String)
    case uninstallExtension(requestID: String, id: String)
    case trustExtensionHooks(requestID: String, id: String, expectedDigest: String)
    case revokeExtensionHooksTrust(requestID: String, id: String, expectedDigest: String)
    case probeGitCredential(requestID: String, target: String)
    case approveGitCredential(
        requestID: String,
        target: String,
        username: String,
        token: String
    )
    case listSshIdentities(requestID: String)
    case generateSshIdentity(requestID: String)
    case getGitDiff(requestID: String, sessionID: String, scope: GitDiffScope)
    case listWorkspaceFiles(requestID: String, sessionID: String, scope: WorkspaceFileScope)
    case readWorkspaceFile(
        requestID: String,
        sessionID: String,
        path: String,
        offset: UInt64,
        maxBytes: Int
    )
    case writeWorkspaceFile(
        requestID: String,
        sessionID: String,
        path: String,
        content: String
    )
    case beginSessionFileUpload(
        requestID: String,
        sessionID: String,
        name: String,
        size: Int64,
        mediaType: String
    )
    case uploadSessionFileChunk(
        requestID: String,
        sessionID: String,
        uploadID: String,
        offset: Int64,
        data: Data
    )
    case finishSessionFileUpload(requestID: String, sessionID: String, uploadID: String)
    case listSessionFiles(requestID: String, sessionID: String)
    case readSessionFile(
        requestID: String,
        sessionID: String,
        fileID: String,
        offset: Int64,
        maxBytes: Int
    )
    case switchGitBranch(requestID: String, sessionID: String, branch: String)
    case listDirectories(requestID: String, path: String, includeFiles: Bool)
    case createWorkspaceDirectory(requestID: String, parent: String, name: String)
    case setProviderCredential(
        requestID: String,
        instance: String,
        provider: String,
        apiKey: String
    )
    case setProviderEndpointCredential(
        requestID: String,
        instance: String,
        provider: String,
        baseURL: String,
        apiKey: String
    )
    case registerProvider(
        requestID: String,
        config: ProviderConfig,
        label: String,
        tint: AccentTint,
        modelIds: [String],
        reasoningEfforts: [String]
    )
    case removeProvider(requestID: String, instance: String)
    case createPairingCode(requestID: String)
    case startProviderLogin(requestID: String, provider: String)
    case getProfile(requestID: String)
    case createRoutine(
        requestID: String,
        botID: String,
        workspace: String,
        instructions: String,
        schedule: RoutineSchedule,
        endsAt: Int64?
    )
    case listRoutines(requestID: String, botID: String?)
    case updateRoutine(
        requestID: String,
        id: String,
        botID: String,
        workspace: String,
        instructions: String,
        schedule: RoutineSchedule,
        endsAt: Int64?,
        enabled: Bool
    )
    case deleteRoutine(requestID: String, id: String)
    case deleteRoutineRun(requestID: String, id: String)
    case runRoutine(requestID: String, id: String)
    case listRoutineHistory(requestID: String, id: String?)
    case getRoutineRunPreview(requestID: String, id: String, beforeSequence: UInt64?)

    // One exhaustive switch is the wire contract; splitting it would add fake dispatch.
    // swift-complexity:disable cyclomatic
    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: DynamicCodingKey.self)
        try container.encode(gatewayProtocolVersion, forKey: "version")
        switch self {
        case .pair(let code, let clientLabel, let clientKind):
            try container.encode("pair", forKey: "type")
            try container.encode(code, forKey: "code")
            try container.encode(clientLabel, forKey: "clientLabel")
            try container.encode(clientKind, forKey: "clientKind")
        case .authenticate(let token, let clientKind):
            try container.encode("authenticate", forKey: "type")
            try container.encode(token, forKey: "token")
            try container.encode(clientKind, forKey: "clientKind")
        case .listClients(let requestID):
            try container.encode("list_clients", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
        case .unpairClient(let requestID, let clientID):
            try container.encode("unpair_client", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(clientID, forKey: "clientId")
        case .listSessions(let requestID):
            try container.encode("list_sessions", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
        case .listBotSessions(let requestID, let botID):
            try container.encode("list_bot_sessions", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(botID, forKey: "botId")
        case .createSession(let requestID, let workspace, let botID):
            try container.encode("create_session", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(workspace, forKey: "workspace")
            try container.encode(botID, forKey: "botId")
        case .openSession(let requestID, let sessionID, let lastSequence):
            try container.encode("open_session", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(lastSequence, forKey: "lastSequence")
        case .getSessionHistory(let requestID, let sessionID, let beforeSequence):
            try container.encode("get_session_history", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(beforeSequence, forKey: "beforeSequence")
        case .renameSession(let requestID, let sessionID, let title):
            try container.encode("rename_session", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(title, forKey: "title")
        case .setSessionPinned(let requestID, let sessionID, let pinned):
            try container.encode("set_session_pinned", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(pinned, forKey: "pinned")
        case .deleteSession(let requestID, let sessionID):
            try container.encode("delete_session", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
        case .createSwarm(let requestID, let title, let leaderBotID, let memberBotIDs):
            try container.encode("create_swarm", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(title, forKey: "title")
            try container.encode(leaderBotID, forKey: "leaderBotId")
            try container.encode(memberBotIDs, forKey: "memberBotIds")
        case .addSwarmMember(let requestID, let swarmID, let botID):
            try container.encode("add_swarm_member", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(swarmID, forKey: "swarmId")
            try container.encode(botID, forKey: "botId")
        case .leaveSwarm(let requestID, let swarmID, let botID):
            try container.encode("leave_swarm", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(swarmID, forKey: "swarmId")
            try container.encode(botID, forKey: "botId")
        case .renameSwarm(let requestID, let swarmID, let title):
            try container.encode("rename_swarm", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(swarmID, forKey: "swarmId")
            try container.encode(title, forKey: "title")
        case .disbandSwarm(let requestID, let swarmID):
            try container.encode("disband_swarm", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(swarmID, forKey: "swarmId")
        case .postSwarmMessage(let requestID, let swarmID, let text):
            try container.encode("post_swarm_message", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(swarmID, forKey: "swarmId")
            try container.encode(text, forKey: "text")
        case .submit(let sessionID, let submission):
            try container.encode("submit", forKey: "type")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(submission, forKey: "submission")
        case .submitScratchpad(let requestID, let scope, let operation):
            try container.encode("submit_scratchpad", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(scope, forKey: "scope")
            try container.encode(operation, forKey: "operation")
        case .createBot(let requestID, let name, let description):
            try container.encode("create_bot", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(name, forKey: "name")
            try container.encode(description, forKey: "description")
        case .listBots(let requestID):
            try container.encode("list_bots", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
        case .updateBot(
            let requestID,
            let id,
            let expectedRevision,
            let name,
            let description,
            let tint,
            let config
        ):
            try container.encode("update_bot", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
            try container.encode(expectedRevision, forKey: "expectedRevision")
            try container.encode(name, forKey: "name")
            try container.encode(description, forKey: "description")
            try container.encode(tint, forKey: "tint")
            try container.encode(config, forKey: "config")
        case .deleteBot(let requestID, let id, let expectedRevision):
            try container.encode("delete_bot", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
            try container.encode(expectedRevision, forKey: "expectedRevision")
        case .configureBotDefaults(let requestID, let expectedRevision, let config):
            try container.encode("configure_bot_defaults", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(expectedRevision, forKey: "expectedRevision")
            try container.encode(config, forKey: "config")
        case .installExtension(let requestID, let source, let reference, let subdirectory):
            try container.encode("install_extension", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(source, forKey: "source")
            try container.encode(reference, forKey: "reference")
            try container.encode(subdirectory, forKey: "subdirectory")
        case .updateExtension(let requestID, let id):
            try container.encode("update_extension", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
        case .uninstallExtension(let requestID, let id):
            try container.encode("uninstall_extension", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
        case .trustExtensionHooks(let requestID, let id, let expectedDigest):
            try container.encode("trust_extension_hooks", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
            try container.encode(expectedDigest, forKey: "expectedDigest")
        case .revokeExtensionHooksTrust(let requestID, let id, let expectedDigest):
            try container.encode("revoke_extension_hooks_trust", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
            try container.encode(expectedDigest, forKey: "expectedDigest")
        case .probeGitCredential(let requestID, let target):
            try container.encode("probe_git_credential", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(target, forKey: "target")
        case .approveGitCredential(let requestID, let target, let username, let token):
            try container.encode("approve_git_credential", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(target, forKey: "target")
            try container.encode(username, forKey: "username")
            try container.encode(token, forKey: "token")
        case .listSshIdentities(let requestID):
            try container.encode("list_ssh_identities", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
        case .generateSshIdentity(let requestID):
            try container.encode("generate_ssh_identity", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
        case .getGitDiff(let requestID, let sessionID, let scope):
            try container.encode("get_git_diff", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(scope, forKey: "scope")
        case .listWorkspaceFiles(let requestID, let sessionID, let scope):
            try container.encode("list_workspace_files", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(scope, forKey: "scope")
        case .readWorkspaceFile(let requestID, let sessionID, let path, let offset, let maxBytes):
            try container.encode("read_workspace_file", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(path, forKey: "path")
            try container.encode(offset, forKey: "offset")
            try container.encode(maxBytes, forKey: "maxBytes")
        case .writeWorkspaceFile(let requestID, let sessionID, let path, let content):
            try container.encode("write_workspace_file", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(path, forKey: "path")
            try container.encode(content, forKey: "content")
        case .beginSessionFileUpload(let requestID, let sessionID, let name, let size, let mediaType):
            try container.encode("begin_session_file_upload", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(name, forKey: "name")
            try container.encode(size, forKey: "size")
            try container.encode(mediaType, forKey: "mediaType")
        case .uploadSessionFileChunk(let requestID, let sessionID, let uploadID, let offset, let data):
            try container.encode("upload_session_file_chunk", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(uploadID, forKey: "uploadId")
            try container.encode(offset, forKey: "offset")
            try container.encode(data, forKey: "data")
        case .finishSessionFileUpload(let requestID, let sessionID, let uploadID):
            try container.encode("finish_session_file_upload", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(uploadID, forKey: "uploadId")
        case .listSessionFiles(let requestID, let sessionID):
            try container.encode("list_session_files", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
        case .readSessionFile(let requestID, let sessionID, let fileID, let offset, let maxBytes):
            try container.encode("read_session_file", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(fileID, forKey: "fileId")
            try container.encode(offset, forKey: "offset")
            try container.encode(maxBytes, forKey: "maxBytes")
        case .switchGitBranch(let requestID, let sessionID, let branch):
            try container.encode("switch_git_branch", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(branch, forKey: "branch")
        case .listDirectories(let requestID, let path, let includeFiles):
            try container.encode("list_directories", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(path, forKey: "path")
            try container.encode(includeFiles, forKey: "includeFiles")
        case .createWorkspaceDirectory(let requestID, let parent, let name):
            try container.encode("create_workspace_directory", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(parent, forKey: "parent")
            try container.encode(name, forKey: "name")
        case .setProviderCredential(let requestID, let instance, let provider, let apiKey):
            try container.encode("set_provider_credential", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(instance, forKey: "instance")
            try container.encode(provider, forKey: "provider")
            try container.encode(apiKey, forKey: "apiKey")
        case .setProviderEndpointCredential(
            let requestID,
            let instance,
            let provider,
            let baseURL,
            let apiKey
        ):
            try container.encode("set_provider_endpoint_credential", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(instance, forKey: "instance")
            try container.encode(provider, forKey: "provider")
            try container.encode(baseURL, forKey: "baseUrl")
            try container.encode(apiKey, forKey: "apiKey")
        case .registerProvider(
            let requestID,
            let config,
            let label,
            let tint,
            let modelIds,
            let reasoningEfforts
        ):
            try container.encode("register_provider", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(config, forKey: "config")
            try container.encode(label, forKey: "label")
            try container.encode(tint, forKey: "tint")
            try container.encode(modelIds, forKey: "modelIds")
            try container.encode(reasoningEfforts, forKey: "reasoningEfforts")
        case .removeProvider(let requestID, let instance):
            try container.encode("remove_provider", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(instance, forKey: "instance")
        case .createPairingCode(let requestID):
            try container.encode("create_pairing_code", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
        case .startProviderLogin(let requestID, let provider):
            try container.encode("start_provider_login", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(provider, forKey: "provider")
        case .getProfile(let requestID):
            try container.encode("get_profile", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
        case .createRoutine(
            let requestID,
            let botID,
            let workspace,
            let instructions,
            let schedule,
            let endsAt
        ):
            try container.encode("create_routine", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(botID, forKey: "botId")
            try container.encode(workspace, forKey: "workspace")
            try container.encode(instructions, forKey: "instructions")
            try container.encode(schedule, forKey: "schedule")
            try container.encode(endsAt, forKey: "endsAt")
        case .listRoutines(let requestID, let botID):
            try container.encode("list_routines", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(botID, forKey: "botId")
        case .updateRoutine(
            let requestID,
            let id,
            let botID,
            let workspace,
            let instructions,
            let schedule,
            let endsAt,
            let enabled
        ):
            try container.encode("update_routine", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
            try container.encode(botID, forKey: "botId")
            try container.encode(workspace, forKey: "workspace")
            try container.encode(instructions, forKey: "instructions")
            try container.encode(schedule, forKey: "schedule")
            try container.encode(endsAt, forKey: "endsAt")
            try container.encode(enabled, forKey: "enabled")
        case .deleteRoutine(let requestID, let id):
            try container.encode("delete_routine", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
        case .deleteRoutineRun(let requestID, let id):
            try container.encode("delete_routine_run", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
        case .runRoutine(let requestID, let id):
            try container.encode("run_routine", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
        case .listRoutineHistory(let requestID, let id):
            try container.encode("list_routine_history", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
        case .getRoutineRunPreview(let requestID, let id, let beforeSequence):
            try container.encode("get_routine_run_preview", forKey: "type")
            try container.encode(requestID, forKey: "requestId")
            try container.encode(id, forKey: "id")
            try container.encode(beforeSequence, forKey: "beforeSequence")
        }
    }
}

enum GatewayEnvelope: Decodable, Sendable {
    case paired(clientID: String, token: String)
    case authenticated
    case ready(ReadyPayload)
    case sessionOpened(requestID: String, payload: SessionReadyPayload)
    case sessionReplayComplete(requestID: String, sessionID: String)
    case sessionHistory(
        requestID: String,
        sessionID: String,
        records: [RecordedEvent],
        nextBeforeSequence: UInt64?
    )
    case sessionChanged(SessionReadyPayload)
    case gatewayConfigured(requestID: String, payload: ReadyPayload)
    case scratchpadChanged(
        requestID: String,
        scope: ScratchpadScope,
        contribution: FrontendContribution
    )
    case accepted(requestID: String)
    case rejected(GatewayRejection)
    case agentEvent(
        sessionID: String,
        record: RecordedEvent
    )
    case sessions(requestID: String?, sessions: [SessionRecord])
    case botSessions(requestID: String, botID: String, sessions: [SessionRecord])
    case bots(requestID: String?, bots: [BotRecord])
    case swarms(requestID: String?, swarms: [SwarmRecord])
    case clients(requestID: String, currentClientID: String, clients: [ClientStatus])
    case providerCredentialSaved(requestID: String, instance: String, provider: String)
    case pairingCode(requestID: String, code: String, expiresAt: Int64)
    case providerLoginStarted(
        requestID: String,
        loginID: String,
        provider: String,
        verificationURL: String,
        userCode: String
    )
    case providerLoginFinished(requestID: String, loginID: String, provider: String)
    case gitCredentialStatus(requestID: String, available: Bool, username: String?)
    case sshIdentities(requestID: String, identities: [SshIdentityRecord])
    case sshIdentityGenerated(
        requestID: String,
        identity: SshIdentityRecord,
        publicKey: String
    )
    case profile(requestID: String, profile: ProfileSnapshot)
    case gitDiff(requestID: String, sessionID: String, scope: GitDiffScope, diff: String)
    case workspaceFiles(
        requestID: String,
        sessionID: String,
        files: [WorkspaceFileRecord],
        truncated: Bool
    )
    case workspaceFileChunk(
        requestID: String,
        sessionID: String,
        path: String,
        offset: UInt64,
        data: Data,
        nextOffset: UInt64?
    )
    case sessionFileUploadReady(
        requestID: String,
        sessionID: String,
        uploadID: String,
        maxChunkBytes: Int
    )
    case sessionFileUploadChunkAccepted(
        requestID: String,
        sessionID: String,
        uploadID: String,
        nextOffset: Int64
    )
    case sessionFileUploadCompleted(
        requestID: String,
        sessionID: String,
        file: SessionFileReference
    )
    case sessionFiles(requestID: String, sessionID: String, files: [SessionFileRecord])
    case sessionFileChunk(
        requestID: String,
        sessionID: String,
        fileID: String,
        offset: Int64,
        data: Data,
        nextOffset: Int64?
    )
    case directories(requestID: String, listing: DirectoryListing)
    case routines(requestID: String, routines: [Routine])
    case routineHistory(requestID: String, runs: [RoutineRun])
    case routineRunPreview(RoutineRunPreview)
    case error(GatewayFailure)

    // One exhaustive switch is the wire contract; splitting it would add fake dispatch.
    // swift-complexity:disable cyclomatic
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: DynamicCodingKey.self)
        let version = try container.decode(Int.self, forKey: "version")
        guard version == gatewayProtocolVersion else {
            throw GatewayWireError.unsupportedVersion(version)
        }
        let type = try container.decode(String.self, forKey: "type")
        switch type {
        case "paired":
            self = .paired(
                clientID: try container.decode(String.self, forKey: "clientId"),
                token: try container.decode(String.self, forKey: "token")
            )
        case "authenticated":
            self = .authenticated
        case "ready":
            self = .ready(try container.decode(
                ReadyPayload.self,
                forKey: "payload"
            ).validated())
        case "session_opened":
            self = .sessionOpened(
                requestID: try container.decode(String.self, forKey: "requestId"),
                payload: try container.decode(SessionReadyPayload.self, forKey: "payload")
            )
        case "session_replay_complete":
            self = .sessionReplayComplete(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId")
            )
        case "session_history":
            self = .sessionHistory(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                records: try container.decode([RecordedEvent].self, forKey: "records"),
                nextBeforeSequence: try container.decodeIfPresent(
                    UInt64.self,
                    forKey: "nextBeforeSequence"
                )
            )
        case "session_changed":
            self = .sessionChanged(
                try container.decode(SessionReadyPayload.self, forKey: "payload")
            )
        case "gateway_configured":
            self = .gatewayConfigured(
                requestID: try container.decode(String.self, forKey: "requestId"),
                payload: try container.decode(
                    ReadyPayload.self,
                    forKey: "payload"
                ).validated()
            )
        case "scratchpad_changed":
            self = .scratchpadChanged(
                requestID: try container.decode(String.self, forKey: "requestId"),
                scope: try container.decode(ScratchpadScope.self, forKey: "scope"),
                contribution: try container.decode(
                    FrontendContribution.self,
                    forKey: "contribution"
                )
            )
        case "accepted":
            self = .accepted(requestID: try container.decode(String.self, forKey: "requestId"))
        case "rejected":
            self = .rejected(try GatewayRejection(from: decoder))
        case "agent_event":
            self = .agentEvent(
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                record: try container.decode(RecordedEvent.self, forKey: "record")
            )
        case "sessions":
            self = .sessions(
                requestID: try container.decodeIfPresent(String.self, forKey: "requestId"),
                sessions: try container.decode([SessionRecord].self, forKey: "sessions")
            )
        case "bot_sessions":
            self = .botSessions(
                requestID: try container.decode(String.self, forKey: "requestId"),
                botID: try container.decode(String.self, forKey: "botId"),
                sessions: try container.decode([SessionRecord].self, forKey: "sessions")
            )
        case "bots":
            self = .bots(
                requestID: try container.decodeIfPresent(String.self, forKey: "requestId"),
                bots: try container.decode([BotRecord].self, forKey: "bots")
            )
        case "swarms":
            self = .swarms(
                requestID: try container.decodeIfPresent(String.self, forKey: "requestId"),
                swarms: try container.decode([SwarmRecord].self, forKey: "swarms")
            )
        case "clients":
            self = .clients(
                requestID: try container.decode(String.self, forKey: "requestId"),
                currentClientID: try container.decode(String.self, forKey: "currentClientId"),
                clients: try container.decode([ClientStatus].self, forKey: "clients")
            )
        case "provider_credential_saved":
            self = .providerCredentialSaved(
                requestID: try container.decode(String.self, forKey: "requestId"),
                instance: try container.decode(String.self, forKey: "instance"),
                provider: try container.decode(String.self, forKey: "provider")
            )
        case "pairing_code":
            self = .pairingCode(
                requestID: try container.decode(String.self, forKey: "requestId"),
                code: try container.decode(String.self, forKey: "code"),
                expiresAt: try container.decode(Int64.self, forKey: "expiresAt")
            )
        case "provider_login_started":
            self = .providerLoginStarted(
                requestID: try container.decode(String.self, forKey: "requestId"),
                loginID: try container.decode(String.self, forKey: "loginId"),
                provider: try container.decode(String.self, forKey: "provider"),
                verificationURL: try container.decode(String.self, forKey: "verificationUrl"),
                userCode: try container.decode(String.self, forKey: "userCode")
            )
        case "provider_login_finished":
            self = .providerLoginFinished(
                requestID: try container.decode(String.self, forKey: "requestId"),
                loginID: try container.decode(String.self, forKey: "loginId"),
                provider: try container.decode(String.self, forKey: "provider")
            )
        case "git_credential_status":
            self = .gitCredentialStatus(
                requestID: try container.decode(String.self, forKey: "requestId"),
                available: try container.decode(Bool.self, forKey: "available"),
                username: try container.decodeIfPresent(String.self, forKey: "username")
            )
        case "ssh_identities":
            self = .sshIdentities(
                requestID: try container.decode(String.self, forKey: "requestId"),
                identities: try container.decode([SshIdentityRecord].self, forKey: "identities")
            )
        case "ssh_identity_generated":
            self = .sshIdentityGenerated(
                requestID: try container.decode(String.self, forKey: "requestId"),
                identity: try container.decode(SshIdentityRecord.self, forKey: "identity"),
                publicKey: try container.decode(String.self, forKey: "publicKey")
            )
        case "profile":
            self = .profile(
                requestID: try container.decode(String.self, forKey: "requestId"),
                profile: try container.decode(ProfileSnapshot.self, forKey: "profile")
            )
        case "git_diff":
            self = .gitDiff(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                scope: try container.decode(GitDiffScope.self, forKey: "scope"),
                diff: try container.decode(String.self, forKey: "diff")
            )
        case "workspace_files":
            self = .workspaceFiles(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                files: try container.decode([WorkspaceFileRecord].self, forKey: "files"),
                truncated: try container.decodeIfPresent(Bool.self, forKey: "truncated") ?? false
            )
        case "workspace_file_chunk":
            self = .workspaceFileChunk(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                path: try container.decode(String.self, forKey: "path"),
                offset: try container.decode(UInt64.self, forKey: "offset"),
                data: try container.decode(Data.self, forKey: "data"),
                nextOffset: try container.decodeIfPresent(UInt64.self, forKey: "nextOffset")
            )
        case "session_file_upload_ready":
            self = .sessionFileUploadReady(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                uploadID: try container.decode(String.self, forKey: "uploadId"),
                maxChunkBytes: try container.decode(Int.self, forKey: "maxChunkBytes")
            )
        case "session_file_upload_chunk_accepted":
            self = .sessionFileUploadChunkAccepted(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                uploadID: try container.decode(String.self, forKey: "uploadId"),
                nextOffset: try container.decode(Int64.self, forKey: "nextOffset")
            )
        case "session_file_upload_completed":
            self = .sessionFileUploadCompleted(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                file: try container.decode(SessionFileReference.self, forKey: "file")
            )
        case "session_files":
            self = .sessionFiles(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                files: try container.decode([SessionFileRecord].self, forKey: "files")
            )
        case "session_file_chunk":
            self = .sessionFileChunk(
                requestID: try container.decode(String.self, forKey: "requestId"),
                sessionID: try container.decode(String.self, forKey: "sessionId"),
                fileID: try container.decode(String.self, forKey: "fileId"),
                offset: try container.decode(Int64.self, forKey: "offset"),
                data: try container.decode(Data.self, forKey: "data"),
                nextOffset: try container.decodeIfPresent(Int64.self, forKey: "nextOffset")
            )
        case "directories":
            self = .directories(
                requestID: try container.decode(String.self, forKey: "requestId"),
                listing: try container.decode(DirectoryListing.self, forKey: "listing")
            )
        case "routines":
            self = .routines(
                requestID: try container.decode(String.self, forKey: "requestId"),
                routines: try container.decode([Routine].self, forKey: "routines")
            )
        case "routine_history":
            self = .routineHistory(
                requestID: try container.decode(String.self, forKey: "requestId"),
                runs: try container.decode([RoutineRun].self, forKey: "runs")
            )
        case "routine_run_preview":
            let preview = try container.nestedContainer(
                keyedBy: DynamicCodingKey.self,
                forKey: DynamicCodingKey("preview")
            )
            self = .routineRunPreview(RoutineRunPreview(
                requestID: try container.decode(String.self, forKey: "requestId"),
                routine: try preview.decode(Routine.self, forKey: "routine"),
                run: try preview.decode(RoutineRun.self, forKey: "run"),
                records: try preview.decode([RecordedEvent].self, forKey: "records"),
                nextBeforeSequence: try preview.decodeIfPresent(
                    UInt64.self,
                    forKey: "nextBeforeSequence"
                )
            ))
        case "error":
            self = .error(try GatewayFailure(from: decoder))
        default:
            throw GatewayWireError.invalidFrame("unknown gateway message \(type)")
        }
    }
}

struct GatewayRejection: Decodable, Sendable {
    let requestId: String
    let code: String
    let message: String
    let fatal: Bool
}

struct GatewayFailure: Decodable, Sendable {
    let code: String
    let message: String
    let fatal: Bool
}

struct SessionFileLimits: Decodable, Equatable, Sendable {
    let maxAttachmentReferences: Int
    let maxFileBytes: UInt64
    let maxSessionFiles: Int
    let maxSessionBytes: UInt64
    let maxUploadChunkBytes: Int
}

struct ReadyPayload: Decodable, Sendable {
    let machineName: String
    let bots: [BotRecord]
    let sessions: [SessionRecord]
    let swarms: [SwarmRecord]
    let providers: [ProviderStatus]
    let providerInstances: [ProviderInstance]
    let botDefaults: VersionedAgentConfig?
    let models: [ModelChoice]
    let modelProviders: [String: String]
    let middlewareFeatures: [MiddlewareFeature]
    let extensions: [ExtensionRecord]
    let contributions: [FrontendContribution]
    let maxActiveSessions: Int
    let sessionFileLimits: SessionFileLimits
}

private extension ReadyPayload {
    func validated() throws -> Self {
        guard machineName == machineName.trimmingCharacters(in: .whitespacesAndNewlines),
              !machineName.isEmpty,
              machineName.utf8.count <= 255,
              !machineName.unicodeScalars.contains(where: {
                  CharacterSet.controlCharacters.contains($0)
              })
        else {
            throw GatewayWireError.invalidFrame("gateway machine name is invalid")
        }
        for provider in providers {
            var values = Set<String>()
            guard provider.webSearch.first?.value == HostedWebSearch.off.rawValue,
                  provider.webSearch.allSatisfy({ option in
                      option.value == option.value.trimmingCharacters(in: .whitespacesAndNewlines)
                          && HostedWebSearch(rawValue: option.value) != nil
                          && !option.label.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                          && !option.description.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                          && values.insert(option.value).inserted
                  })
            else {
                throw GatewayWireError.invalidFrame("provider web search options are invalid")
            }
        }
        guard sessionFileLimits.maxAttachmentReferences > 0,
              sessionFileLimits.maxFileBytes > 0,
              sessionFileLimits.maxSessionFiles >= sessionFileLimits.maxAttachmentReferences,
              sessionFileLimits.maxSessionBytes >= sessionFileLimits.maxFileBytes,
              sessionFileLimits.maxUploadChunkBytes > 0,
              UInt64(sessionFileLimits.maxUploadChunkBytes) <= sessionFileLimits.maxFileBytes
        else {
            throw GatewayWireError.invalidFrame("gateway session file limits are invalid")
        }
        return self
    }
}

struct SessionReadyPayload: Decodable, Sendable {
    let latestSequence: UInt64
    let nextBeforeSequence: UInt64?
    let workspace: WorkspaceInfo
    let git: GitStatus?
    let session: SessionConfigured
    let contributions: [FrontendContribution]
    let widgets: [SessionWidget]
    let toolCount: Int
    let compactionCount: UInt64
    let contextLimitTokens: Int64?
    let runStats: RunStats
}

struct SessionWidget: Decodable, Sendable {
    let capability: String
    let item: FrontendWidget
}

enum ScratchpadScope: Codable, Hashable, Sendable {
    case global
    case swarm(id: String)

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: DynamicCodingKey.self)
        switch try container.decode(String.self, forKey: "type") {
        case "global":
            self = .global
        case "swarm":
            let id = try container.decode(String.self, forKey: "id")
            guard !id.isEmpty else {
                throw GatewayWireError.invalidFrame("scratchpad swarm scope has an empty ID")
            }
            self = .swarm(id: id)
        case let type:
            throw GatewayWireError.invalidFrame("unknown scratchpad scope \(type)")
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: DynamicCodingKey.self)
        switch self {
        case .global:
            try container.encode("global", forKey: "type")
        case .swarm(let id):
            guard !id.isEmpty else {
                throw GatewayWireError.invalidFrame("scratchpad swarm scope has an empty ID")
            }
            try container.encode("swarm", forKey: "type")
            try container.encode(id, forKey: "id")
        }
    }
}

enum GitDiffScope: String, Codable, CaseIterable, Identifiable, Sendable {
    case staged
    case unstaged
    case committed

    var id: Self { self }
}

enum WorkspaceFileScope: String, Codable, CaseIterable, Identifiable, Sendable {
    case modified
    case all

    var id: Self { self }
}

struct WorkspaceFileRecord: Identifiable, Codable, Hashable, Sendable {
    var id: String { path }

    let path: String
    let size: UInt64
}

struct WorkspaceInfo: Identifiable, Codable, Hashable, Sendable {
    let id: String
    let path: String
}

struct GitStatus: Codable, Equatable, Sendable {
    let currentBranch: String
    let branches: [String]
}

struct SshIdentityRecord: Identifiable, Codable, Hashable, Sendable {
    var id: String { label }

    let label: String
    let algorithm: String
    let fingerprint: String
}

struct GeneratedSshIdentity: Identifiable, Equatable, Sendable {
    var id: String { identity.id }

    let identity: SshIdentityRecord
    let publicKey: String
}

struct DirectoryListing: Codable, Equatable, Sendable {
    let path: String
    let parent: String?
    let entries: [DirectoryEntry]
}

struct DirectoryEntry: Identifiable, Codable, Equatable, Sendable {
    var id: String { path }

    let name: String
    let path: String
    let isDirectory: Bool
}

struct SessionConfigured: Decodable, Sendable {
    let sessionId: String
    let context: SessionContext
    let model: ModelChanged
}

struct SessionContext: Codable, Hashable, Sendable {
    let botId: String
    var tenantId: String?
    var userId: String?
    var userName: String?
    var workspaceId: String?
    var workspaceLabel: String?
    var originLabel: String?
}

struct BotRecord: Identifiable, Codable, Equatable, Sendable {
    let id: String
    let handle: String
    let name: String
    let description: String
    let tint: AccentTint
    let config: VersionedAgentConfig
}

struct ModelChanged: Codable, Hashable, Sendable {
    let route: String
    let model: String
    let reasoningEffort: String?
    let modelContextWindow: Int64?
}

struct SessionRecord: Identifiable, Codable, Hashable, Sendable {
    var id: String { sessionId }

    let sessionId: String
    let sessionContext: SessionContext
    let parentSessionId: String?
    let parentSequence: UInt64?
    let sequence: UInt64
    let firstUserMessage: String?
    let executionStats: ExecutionStats
    let title: String?
    let pinned: Bool
    let activity: SessionActivity
    let createdAt: Int64
    let updatedAt: Int64
}

struct SwarmMemberRecord: Identifiable, Codable, Hashable, Sendable {
    var id: String { botId }

    let botId: String
    let handle: String
}

struct SwarmMessageRecord: Identifiable, Codable, Hashable, Sendable {
    let id: String
    let sequence: UInt64
    let authorBotId: String
    let authorHandle: String
    let sourceSessionId: String
    let text: String
    let createdAtMs: Int64
    let inReplyToMessageId: String?
    let replyDepth: UInt64
}

struct SwarmRecord: Identifiable, Codable, Hashable, Sendable {
    let id: String
    let title: String
    let leaderBotId: String
    let members: [SwarmMemberRecord]
    let messages: [SwarmMessageRecord]
    let updatedAtMs: Int64
}

struct SessionActivity: Codable, Hashable, Sendable {
    let state: SessionActivityState
    let turnId: String?
    let startedAt: Int64?
    let lastOutcome: SessionOutcome?
    let message: String?
}

enum SessionActivityState: String, Codable, Hashable, Sendable {
    case idle
    case running
    case awaitingApproval = "awaiting_approval"
}

enum SessionOutcome: String, Codable, Hashable, Sendable {
    case completed
    case aborted
    case failed
}

struct ModelChoice: Identifiable, Codable, Hashable, Sendable {
    private enum CodingKeys: String, CodingKey {
        case route, group, model, reasoningEffort, contextWindow, supportsImageInput
        case toolDiscovery
    }

    var id: String { route }

    let route: String
    let group: String
    let model: String
    let reasoningEffort: String?
    let contextWindow: Int64?
    let supportsImageInput: Bool
    let toolDiscovery: ToolDiscoveryMode

    init(
        route: String,
        group: String,
        model: String,
        reasoningEffort: String?,
        contextWindow: Int64?,
        supportsImageInput: Bool,
        toolDiscovery: ToolDiscoveryMode
    ) {
        self.route = route
        self.group = group
        self.model = model
        self.reasoningEffort = reasoningEffort
        self.contextWindow = contextWindow
        self.supportsImageInput = supportsImageInput
        self.toolDiscovery = toolDiscovery
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        self.init(
            route: try container.decode(String.self, forKey: .route),
            group: try container.decode(String.self, forKey: .group),
            model: try container.decode(String.self, forKey: .model),
            reasoningEffort: try container.decodeIfPresent(String.self, forKey: .reasoningEffort),
            contextWindow: try container.decodeIfPresent(Int64.self, forKey: .contextWindow),
            supportsImageInput: try container.decode(
                Bool.self,
                forKey: .supportsImageInput
            ),
            toolDiscovery: try container.decode(ToolDiscoveryMode.self, forKey: .toolDiscovery)
        )
    }
}
