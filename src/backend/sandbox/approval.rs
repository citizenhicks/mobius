use crate::SessionId;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use uuid::Uuid;

use super::NetworkAccess;
use super::SandboxApprovalRequest;
use super::SandboxAuthorization;
use super::SandboxMode;
use super::SandboxPermissions;
use crate::Error;
use crate::Result;
use crate::preview_json;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendBlockFormat;
use crate::protocol::FrontendBlockRole;
use crate::protocol::FrontendBlockState;
use crate::protocol::FrontendBlockUpdate;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendSlot;
use crate::protocol::FrontendTone;
use crate::protocol::FrontendWidget;
use crate::protocol::ReviewDecision;
use crate::protocol::ToolCall;
use crate::protocol::ToolContent;

const CAPABILITY: &str = "sandbox";
const MAX_SESSION_APPROVALS: usize = 64;

/// How approval-required tools receive execution authority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    /// Selects the ask case.
    Ask,
    /// Selects the allow case.
    Allow,
    /// Selects the allow network case.
    AllowNetwork,
    /// Selects the full access case.
    FullAccess,
}

impl ApprovalPolicy {
    fn network_access(self) -> NetworkAccess {
        match self {
            Self::Allow => NetworkAccess::Denied,
            // `Ask` cannot reach backend execution until per-call mutation approval is granted.
            Self::Ask | Self::AllowNetwork | Self::FullAccess => NetworkAccess::Allowed,
        }
    }

    fn sandbox_mode(self) -> SandboxMode {
        if self == Self::FullAccess {
            SandboxMode::DangerFullAccess
        } else {
            SandboxMode::WorkspaceWrite
        }
    }
}

impl FromStr for ApprovalPolicy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "ask" => Ok(Self::Ask),
            "allow" => Ok(Self::Allow),
            "allow_network" => Ok(Self::AllowNetwork),
            "full_access" => Ok(Self::FullAccess),
            _ => Err(Error::Config(format!(
                "unknown sandbox approval policy `{value}`"
            ))),
        }
    }
}

#[derive(Default)]
struct ApprovalState {
    approved_for_session: Arc<BTreeSet<[u8; 32]>>,
}

pub(super) struct Approval {
    default_policy: ApprovalPolicy,
    states: Mutex<BTreeMap<SessionId, ApprovalState>>,
}

