use super::*;

const MAX_REQUEST_ID_BYTES: usize = 256;

/// One client-to-gateway frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientFrame {
    /// The version.
    pub version: u16,
    #[serde(flatten)]
    /// The message.
    pub message: ClientMessage,
}

impl<'de> Deserialize<'de> for ClientFrame {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (version, message) = deserialize_frame(deserializer)?;
        if let Some(request_id) = message.get("request_id")
            && !request_id
                .as_str()
                .is_some_and(|id| !id.is_empty() && id.len() <= MAX_REQUEST_ID_BYTES)
        {
            return Err(D::Error::custom("request ID must be 1–256 bytes"));
        }
        let message = serde_json::from_value(message).map_err(D::Error::custom)?;
        Ok(Self { version, message })
    }
}

impl ClientFrame {
    /// Wraps a message in the current protocol version.
    #[must_use]
    pub const fn new(message: ClientMessage) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            message,
        }
    }
}

/// Authenticated operations accepted by the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ClientMessage {
    /// Selects the set desktop runtime case.
    SetDesktopRuntime {
        /// The request identifier.
        request_id: String,
        /// The enabled.
        enabled: bool,
    },
    /// Selects the desktop control reply case.
    DesktopControlReply {
        /// The request identifier.
        request_id: String,
        /// The response.
        response: Value,
    },
    /// Selects the pair case.
    Pair {
        /// The code.
        code: String,
        /// The client label.
        client_label: String,
        /// The client kind.
        client_kind: ClientKind,
    },
    /// Selects the repair pairing case.
    RepairPairing {
        /// The code.
        code: String,
        /// The digest of the client token being replaced.
        replacing_token_digest: [u8; 32],
        /// The client label.
        client_label: String,
        /// The client kind.
        client_kind: ClientKind,
    },
    /// Selects the authenticate case.
    Authenticate {
        /// The token.
        token: String,
        /// The client kind.
        client_kind: ClientKind,
    },
    /// Selects the list clients case.
    ListClients {
        /// The request identifier.
        request_id: String,
    },
    /// Selects the unpair client case.
    UnpairClient {
        /// The request identifier.
        request_id: String,
        /// The client identifier.
        client_id: String,
    },
    /// Selects the list sessions case.
    ListSessions {
        /// The request identifier.
        request_id: String,
    },
    /// Selects the list bot sessions case.
    ListBotSessions {
        /// The request identifier.
        request_id: String,
        /// The bot identifier.
        bot_id: String,
    },
    /// Selects the create session case.
    CreateSession {
        /// The request identifier.
        request_id: String,
        /// The workspace.
        workspace: PathBuf,
        /// The bot identifier.
        bot_id: String,
    },
    /// Selects the create workspace directory case.
    CreateWorkspaceDirectory {
        /// The request identifier.
        request_id: String,
        /// The parent.
        parent: PathBuf,
        /// The name.
        name: String,
    },
    /// Selects the open session case.
    OpenSession {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The last sequence.
        last_sequence: Option<u64>,
    },
    /// Selects the get session history case.
    GetSessionHistory {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The before sequence.
        before_sequence: Option<u64>,
    },
    /// Selects the reassign session case.
    ReassignSession {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The bot identifier.
        bot_id: String,
    },
    /// Selects the rename session case.
    RenameSession {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The title.
        title: String,
    },
    /// Selects the attach session folder case.
    AttachSessionFolder {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The folder.
        folder: PathBuf,
    },
    /// Selects the set session pinned case.
    SetSessionPinned {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The pinned.
        pinned: bool,
    },
    /// Selects the delete sessions case.
    DeleteSessions {
        /// The request identifier.
        request_id: String,
        /// The session identifiers.
        session_ids: Vec<String>,
    },
    /// Selects the submit case.
    Submit {
        /// The session identifier.
        session_id: String,
        /// The submission.
        submission: Submission,
    },
    /// Selects the start realtime voice case.
    StartRealtimeVoice {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The offer sdp.
        offer_sdp: String,
    },
    /// Selects the end realtime voice case.
    EndRealtimeVoice {
        /// The session identifier.
        session_id: String,
        /// The voice identifier.
        voice_id: String,
    },
    /// Selects the get contributions case.
    GetContributions {
        /// The request identifier.
        request_id: String,
    },
    /// Selects the submit contribution case.
    SubmitContribution {
        /// The request identifier.
        request_id: String,
        /// The operation.
        operation: Op,
    },
    /// Selects the begin session file upload case.
    BeginSessionFileUpload {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The name.
        name: String,
        /// The size.
        size: u64,
        /// The media type.
        media_type: String,
    },
    /// Selects the upload session file chunk case.
    UploadSessionFileChunk {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The upload identifier.
        upload_id: String,
        /// The offset.
        offset: u64,
        #[serde(with = "base64_bytes")]
        /// The data.
        data: Vec<u8>,
    },
    /// Selects the finish session file upload case.
    FinishSessionFileUpload {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The upload identifier.
        upload_id: String,
    },
    /// Selects the delete session file case.
    DeleteSessionFile {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The file identifier.
        file_id: String,
    },
    /// Selects the list session files case.
    ListSessionFiles {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
    },
    /// Selects the read session file case.
    ReadSessionFile {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The file identifier.
        file_id: String,
        /// The offset.
        offset: u64,
        /// The max bytes.
        max_bytes: usize,
    },
    /// Selects the create bot case.
    CreateBot {
        /// The request identifier.
        request_id: String,
        /// The name.
        name: String,
        /// The description.
        description: String,
    },
    /// Selects the list bots case.
    ListBots {
        /// The request identifier.
        request_id: String,
    },
    /// Selects the update bot case.
    UpdateBot {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
        /// The expected revision.
        expected_revision: u64,
        /// The name.
        name: String,
        /// The description.
        description: String,
        /// The tint.
        tint: ProviderTint,
        /// The config.
        config: AgentComposition,
    },
    /// Selects the delete bot case.
    DeleteBot {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
        /// The expected revision.
        expected_revision: u64,
    },
    /// Selects the configure bot defaults case.
    ConfigureBotDefaults {
        /// The request identifier.
        request_id: String,
        /// The expected revision.
        expected_revision: u64,
        /// The config.
        config: AgentComposition,
    },
    /// Selects the install extension case.
    InstallExtension {
        /// The request identifier.
        request_id: String,
        /// The source.
        source: String,
        /// The reference.
        reference: Option<String>,
        /// The subdirectory.
        subdirectory: Option<String>,
    },
    /// Selects the update extension case.
    UpdateExtension {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
    },
    /// Selects the uninstall extension case.
    UninstallExtension {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
    },
    /// Selects the trust extension hooks case.
    TrustExtensionHooks {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
        /// The expected digest.
        expected_digest: String,
    },
    /// Selects the revoke extension hooks trust case.
    RevokeExtensionHooksTrust {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
        /// The expected digest.
        expected_digest: String,
    },
    /// Selects the probe git credential case.
    ProbeGitCredential {
        /// The request identifier.
        request_id: String,
        /// The target.
        target: String,
    },
    /// Selects the approve git credential case.
    ApproveGitCredential {
        /// The request identifier.
        request_id: String,
        /// The target.
        target: String,
        /// The username.
        username: String,
        /// The token.
        token: String,
    },
    /// Selects the list ssh identities case.
    ListSshIdentities {
        /// The request identifier.
        request_id: String,
    },
    /// Selects the generate ssh identity case.
    GenerateSshIdentity {
        /// The request identifier.
        request_id: String,
    },
    /// Selects the get git diff case.
    GetGitDiff {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The scope.
        scope: GitDiffScope,
    },
    /// Selects the switch git branch case.
    SwitchGitBranch {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The branch.
        branch: String,
    },
    /// Selects the list directories case.
    ListDirectories {
        /// The request identifier.
        request_id: String,
        /// The path.
        path: PathBuf,
        /// The include files.
        include_files: bool,
    },
    /// Selects the list workspace files case.
    ListWorkspaceFiles {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The scope.
        scope: WorkspaceFileScope,
    },
    /// Selects the read workspace file case.
    ReadWorkspaceFile {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The path.
        path: String,
        /// The offset.
        offset: u64,
        /// The max bytes.
        max_bytes: usize,
    },
    /// Selects the write workspace file case.
    WriteWorkspaceFile {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The path.
        path: String,
        /// The content.
        content: String,
    },
    /// Selects the clear provider credential case.
    ClearProviderCredential {
        /// The request identifier.
        request_id: String,
        /// The instance.
        instance: String,
    },
    /// Selects the set provider credential case.
    SetProviderCredential {
        /// The request identifier.
        request_id: String,
        /// The instance.
        instance: String,
        /// The provider.
        provider: String,
        /// The API key.
        api_key: String,
        /// The expires at.
        expires_at: Option<u64>,
    },
    /// Selects the set provider endpoint credential case.
    SetProviderEndpointCredential {
        /// The request identifier.
        request_id: String,
        /// The instance.
        instance: String,
        /// The provider.
        provider: String,
        /// The base URL.
        base_url: String,
        /// The API key.
        api_key: String,
        /// The expires at.
        expires_at: Option<u64>,
    },
    /// Selects the register provider case.
    RegisterProvider {
        /// The request identifier.
        request_id: String,
        /// The config.
        config: ProviderConfig,
        /// The label.
        label: String,
        /// The tint.
        tint: ProviderTint,
        /// The model identifiers.
        model_ids: Vec<String>,
        /// The reasoning efforts.
        reasoning_efforts: Vec<String>,
    },
    /// Selects the remove provider case.
    RemoveProvider {
        /// The request identifier.
        request_id: String,
        /// The instance.
        instance: String,
    },
    /// Selects the create pairing code case.
    CreatePairingCode {
        /// The request identifier.
        request_id: String,
    },
    /// Selects the start provider login case.
    StartProviderLogin {
        /// The request identifier.
        request_id: String,
        /// The provider.
        provider: String,
    },
    /// Selects the get profile case.
    GetProfile {
        /// The request identifier.
        request_id: String,
        /// The include provider usage.
        include_provider_usage: bool,
    },
    /// Selects the create routine case.
    CreateRoutine {
        /// The request identifier.
        request_id: String,
        /// The bot identifier.
        bot_id: String,
        /// The workspace.
        workspace: PathBuf,
        /// The instructions.
        instructions: String,
        /// The schedule.
        schedule: RoutineSchedule,
        /// The ends at.
        ends_at: Option<i64>,
    },
    /// Selects the list routines case.
    ListRoutines {
        /// The request identifier.
        request_id: String,
        /// The bot identifier.
        bot_id: Option<String>,
    },
    /// Selects the update routine case.
    UpdateRoutine {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
        /// The bot identifier.
        bot_id: String,
        /// The workspace.
        workspace: PathBuf,
        /// The instructions.
        instructions: String,
        /// The schedule.
        schedule: RoutineSchedule,
        /// The ends at.
        ends_at: Option<i64>,
        /// The enabled.
        enabled: bool,
    },
    /// Selects the delete routine case.
    DeleteRoutine {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
    },
    /// Selects the run routine case.
    RunRoutine {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
    },
    /// Selects the list routine history case.
    ListRoutineHistory {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: Option<String>,
    },
    /// Selects the delete routine run case.
    DeleteRoutineRun {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
    },
    /// Selects the get routine run preview case.
    GetRoutineRunPreview {
        /// The request identifier.
        request_id: String,
        /// The identifier.
        id: String,
        /// The before sequence.
        before_sequence: Option<u64>,
    },
}

