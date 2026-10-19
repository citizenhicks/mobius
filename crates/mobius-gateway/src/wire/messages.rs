use std::sync::Arc;

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
    /// Opens the gateway-owned computer and foregrounds the chat's assigned tab.
    OpenComputer {
        /// The request identifier.
        request_id: Arc<str>,
        /// The chat whose assigned browser tab to show.
        session_id: String,
    },
    /// Opens or closes this authenticated connection's remote desktop stream.
    SetDesktopStream {
        /// The request identifier.
        request_id: Arc<str>,
        /// Whether to stream the desktop.
        enabled: bool,
    },
    /// Ordered RFB bytes for this connection's desktop stream.
    DesktopData {
        /// The raw RFB data, limited to [`MAX_DESKTOP_CHUNK_BYTES`].
        #[serde(with = "desktop_bytes")]
        data: Vec<u8>,
    },
    /// Requests or releases the gateway's human desktop lease.
    SetDesktopControl {
        /// The request identifier.
        request_id: Arc<str>,
        /// Whether to take control.
        enabled: bool,
        /// The chat whose computer view requested control.
        session_id: Option<String>,
    },
    /// Replaces this connection's optional notification exclusions.
    SetNotifications {
        /// The request identifier.
        request_id: Arc<str>,
        /// Broadcasts to suppress until replaced or the connection closes.
        disabled: BTreeSet<GatewayNotification>,
    },
    /// Selects a chat for file and workspace requests without replaying or streaming
    /// its events; answered by `session_opened` alone. Selecting loads a stopped chat
    /// and runs its start-up, so history reads should not select first.
    SelectSession {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
    },
    /// Asks for the line totals of a Git diff, as Git counts them.
    GetGitDiffTotals {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The scope.
        scope: GitDiffScope,
    },
    /// Registers the local Mac renderer for gateway-assigned browser pages.
    SetBrowserRuntime {
        /// The request identifier.
        request_id: Arc<str>,
        /// Whether the renderer is available.
        enabled: bool,
    },
    /// Supplies the scoped Unix endpoint of an assigned local browser page.
    BrowserPageReply {
        /// The gateway's page request identifier.
        request_id: Arc<str>,
        /// The endpoint, or none if the renderer cannot provide the page.
        endpoint: Option<String>,
    },
    /// Selects the set desktop runtime case.
    SetDesktopRuntime {
        /// The request identifier.
        request_id: Arc<str>,
        /// The enabled.
        enabled: bool,
    },
    /// Selects the desktop control reply case.
    DesktopControlReply {
        /// The request identifier.
        request_id: Arc<str>,
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
        /// The catalog this client already holds, or does not need. An empty hint is left
        /// out, so the version check that replaces an older running gateway can reach it.
        #[serde(default, skip_serializing_if = "CatalogHint::is_empty")]
        catalog: CatalogHint,
    },
    /// Selects the list clients case.
    ListClients {
        /// The request identifier.
        request_id: Arc<str>,
    },
    /// Read collector configuration and delivery status.
    GetTelemetry {
        /// Correlates this response with its request.
        request_id: Arc<str>,
    },
    /// Read storage ownership and measured sizes without mutating files.
    GetStorageUsage {
        /// Correlates this response with its request.
        request_id: Arc<str>,
    },
    /// Replace collector configuration at an expected revision.
    ConfigureTelemetry {
        /// Correlates this response with its request.
        request_id: Arc<str>,
        /// Configuration revision observed before editing.
        expected_revision: u64,
        /// Configured collectors.
        sinks: Vec<crate::telemetry::TelemetrySink>,
        /// IDs whose existing credential sources must survive a redacted report edit.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preserve_auth: Vec<String>,
    },
    /// Deliver a snapshot immediately.
    SendTelemetry {
        /// Correlates this response with its request.
        request_id: Arc<str>,
        /// Collector to send now.
        sink_id: String,
    },
    /// Selects the unpair client case.
    UnpairClient {
        /// The request identifier.
        request_id: Arc<str>,
        /// The client identifier.
        client_id: String,
    },
    /// Selects the list sessions case.
    ListSessions {
        /// The request identifier.
        request_id: Arc<str>,
    },
    /// Selects the list bot sessions case.
    ListBotSessions {
        /// The request identifier.
        request_id: Arc<str>,
        /// The bot identifier.
        bot_id: Arc<str>,
    },
    /// Selects the create session case.
    CreateSession {
        /// The request identifier.
        request_id: Arc<str>,
        /// The workspace.
        workspace: PathBuf,
        /// The bot identifier.
        bot_id: Arc<str>,
    },
    /// Selects the create workspace directory case.
    CreateWorkspaceDirectory {
        /// The request identifier.
        request_id: Arc<str>,
        /// The parent.
        parent: PathBuf,
        /// The name.
        name: String,
    },
    /// Selects the open session case.
    OpenSession {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: Arc<str>,
        /// The last sequence.
        last_sequence: Option<u64>,
    },
    /// Requests one complete durable turn page of any chat this client could select;
    /// answered by `session_history`. The selected chat answers itself. Any other chat
    /// is read without selecting, loading or starting it; when it is not running,
    /// records carry only framework-rendered blocks, not middleware-rendered ones.
    GetSessionHistory {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The before sequence.
        before_sequence: Option<u64>,
    },
    /// Selects the reassign session case.
    ReassignSession {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The bot identifier.
        bot_id: Arc<str>,
    },
    /// Selects the rename session case.
    RenameSession {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The title.
        title: String,
    },
    /// Selects the attach session folder case.
    AttachSessionFolder {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The folder.
        folder: PathBuf,
    },
    /// Selects the set session pinned case.
    SetSessionPinned {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The pinned.
        pinned: bool,
    },
    /// Pins or unpins one assistant message in the opened chat.
    SetMessagePinned {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The assistant message.
        target: MessageTarget,
        /// The pinned.
        pinned: bool,
    },
    /// Selects the delete sessions case.
    DeleteSessions {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifiers.
        session_ids: Vec<String>,
        /// Required deletion scope; history is removed only for all.
        selection: mobius::backend::session_files::SessionFileSelection,
    },
    /// Selects the submit case.
    Submit {
        /// The session identifier.
        session_id: Arc<str>,
        /// The submission.
        submission: Submission,
    },
    /// Selects the start realtime voice case.
    StartRealtimeVoice {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The offer sdp.
        offer_sdp: String,
    },
    /// Selects the end realtime voice case.
    EndRealtimeVoice {
        /// The session identifier.
        session_id: Arc<str>,
        /// The voice identifier.
        voice_id: Arc<str>,
    },
    /// Selects the get contributions case.
    GetContributions {
        /// The request identifier.
        request_id: Arc<str>,
    },
    /// Selects the submit contribution case.
    SubmitContribution {
        /// The request identifier.
        request_id: Arc<str>,
        /// The operation.
        operation: Op,
    },
    /// Selects the begin session file upload case.
    BeginSessionFileUpload {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The upload identifier.
        upload_id: String,
    },
    /// Selects the list session files case.
    ListSessionFiles {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
    },
    /// Selects the read session file case.
    ReadSessionFile {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The name.
        name: String,
        /// The description.
        description: String,
    },
    /// Selects the list bots case.
    ListBots {
        /// The request identifier.
        request_id: Arc<str>,
    },
    /// Selects the update bot case.
    UpdateBot {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identifier.
        id: Arc<str>,
        /// The expected revision.
        expected_revision: u64,
        /// The name.
        name: Arc<str>,
        /// The description.
        description: Arc<str>,
        /// The tint.
        tint: ProviderTint,
        /// The face's silhouette.
        shape: BotShape,
        /// The config.
        config: AgentComposition,
    },
    /// Selects the delete bot case.
    DeleteBot {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identifier.
        id: Arc<str>,
        /// The expected revision.
        expected_revision: u64,
    },
    /// Selects the configure bot defaults case.
    ConfigureBotDefaults {
        /// The request identifier.
        request_id: Arc<str>,
        /// The expected revision.
        expected_revision: u64,
        /// The config.
        config: AgentComposition,
    },
    /// Selects the install extension case.
    InstallExtension {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The identifier.
        id: Arc<str>,
    },
    /// Selects the uninstall extension case.
    UninstallExtension {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identifier.
        id: Arc<str>,
    },
    /// Selects the trust extension hooks case.
    TrustExtensionHooks {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identifier.
        id: Arc<str>,
        /// The expected digest.
        expected_digest: Arc<str>,
    },
    /// Selects the revoke extension hooks trust case.
    RevokeExtensionHooksTrust {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identifier.
        id: Arc<str>,
        /// The expected digest.
        expected_digest: Arc<str>,
    },
    /// Selects the probe git credential case.
    ProbeGitCredential {
        /// The request identifier.
        request_id: Arc<str>,
        /// The target.
        target: String,
    },
    /// Selects the approve git credential case.
    ApproveGitCredential {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
    },
    /// Selects the generate ssh identity case.
    GenerateSshIdentity {
        /// The request identifier.
        request_id: Arc<str>,
    },
    /// Selects the get git diff case.
    GetGitDiff {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The scope.
        scope: GitDiffScope,
    },
    /// Selects the switch git branch case.
    SwitchGitBranch {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The branch.
        branch: String,
    },
    /// Selects the list directories case.
    ListDirectories {
        /// The request identifier.
        request_id: Arc<str>,
        /// The path.
        path: PathBuf,
        /// The include files.
        include_files: bool,
    },
    /// Selects the list workspace files case.
    ListWorkspaceFiles {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The scope.
        scope: WorkspaceFileScope,
    },
    /// Selects the read workspace file case.
    ReadWorkspaceFile {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The path.
        path: String,
        /// The offset.
        offset: u64,
        /// The max bytes.
        max_bytes: usize,
    },
    /// Deletes a file inside the selected workspace.
    DeleteWorkspaceFile {
        /// Request correlation identifier.
        request_id: Arc<str>,
        /// Selected project chat.
        session_id: String,
        /// Relative regular file path.
        path: String,
    },
    /// Writes one workspace text file.
    WriteWorkspaceFile {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The instance.
        instance: String,
    },
    /// Selects the set provider credential case.
    SetProviderCredential {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The config.
        config: ProviderConfig,
        /// Retain live catalogs, labels and model/search selection during an endpoint or credential refresh.
        #[serde(default)]
        preserve_selection: bool,
        /// Leave an uninstalled optional setup unconfigured.
        #[serde(default)]
        if_configured: bool,
        /// Omission retains the live label, or uses the provider label for a new setup.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Omission retains the live tint, or uses the default tint for a new setup.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tint: Option<ProviderTint>,
        /// An empty or omitted list retains existing models, or seeds a new setup.
        #[serde(default)]
        models: Vec<ConfiguredModel>,
        /// Omission retains live image models; an explicit empty list clears them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image_model_ids: Option<Vec<String>>,
    },
    /// Selects the remove provider case.
    RemoveProvider {
        /// The request identifier.
        request_id: Arc<str>,
        /// The instance.
        instance: String,
    },
    /// Selects the create pairing code case.
    CreatePairingCode {
        /// The request identifier.
        request_id: Arc<str>,
    },
    /// Selects the start provider login case.
    StartProviderLogin {
        /// The request identifier.
        request_id: Arc<str>,
        /// The provider.
        provider: String,
    },
    /// Selects the get profile case.
    GetProfile {
        /// The request identifier.
        request_id: Arc<str>,
        /// The include provider usage.
        include_provider_usage: bool,
    },
    /// Create a validated routine definition.
    CreateRoutine {
        /// Request identity.
        request_id: Arc<str>,
        /// Owning Bot.
        bot_id: Arc<str>,
        /// Editable content and exact bindings.
        definition: RoutineDefinition,
    },
    /// List the current routine definitions.
    ListRoutines {
        /// Request identity.
        request_id: Arc<str>,
        /// Optional owning Bot.
        bot_id: Option<Arc<str>>,
    },
    /// Apply the same typed command used by timers and hooks.
    RoutineCommand {
        /// Request and command identity.
        request_id: Arc<str>,
        /// Target and action.
        command: super::RoutineCommand,
    },
    /// Selects the list routine history case.
    ListRoutineHistory {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identifier.
        id: Option<Arc<str>>,
    },
    /// Selects the delete routine run case.
    DeleteRoutineRun {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identifier.
        id: Arc<str>,
    },
    /// Selects the get routine run preview case.
    GetRoutineRunPreview {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identifier.
        id: Arc<str>,
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
    /// Acknowledges showing the gateway-owned computer.
    ComputerOpened {
        /// The request identifier.
        request_id: Arc<str>,
        /// The chat whose browser tab was shown.
        session_id: String,
    },
    /// Acknowledges opening or closing this connection's desktop stream.
    DesktopStreamState {
        /// The request identifier.
        request_id: Arc<str>,
        /// Whether the stream is open.
        enabled: bool,
    },
    /// Ordered RFB bytes from the gateway desktop.
    DesktopData {
        /// The raw RFB data, limited to [`MAX_DESKTOP_CHUNK_BYTES`].
        #[serde(with = "desktop_bytes")]
        data: Vec<u8>,
    },
    /// The gateway's current desktop lease, relative to this connection.
    DesktopControlState {
        /// The acknowledged request, or absent for a broadcast update.
        request_id: Option<Arc<str>>,
        /// Whether execution is held for pending or granted human control.
        enabled: bool,
        /// Whether this connection owns control.
        is_owner: bool,
        /// The controlling chat, or the latest agent chat when watching.
        session_id: Option<Arc<str>>,
    },
    /// A committed source changed; clients refresh their existing snapshots.
    HookEvent {
        /// Committed typed lifecycle fact.
        event: super::HookEvent,
    },
    /// Fresh measured storage usage.
    StorageUsage {
        /// Correlates this response with its request.
        request_id: Arc<str>,
        /// Measured storage ownership and totals.
        usage: crate::storage_usage::StorageUsage,
    },
    /// Collector configuration and delivery state.
    Telemetry {
        /// Correlates this response with its request.
        request_id: Arc<str>,
        /// Current collector configuration revision.
        revision: u64,
        /// Configured collectors.
        sinks: Vec<crate::telemetry::TelemetrySinkReport>,
    },
    /// Requests the local renderer's assigned page for one chat.
    BrowserPageRequested {
        /// The request identifier.
        request_id: Arc<str>,
        /// The chat identifier.
        session_id: String,
        /// Whether the human explicitly asked to activate this page.
        foreground: bool,
    },
    /// Selects the desktop control requested case.
    DesktopControlRequested {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The payload.
        payload: SessionReadyPayload,
    },
    /// Selects the session replay complete case.
    SessionReplayComplete {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
    },
    /// Selects the session history case.
    SessionHistory {
        /// The request identifier.
        request_id: Arc<str>,
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
    /// Replaces the opened chat's message pins without refreshing its runtime state.
    MessagePinsChanged {
        /// The session identifier.
        session_id: std::sync::Arc<str>,
        /// Pinned assistant messages, in transcript order.
        pinned_messages: BTreeSet<MessageTarget>,
    },
    /// Selects the realtime voice started case.
    RealtimeVoiceStarted {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: Arc<str>,
        /// The voice identifier.
        voice_id: Arc<str>,
        /// The answer sdp.
        answer_sdp: String,
    },
    /// Selects the realtime voice failed case.
    RealtimeVoiceFailed {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: Arc<str>,
        /// The message.
        message: Arc<str>,
    },
    /// Selects the realtime voice ended case.
    RealtimeVoiceEnded {
        /// The session identifier.
        session_id: Arc<str>,
        /// The voice identifier.
        voice_id: Arc<str>,
        /// The reason.
        reason: Option<String>,
    },
    /// Selects the gateway configured case.
    GatewayConfigured {
        /// The request identifier.
        request_id: Arc<str>,
        /// The payload.
        payload: ReadyPayload,
    },
    /// Selects the contributions case.
    Contributions {
        /// The request identifier.
        request_id: Arc<str>,
        /// The contributions.
        contributions: Vec<FrontendContribution>,
    },
    /// Selects the accepted case.
    Accepted {
        /// The request identifier.
        request_id: Arc<str>,
    },
    /// Selects the session file upload ready case.
    SessionFileUploadReady {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The file.
        file: SessionFileReference,
    },
    /// Selects the session files case.
    SessionFiles {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The files.
        files: Vec<SessionFileRecord>,
    },
    /// Selects the session file chunk case.
    SessionFileChunk {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The code.
        code: String,
        /// The message.
        message: Arc<str>,
        /// The fatal.
        fatal: bool,
    },
    /// Selects the agent event case.
    AgentEvent {
        /// Shared across the token events retained by a running session.
        session_id: std::sync::Arc<str>,
        /// The record.
        record: RecordedEvent,
    },
    /// Selects the sessions case.
    Sessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The request identifier.
        request_id: Option<Arc<str>>,
        /// The sessions.
        sessions: Vec<SessionRecord>,
    },
    /// Selects the background approvals case.
    BackgroundApprovals {
        /// The approvals.
        approvals: Vec<Arc<BackgroundApproval>>,
    },
    /// The whole visible session catalog, in order, relative to the catalog this
    /// connection last received; unsolicited catalog updates arrive only in this form.
    SessionsChanged {
        /// Every session of the catalog, unchanged ones by ID.
        sessions: Vec<SessionSlot>,
        /// The new catalog's revision.
        revision: String,
    },

    /// Selects the bot sessions case.
    BotSessions {
        /// The request identifier.
        request_id: Arc<str>,
        /// The bot identifier.
        bot_id: Arc<str>,
        /// The sessions.
        sessions: Vec<SessionRecord>,
    },
    /// Selects the bots case.
    Bots {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The request identifier.
        request_id: Option<Arc<str>>,
        /// The bots.
        bots: Vec<BotRecord>,
    },

    /// Selects the clients case.
    Clients {
        /// The request identifier.
        request_id: Arc<str>,
        /// The current client identifier.
        current_client_id: String,
        /// The clients.
        clients: Vec<ClientStatus>,
    },
    /// Selects the provider credential cleared case.
    ProviderCredentialCleared {
        /// The request identifier.
        request_id: Arc<str>,
        /// The instance.
        instance: String,
    },
    /// Selects the provider credential saved case.
    ProviderCredentialSaved {
        /// The request identifier.
        request_id: Arc<str>,
        /// The instance.
        instance: String,
        /// The provider.
        provider: String,
    },
    /// Selects the pairing code case.
    PairingCode {
        /// The request identifier.
        request_id: Arc<str>,
        /// The code.
        code: String,
        /// The expires at.
        expires_at: i64,
    },
    /// Selects the provider login started case.
    ProviderLoginStarted {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The login identifier.
        login_id: String,
        /// The provider.
        provider: String,
    },
    /// Selects the git credential status case.
    GitCredentialStatus {
        /// The request identifier.
        request_id: Arc<str>,
        /// The available.
        available: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        /// The username.
        username: Option<String>,
    },
    /// Selects the ssh identities case.
    SshIdentities {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identities.
        identities: Vec<SshIdentityRecord>,
    },
    /// Selects the ssh identity generated case.
    SshIdentityGenerated {
        /// The request identifier.
        request_id: Arc<str>,
        /// The identity.
        identity: SshIdentityRecord,
        /// The public key.
        public_key: String,
    },
    /// Selects the profile case.
    Profile {
        /// The request identifier.
        request_id: Arc<str>,
        /// The profile.
        profile: ProfileSnapshot,
    },
    /// Selects the git diff case.
    GitDiff {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The scope.
        scope: GitDiffScope,
        /// The diff.
        diff: String,
    },
    /// Line totals of a Git diff.
    GitDiffTotals {
        /// The request identifier.
        request_id: Arc<str>,
        /// The session identifier.
        session_id: String,
        /// The scope.
        scope: GitDiffScope,
        /// The totals.
        totals: DiffTotals,
    },
    /// Selects the directories case.
    Directories {
        /// The request identifier.
        request_id: Arc<str>,
        /// The listing.
        listing: DirectoryListing,
    },
    /// Selects the workspace files case.
    WorkspaceFiles {
        /// The request identifier.
        request_id: Arc<str>,
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
        request_id: Arc<str>,
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
        request_id: Arc<str>,
        /// The routines.
        routines: Vec<Routine>,
    },
    /// Selects the routine history case.
    RoutineHistory {
        /// The request identifier.
        request_id: Arc<str>,
        /// The runs.
        runs: Vec<RoutineRun>,
    },
    /// Selects the routine run preview case.
    RoutineRunPreview {
        /// The request identifier.
        request_id: Arc<str>,
        /// The preview.
        preview: RoutineRunPreview,
    },
    /// Selects the error case.
    Error {
        /// The code.
        code: String,
        /// The message.
        message: Arc<str>,
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
            } if request_id == Some(&**actual) => Some(GatewayResponseError {
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