impl Approval {
    pub(super) fn new(default_policy: ApprovalPolicy) -> Self {
        Self {
            default_policy,
            states: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) const fn policy(&self) -> ApprovalPolicy {
        self.default_policy
    }

    pub(super) fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: CAPABILITY.into(),
            count: None,
            count_label: None,
            commands: Vec::new(),
            widgets: vec![Arc::new(widget(self.default_policy))],
            references: Vec::new(),
        }
    }

    pub(super) fn render(event: &EventMsg) -> Option<FrontendBlock> {
        match event {
            EventMsg::ExecApprovalRequest(request) => Some(FrontendBlock {
                id: Some(approval_block_id(&request.turn_id, &request.id)),
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Pending,
                role: FrontendBlockRole::Approval,
                title: "Approval required".into(),
                text: format!("{}\n{}", request.reason, approval_tools(&request.calls)),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Warning,
                ..Default::default()
            }),
            // Append the outcome to the existing request, keeping its reason and tool details.
            EventMsg::ExecApprovalDecided(decided) => {
                let mut block = FrontendBlock {
                    id: Some(approval_block_id(&decided.turn_id, &decided.id)),
                    update: FrontendBlockUpdate::Append,
                    role: FrontendBlockRole::Approval,
                    ..Default::default()
                };
                block.title = match &decided.decision {
                    ReviewDecision::Approved => {
                        block.tone = FrontendTone::Success;
                        "Allowed once"
                    }
                    ReviewDecision::ApprovedForSession => {
                        block.tone = FrontendTone::Success;
                        "Allowed for this chat"
                    }
                    ReviewDecision::Denied { rejection } => {
                        block.content = ToolContent::from(rejection);
                        block.tone = FrontendTone::Warning;
                        "Denied"
                    }
                    ReviewDecision::Abort => "Stopped",
                }
                .into();
                Some(block)
            }
            _ => None,
        }
    }

    pub(super) fn session_start(&self, session_id: &SessionId) -> Result<Vec<FrontendEvent>> {
        self.states
            .lock()
            .map_err(state_lock_error)?
            .insert(session_id.share(), ApprovalState::default());
        Ok(vec![FrontendEvent::Widget {
            capability: CAPABILITY.into(),
            item: Arc::new(widget(self.default_policy)),
        }])
    }

    pub(super) fn authorize<'a>(
        &self,
        session_id: &SessionId,
        calls: &[ToolCall],
        mutation_call_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<SandboxAuthorization> {
        let approved_for_session = self
            .states
            .lock()
            .map_err(state_lock_error)?
            .get(session_id)
            .map(|state| Arc::clone(&state.approved_for_session))
            .ok_or_else(state_not_initialized)?;
        let policy = self.default_policy;
        let calls_by_id = calls
            .iter()
            .map(|call| (call.call_id.as_ref(), call))
            .collect::<BTreeMap<_, _>>();
        let mut approved = Vec::new();
        let mut requested = Vec::new();
        for call_id in mutation_call_ids {
            let call = calls_by_id
                .get(call_id)
                .ok_or_else(|| Error::Tool(format!("unknown mutation call `{call_id}`")))?;
            if policy != ApprovalPolicy::Ask
                || approved_for_session.contains(&call_key(session_id, call)?)
            {
                approved.push(Arc::clone(&call.call_id));
            } else {
                requested.push(Arc::clone(&call.call_id));
            }
        }
        let permissions = SandboxPermissions::new(
            session_id.share(),
            policy.sandbox_mode(),
            policy.network_access(),
            approved,
        );
        if requested.is_empty() {
            return Ok(SandboxAuthorization::Execute(permissions));
        }
        let request = SandboxApprovalRequest {
            id: Uuid::new_v4().to_string().into(),
            reason: "one or more tools require approval".into(),
            call_ids: requested,
        };
        Ok(SandboxAuthorization::Approval {
            request,
            permissions,
        })
    }

    pub(super) fn resolve(
        &self,
        session_id: &SessionId,
        calls: &[ToolCall],
        approval_call_ids: &[Arc<str>],
        decision: &ReviewDecision,
        mut permissions: SandboxPermissions,
    ) -> Result<SandboxPermissions> {
        if !matches!(
            decision,
            ReviewDecision::Approved | ReviewDecision::ApprovedForSession
        ) {
            return Ok(permissions);
        }
        let calls_by_id = calls
            .iter()
            .map(|call| (call.call_id.as_ref(), call))
            .collect::<BTreeMap<_, _>>();
        permissions.allow_mutations(approval_call_ids.iter().map(Arc::clone));
        if !matches!(decision, ReviewDecision::ApprovedForSession) {
            return Ok(permissions);
        }
        let keys = approval_call_ids
            .iter()
            .filter_map(|call_id| calls_by_id.get(&**call_id))
            .map(|call| call_key(session_id, call))
            .collect::<Result<Vec<_>>>()?;
        let mut states = self.states.lock().map_err(state_lock_error)?;
        let state = states
            .get_mut(session_id)
            .ok_or_else(state_not_initialized)?;
        // In-flight admissions retain the old snapshot; only approval changes may copy it.
        let approved = Arc::make_mut(&mut state.approved_for_session);
        for key in keys {
            if approved.len() >= MAX_SESSION_APPROVALS {
                approved.clear();
            }
            approved.insert(key);
        }
        drop(states);
        Ok(permissions)
    }

    pub(super) fn session_end(&self, session_id: &SessionId) -> Result<()> {
        self.states
            .lock()
            .map_err(state_lock_error)?
            .remove(session_id);
        Ok(())
    }
}

fn approval_tools(calls: &[crate::protocol::ApprovalCall]) -> String {
    calls
        .iter()
        .map(|call| format!("{} {}", call.name, preview_json(&call.arguments)))
        .collect::<Vec<_>>()
        .join("\n  ")
}

