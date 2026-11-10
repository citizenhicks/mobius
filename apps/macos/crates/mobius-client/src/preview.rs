use mobius::SessionId;
use mobius::protocol::SessionFileRecord;
use mobius_gateway::wire::{
    ClientMessage, GitDiffScope, ServerMessage, WorkspaceFileRecord, WorkspaceFileScope,
};
use mobius_gateway::{Error, Result};

use super::files::TransferLink;

const MAX_PREVIEW_BYTES: usize = 16 * 1024 * 1024;
/// The gateway's largest workspace read.
const READ_CHUNK_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub enum PreviewRequest {
    File(String),
    Files,
    Diff(GitDiffScope),
    /// The files stored with the chat: those sent to the agent and those it sent back.
    ChatFiles,
}

#[derive(Debug)]
pub enum PreviewData {
    File {
        path: String,
        bytes: Vec<u8>,
    },
    Files {
        files: Vec<WorkspaceFileRecord>,
        truncated: bool,
    },
    Diff {
        scope: GitDiffScope,
        text: String,
    },
    ChatFiles(Vec<SessionFileRecord>),
}

pub(super) async fn load(
    link: &TransferLink,
    session_id: &SessionId,
    workspace: Option<&std::path::Path>,
    request: &PreviewRequest,
) -> Result<PreviewData> {
    if !matches!(request, PreviewRequest::ChatFiles) && workspace.is_none() {
        return Err(Error::Protocol(
            "This conversation has no workspace to preview.".into(),
        ));
    }
    let response = match request {
        PreviewRequest::File(path) => {
            let path = std::path::Path::new(path);
            // Inside the folder by relative path; anywhere else by absolute path.
            let relative = workspace
                .and_then(|workspace| path.strip_prefix(workspace).ok())
                .unwrap_or(path);
            return read_file(link, session_id, &relative.to_string_lossy()).await;
        }
        PreviewRequest::Files => {
            link.request(|request_id| ClientMessage::ListWorkspaceFiles {
                request_id,
                session_id: session_id.share(),
                scope: WorkspaceFileScope::All,
            })
            .await
        }
        PreviewRequest::Diff(scope) => {
            link.request(|request_id| ClientMessage::GetGitDiff {
                request_id,
                session_id: session_id.share(),
                scope: *scope,
                totals: false,
            })
            .await
        }
        PreviewRequest::ChatFiles => {
            link.request(|request_id| ClientMessage::ListSessionFiles {
                request_id,
                session_id: session_id.share(),
            })
            .await
        }
    };
    match response? {
        ServerMessage::WorkspaceFiles {
            files, truncated, ..
        } => Ok(PreviewData::Files { files, truncated }),
        ServerMessage::GitDiff { scope, diff, .. } => Ok(PreviewData::Diff { scope, text: diff }),
        ServerMessage::SessionFiles { files, .. } => Ok(PreviewData::ChatFiles(files)),
        _ => Err(Error::Protocol(
            "Gateway returned a different preview.".into(),
        )),
    }
}

async fn read_file(link: &TransferLink, session_id: &SessionId, path: &str) -> Result<PreviewData> {
    let mut bytes = Vec::new();
    loop {
        let offset = bytes.len() as u64;
        let response = link
            .request(|request_id| ClientMessage::ReadWorkspaceFile {
                request_id,
                session_id: session_id.share(),
                path: path.into(),
                offset,
                max_bytes: READ_CHUNK_BYTES,
            })
            .await?;
        let ServerMessage::WorkspaceFileChunk {
            data, next_offset, ..
        } = response
        else {
            return Err(Error::Protocol(
                "Gateway returned an unexpected file preview.".into(),
            ));
        };
        bytes.extend(data);
        if bytes.len() > MAX_PREVIEW_BYTES {
            return Err(Error::Protocol(
                "This file exceeds the 16 MiB preview limit.".into(),
            ));
        }
        if next_offset.is_none() {
            return Ok(PreviewData::File {
                path: path.into(),
                bytes,
            });
        }
    }
}
