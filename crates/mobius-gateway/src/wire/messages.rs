use super::*;

/// One client-to-gateway frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientFrame {
    pub version: u16,
    #[serde(flatten)]
    pub message: ClientMessage,
}

impl<'de> Deserialize<'de> for ClientFrame {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (version, message) = deserialize_frame(deserializer)?;
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
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientMessage {
    Pair {
        code: String,
        client_label: String,
        client_kind: ClientKind,
    },
    Authenticate {
        token: String,
        client_kind: ClientKind,
    },
    ListClients {
        request_id: String,
    },
    UnpairClient {
        request_id: String,
        client_id: String,
    },
    ListSessions {
        request_id: String,
    },
    CreateSession {
        request_id: String,
        workspace: PathBuf,
    },
    CreateWorkspaceDirectory {
        request_id: String,
        parent: PathBuf,
        name: String,
    },
    OpenSession {
        request_id: String,
        session_id: String,
        last_sequence: Option<u64>,
    },
    GetSessionHistory {
        request_id: String,
        session_id: String,
        before_sequence: Option<u64>,
    },
    RenameSession {
        request_id: String,
        session_id: String,
        title: String,
    },
    SetSessionPinned {
        request_id: String,
        session_id: String,
        pinned: bool,
    },
    DeleteSession {
        request_id: String,
        session_id: String,
    },
    Submit {
        session_id: String,
        submission: Submission,
    },
    SubmitGlobalScratchpad {
        request_id: String,
        operation: Op,
    },
    BeginSessionFileUpload {
        request_id: String,
        session_id: String,
        name: String,
        size: u64,
        media_type: String,
    },
    UploadSessionFileChunk {
        request_id: String,
        session_id: String,
        upload_id: String,
        offset: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    FinishSessionFileUpload {
        request_id: String,
        session_id: String,
        upload_id: String,
    },
    ListSessionFiles {
        request_id: String,
        session_id: String,
    },
    ReadSessionFile {
        request_id: String,
        session_id: String,
        file_id: String,
        offset: u64,
        max_bytes: usize,
    },
    ConfigureSession {
        request_id: String,
        session_id: String,
        expected_revision: u64,
        config: AgentComposition,
    },
    ConfigureDefaultAgent {
        request_id: String,
        expected_revision: u64,
        config: AgentComposition,
    },
    InstallExtension {
        request_id: String,
        source: String,
        reference: Option<String>,
        subdirectory: Option<String>,
    },
    UpdateExtension {
        request_id: String,
        id: String,
    },
    UninstallExtension {
        request_id: String,
        id: String,
    },
    TrustExtensionHooks {
        request_id: String,
        id: String,
        expected_digest: String,
    },
    RevokeExtensionHooksTrust {
        request_id: String,
        id: String,
        expected_digest: String,
    },
    ProbeGitCredential {
        request_id: String,
        target: String,
    },
    ApproveGitCredential {
        request_id: String,
        target: String,
        username: String,
        token: String,
    },
    ListSshIdentities {
        request_id: String,
    },
    GenerateSshIdentity {
        request_id: String,
    },
    GetGitDiff {
        request_id: String,
        session_id: String,
        scope: GitDiffScope,
    },
    SwitchGitBranch {
        request_id: String,
        session_id: String,
        branch: String,
    },
    ListDirectories {
        request_id: String,
        path: PathBuf,
        include_files: bool,
    },
    ListWorkspaceFiles {
        request_id: String,
        session_id: String,
        scope: WorkspaceFileScope,
    },
    ReadWorkspaceFile {
        request_id: String,
        session_id: String,
        path: String,
        offset: u64,
        max_bytes: usize,
    },
    WriteWorkspaceFile {
        request_id: String,
        session_id: String,
        path: String,
        content: String,
    },
    SetProviderCredential {
        request_id: String,
        instance: String,
        provider: String,
        api_key: String,
    },
    SetProviderEndpointCredential {
        request_id: String,
        instance: String,
        provider: String,
        base_url: String,
        api_key: String,
    },
    RegisterProvider {
        request_id: String,
        config: ProviderConfig,
        label: String,
        tint: ProviderTint,
        model_ids: Vec<String>,
        reasoning_efforts: Vec<String>,
        replace_existing_selections: bool,
    },
    RemoveProvider {
        request_id: String,
        instance: String,
    },
    CreatePairingCode {
        request_id: String,
    },
    StartProviderLogin {
        request_id: String,
        provider: String,
    },
    GetProfile {
        request_id: String,
    },
    CreateCron {
        request_id: String,
        source_session_id: String,
        task: String,
        schedule: CronSchedule,
        ends_at: Option<i64>,
    },
    ListCron {
        request_id: String,
    },
    UpdateCron {
        request_id: String,
        id: String,
        source_session_id: String,
        task: String,
        schedule: CronSchedule,
        ends_at: Option<i64>,
        enabled: bool,
    },
    DeleteCron {
        request_id: String,
        id: String,
    },
    RunCron {
        request_id: String,
        id: String,
    },
    ListCronHistory {
        request_id: String,
        id: Option<String>,
    },
    GetCronRunPreview {
        request_id: String,
        id: String,
        before_sequence: Option<u64>,
    },
}

/// One gateway-to-client frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerFrame {
    pub version: u16,
    #[serde(flatten)]
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
    Paired {
        client_id: String,
        token: String,
    },
    Authenticated,
    Ready {
        payload: ReadyPayload,
    },
    SessionOpened {
        request_id: String,
        payload: SessionReadyPayload,
    },
    SessionReplayComplete {
        request_id: String,
        session_id: String,
    },
    SessionHistory {
        request_id: String,
        session_id: String,
        records: Vec<RecordedEvent>,
        next_before_sequence: Option<u64>,
    },
    SessionChanged {
        payload: SessionReadyPayload,
    },
    GatewayConfigured {
        request_id: String,
        payload: ReadyPayload,
    },
    GlobalScratchpadChanged {
        request_id: String,
        contribution: FrontendContribution,
    },
    Accepted {
        request_id: String,
    },
    SessionFileUploadReady {
        request_id: String,
        session_id: String,
        upload_id: String,
        max_chunk_bytes: usize,
    },
    SessionFileUploadChunkAccepted {
        request_id: String,
        session_id: String,
        upload_id: String,
        next_offset: u64,
    },
    SessionFileUploadCompleted {
        request_id: String,
        session_id: String,
        file: SessionFileReference,
    },
    SessionFiles {
        request_id: String,
        session_id: String,
        files: Vec<SessionFileRecord>,
    },
    SessionFileChunk {
        request_id: String,
        session_id: String,
        file_id: String,
        offset: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        next_offset: Option<u64>,
    },
    Rejected {
        request_id: String,
        code: String,
        message: String,
        fatal: bool,
    },
    AgentEvent {
        session_id: String,
        record: RecordedEvent,
    },
    Sessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        sessions: Vec<SessionRecord>,
    },
    Clients {
        request_id: String,
        current_client_id: String,
        clients: Vec<ClientStatus>,
    },
    ProviderCredentialSaved {
        request_id: String,
        instance: String,
        provider: String,
    },
    PairingCode {
        request_id: String,
        code: String,
        expires_at: i64,
    },
    ProviderLoginStarted {
        request_id: String,
        login_id: String,
        provider: String,
        verification_url: String,
        user_code: String,
    },
    ProviderLoginFinished {
        request_id: String,
        login_id: String,
        provider: String,
    },
    GitCredentialStatus {
        request_id: String,
        available: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<String>,
    },
    SshIdentities {
        request_id: String,
        identities: Vec<SshIdentityRecord>,
    },
    SshIdentityGenerated {
        request_id: String,
        identity: SshIdentityRecord,
        public_key: String,
    },
    Profile {
        request_id: String,
        profile: ProfileSnapshot,
    },
    GitDiff {
        request_id: String,
        session_id: String,
        scope: GitDiffScope,
        diff: String,
    },
    Directories {
        request_id: String,
        listing: DirectoryListing,
    },
    WorkspaceFiles {
        request_id: String,
        session_id: String,
        files: Vec<WorkspaceFileRecord>,
        truncated: bool,
    },
    WorkspaceFileChunk {
        request_id: String,
        session_id: String,
        path: String,
        offset: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        next_offset: Option<u64>,
    },
    CronTasks {
        request_id: String,
        tasks: Vec<CronTask>,
    },
    CronHistory {
        request_id: String,
        runs: Vec<CronRun>,
    },
    CronRunPreview {
        request_id: String,
        preview: CronRunPreview,
    },
    Error {
        code: String,
        message: String,
        fatal: bool,
    },
}