fn widget(policy: ApprovalPolicy) -> FrontendWidget {
    FrontendWidget {
        id: "approval_policy".into(),
        count: None,
        priority: crate::protocol::FrontendPriority::Primary,
        slot: FrontendSlot::Header,
        text: match policy {
            ApprovalPolicy::Ask => "approval ASK".into(),
            ApprovalPolicy::Allow => "approval ALLOW".into(),
            ApprovalPolicy::AllowNetwork => "approval NETWORK".into(),
            ApprovalPolicy::FullAccess => "approval FULL".into(),
        },
        tone: if policy == ApprovalPolicy::Ask {
            FrontendTone::Neutral
        } else {
            FrontendTone::Warning
        },
        symbol: None,
        icon_only: false,
        progress: None,
        content: None,
        action: None,
    }
}

fn call_key(session_id: &SessionId, call: &ToolCall) -> Result<[u8; 32]> {
    let value = serde_json::to_vec(&(session_id, &call.name, &call.arguments))?;
    Ok(Sha256::digest(value).into())
}

fn state_lock_error(error: impl std::fmt::Display) -> Error {
    Error::Stopped(format!("approval state lock poisoned: {error}"))
}

fn state_not_initialized() -> Error {
    Error::Stopped("approval state is not initialized".into())
}

/// The block an approval request and its decision share: `{turn}/approval:{request}`.
fn approval_block_id(turn_id: &str, request_id: &str) -> String {
    format!("{turn_id}/approval:{request_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_appends_its_outcome_to_the_request_block() {
        let request = Approval::render(&EventMsg::ExecApprovalRequest(
            crate::protocol::ExecApprovalRequestEvent {
                id: "request".into(),
                turn_id: "turn".into(),
                calls: Vec::new(),
                reason: "push".into(),
            },
        ))
        .expect("request block");
        let decided = |decision| {
            Approval::render(&EventMsg::ExecApprovalDecided(
                crate::protocol::ExecApprovalDecidedEvent {
                    id: "request".into(),
                    turn_id: "turn".into(),
                    decision,
                },
            ))
            .expect("decision block")
        };
        let denied = decided(ReviewDecision::Denied {
            rejection: "use the 1.99 toolchain".into(),
        });
        assert_eq!(request.id.as_deref(), Some("turn/approval:request"));
        assert_eq!(request.state, FrontendBlockState::Pending);
        assert_eq!(denied.id, request.id);
        assert_eq!(denied.state, FrontendBlockState::Complete);
        assert_eq!(denied.update, FrontendBlockUpdate::Append);
        assert_eq!(denied.title, "Denied");
        assert!(denied.text.is_empty());
        assert_eq!(denied.content, ToolContent::from("use the 1.99 toolchain"));
        assert_eq!(
            decided(ReviewDecision::ApprovedForSession).title,
            "Allowed for this chat"
        );
    }

    #[test]
    fn approval_grants_only_the_reviewed_call() {
        let approval = Approval::new(ApprovalPolicy::Ask);
        approval.states.lock().expect("approval state").insert(
            "session".parse().expect("session ID"),
            ApprovalState {
                approved_for_session: Arc::default(),
            },
        );
        let calls = [ToolCall {
            call_id: "write".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "a"}),
        }];
        let SandboxAuthorization::Approval {
            request,
            permissions,
        } = approval
            .authorize(&"session".parse().expect("session ID"), &calls, ["write"])
            .expect("authorization")
        else {
            panic!("approval required");
        };
        assert_eq!(
            (
                permissions.network_access(),
                permissions.for_call("write").mutation,
            ),
            (NetworkAccess::Allowed, false)
        );

        let permissions = approval
            .resolve(
                &"session".parse().expect("session ID"),
                &calls,
                &request.call_ids,
                &ReviewDecision::Approved,
                permissions,
            )
            .expect("resolution");
        assert!(permissions.for_call("write").mutation);
    }
}
