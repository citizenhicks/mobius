use mobius::SessionId;
use mobius::protocol::SessionFileRecord;
use mobius_gateway::wire::{
    ClientMessage, GitDiffScope, MAX_READ_CHUNK_BYTES, ServerMessage, WorkspaceFileRecord,
    WorkspaceFileScope,
};
use mobius_gateway::{Error, Result};

use super::files::TransferLink;

const MAX_PREVIEW_BYTES: usize = 16 * 1024 * 1024;

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
        truncated: bool,
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
        ServerMessage::GitDiff {
            scope,
            diff,
            truncated,
            ..
        } => Ok(PreviewData::Diff {
            scope,
            text: diff,
            truncated,
        }),
        ServerMessage::SessionFiles { files, .. } => Ok(PreviewData::ChatFiles(files)),
        _ => Err(Error::Protocol(
            "Gateway returned a different preview.".into(),
        )),
    }
}

async fn read_file(link: &TransferLink, session_id: &SessionId, path: &str) -> Result<PreviewData> {
    use futures_util::StreamExt as _;
    // The size is unknown until the last chunk; one read past the limit tells an
    // oversized file from one that ends exactly at it.
    let read = |request_id, offset, max_bytes| ClientMessage::ReadWorkspaceFile {
        request_id,
        session_id: session_id.share(),
        path: path.into(),
        offset,
        max_bytes,
    };
    // Most source files fit in one chunk. Do not speculate beyond their end.
    let first = futures_util::stream::once(
        link.request(|request_id| read(request_id, 0, MAX_READ_CHUNK_BYTES)),
    );
    let remaining = super::files::chunk_reads(
        link,
        MAX_READ_CHUNK_BYTES as u64,
        MAX_PREVIEW_BYTES as u64 + 1,
        read,
    );
    let reads = first.chain(remaining);
    let mut reads = std::pin::pin!(reads);
    let mut bytes = Vec::new();
    while let Some(response) = reads.next().await {
        let ServerMessage::WorkspaceFileChunk {
            data, next_offset, ..
        } = response?
        else {
            return Err(Error::Protocol(
                "Gateway returned an unexpected file preview.".into(),
            ));
        };
        bytes.extend(data);
        if bytes.len() > MAX_PREVIEW_BYTES {
            break;
        }
        if next_offset.is_none() {
            return Ok(PreviewData::File {
                path: path.into(),
                bytes,
            });
        }
    }
    Err(Error::Protocol(
        "This file exceeds the 16 MiB preview limit.".into(),
    ))
}