/// One gateway-to-client frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerFrame {
    /// The version.
    pub version: u16,
    #[serde(flatten)]
    /// The message.
    pub message: ServerMessage,
}

impl<'de> Deserialize<'de> for ServerFrame {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (version, message) = deserialize_frame(deserializer)?;
        let message = serde_json::from_value(message).map_err(D::Error::custom)?;
        Ok(Self { version, message })
    }
}

impl ServerFrame {
    /// Wraps a message in the current protocol version.
    #[must_use]
    pub const fn new(message: ServerMessage) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            message,
        }
    }
}

/// Results and broadcasts emitted by the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServerMessage {
    /// Selects the desktop control requested case.
    DesktopControlRequested {
        /// The request identifier.
        request_id: String,
        /// The execution identifier.
        execution_id: String,
        /// The session identifier.
        session_id: String,
        /// The request.
        request: Value,
    },
    /// Selects the desktop control ended case.
    DesktopControlEnded {
        /// The execution identifier.
        execution_id: String,
    },
    /// Selects the paired case.
    Paired {
        /// The client identifier.
        client_id: String,
        /// The token.
        token: String,
    },
    /// Selects the authenticated case.
    Authenticated,
    /// Selects the ready case.
    Ready {
        /// The payload.
        payload: ReadyPayload,
    },
    /// Selects the session opened case.
    SessionOpened {
        /// The request identifier.
        request_id: String,
        /// The payload.
        payload: SessionReadyPayload,
    },
    /// Selects the session replay complete case.
    SessionReplayComplete {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
    },
    /// Selects the session history case.
    SessionHistory {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The records.
        records: Vec<RecordedEvent>,
        /// The next before sequence.
        next_before_sequence: Option<u64>,
    },
    /// Selects the session changed case.
    SessionChanged {
        /// The payload.
        payload: SessionReadyPayload,
    },
    /// Selects the realtime voice started case.
    RealtimeVoiceStarted {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The voice identifier.
        voice_id: String,
        /// The answer sdp.
        answer_sdp: String,
    },
    /// Selects the realtime voice failed case.
    RealtimeVoiceFailed {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The message.
        message: String,
    },
    /// Selects the realtime voice ended case.
    RealtimeVoiceEnded {
        /// The session identifier.
        session_id: String,
        /// The voice identifier.
        voice_id: String,
        /// The reason.
        reason: Option<String>,
    },
    /// Selects the gateway configured case.
    GatewayConfigured {
        /// The request identifier.
        request_id: String,
        /// The payload.
        payload: ReadyPayload,
    },
    /// Selects the contributions case.
    Contributions {
        /// The request identifier.
        request_id: String,
        /// The contributions.
        contributions: Vec<FrontendContribution>,
    },
    /// Selects the accepted case.
    Accepted {
        /// The request identifier.
        request_id: String,
    },
    /// Selects the session file upload ready case.
    SessionFileUploadReady {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The upload identifier.
        upload_id: String,
        /// The max chunk bytes.
        max_chunk_bytes: usize,
    },
    /// Selects the session file upload chunk accepted case.
    SessionFileUploadChunkAccepted {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The upload identifier.
        upload_id: String,
        /// The next offset.
        next_offset: u64,
    },
    /// Selects the session file upload completed case.
    SessionFileUploadCompleted {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The file.
        file: SessionFileReference,
    },
    /// Selects the session files case.
    SessionFiles {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The files.
        files: Vec<SessionFileRecord>,
    },
    /// Selects the session file chunk case.
    SessionFileChunk {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The file identifier.
        file_id: String,
        /// The offset.
        offset: u64,
        #[serde(with = "base64_bytes")]
        /// The data.
        data: Vec<u8>,
        /// The next offset.
        next_offset: Option<u64>,
    },
    /// Selects the rejected case.
    Rejected {
        /// The request identifier.
        request_id: String,
        /// The code.
        code: String,
        /// The message.
        message: String,
        /// The fatal.
        fatal: bool,
    },
    /// Selects the agent event case.
    AgentEvent {
        /// The session identifier.
        session_id: String,
        /// The record.
        record: RecordedEvent,
    },
    /// Selects the sessions case.
    Sessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The request identifier.
        request_id: Option<String>,
        /// The sessions.
        sessions: Vec<SessionRecord>,
    },
    /// Selects the background approvals case.
    BackgroundApprovals {
        /// The approvals.
        approvals: Vec<BackgroundApproval>,
    },

    /// Selects the bot sessions case.
    BotSessions {
        /// The request identifier.
        request_id: String,
        /// The bot identifier.
        bot_id: String,
        /// The sessions.
        sessions: Vec<SessionRecord>,
    },
    /// Selects the bots case.
    Bots {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The request identifier.
        request_id: Option<String>,
        /// The bots.
        bots: Vec<BotRecord>,
    },

    /// Selects the clients case.
    Clients {
        /// The request identifier.
        request_id: String,
        /// The current client identifier.
        current_client_id: String,
        /// The clients.
        clients: Vec<ClientStatus>,
    },
    /// Selects the provider credential cleared case.
    ProviderCredentialCleared {
        /// The request identifier.
        request_id: String,
        /// The instance.
        instance: String,
    },
    /// Selects the provider credential saved case.
    ProviderCredentialSaved {
        /// The request identifier.
        request_id: String,
        /// The instance.
        instance: String,
        /// The provider.
        provider: String,
    },
    /// Selects the pairing code case.
    PairingCode {
        /// The request identifier.
        request_id: String,
        /// The code.
        code: String,
        /// The expires at.
        expires_at: i64,
    },
    /// Selects the provider login started case.
    ProviderLoginStarted {
        /// The request identifier.
        request_id: String,
        /// The login identifier.
        login_id: String,
        /// The provider.
        provider: String,
        /// The verification URL.
        verification_url: String,
        /// The user code.
        user_code: String,
    },
    /// Selects the provider login finished case.
    ProviderLoginFinished {
        /// The request identifier.
        request_id: String,
        /// The login identifier.
        login_id: String,
        /// The provider.
        provider: String,
    },
    /// Selects the git credential status case.
    GitCredentialStatus {
        /// The request identifier.
        request_id: String,
        /// The available.
        available: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        /// The username.
        username: Option<String>,
    },
    /// Selects the ssh identities case.
    SshIdentities {
        /// The request identifier.
        request_id: String,
        /// The identities.
        identities: Vec<SshIdentityRecord>,
    },
    /// Selects the ssh identity generated case.
    SshIdentityGenerated {
        /// The request identifier.
        request_id: String,
        /// The identity.
        identity: SshIdentityRecord,
        /// The public key.
        public_key: String,
    },
    /// Selects the profile case.
    Profile {
        /// The request identifier.
        request_id: String,
        /// The profile.
        profile: ProfileSnapshot,
    },
    /// Selects the git diff case.
    GitDiff {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The scope.
        scope: GitDiffScope,
        /// The diff.
        diff: String,
    },
    /// Selects the directories case.
    Directories {
        /// The request identifier.
        request_id: String,
        /// The listing.
        listing: DirectoryListing,
    },
    /// Selects the workspace files case.
    WorkspaceFiles {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The files.
        files: Vec<WorkspaceFileRecord>,
        /// The truncated.
        truncated: bool,
    },
    /// Selects the workspace file chunk case.
    WorkspaceFileChunk {
        /// The request identifier.
        request_id: String,
        /// The session identifier.
        session_id: String,
        /// The path.
        path: String,
        /// The offset.
        offset: u64,
        #[serde(with = "base64_bytes")]
        /// The data.
        data: Vec<u8>,
        /// The next offset.
        next_offset: Option<u64>,
    },
    /// Selects the routines case.
    Routines {
        /// The request identifier.
        request_id: String,
        /// The routines.
        routines: Vec<Routine>,
    },
    /// Selects the routine history case.
    RoutineHistory {
        /// The request identifier.
        request_id: String,
        /// The runs.
        runs: Vec<RoutineRun>,
    },
    /// Selects the routine run preview case.
    RoutineRunPreview {
        /// The request identifier.
        request_id: String,
        /// The preview.
        preview: RoutineRunPreview,
    },
    /// Selects the error case.
    Error {
        /// The code.
        code: String,
        /// The message.
        message: String,
        /// The fatal.
        fatal: bool,
    },
}

/// One gateway response failure with its connection-level terminality preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatewayResponseError<'a> {
    /// The message.
    pub message: &'a str,
    /// The fatal.
    pub fatal: bool,
}

impl ServerMessage {
    /// Returns the error which terminates a wait for `request_id`, if any.
    ///
    /// A correlated rejection terminates that request. An uncorrelated global error
    /// only terminates a wait when it also terminates the connection.
    #[must_use]
    pub fn response_error(&self, request_id: Option<&str>) -> Option<GatewayResponseError<'_>> {
        match self {
            Self::Rejected {
                request_id: actual,
                message,
                fatal,
                ..
            } if request_id == Some(actual.as_str()) => Some(GatewayResponseError {
                message,
                fatal: *fatal,
            }),
            Self::Error {
                message,
                fatal: true,
                ..
            } => Some(GatewayResponseError {
                message,
                fatal: true,
            }),
            _ => None,
        }
    }
}
