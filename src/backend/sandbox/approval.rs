use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Mutex;

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
use crate::backend::model::ToolCall;
use crate::preview_json;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendSlot;
use crate::protocol::FrontendTone;
use crate::protocol::FrontendWidget;
use crate::protocol::ReviewDecision;

const CAPABILITY: &str = "sandbox";
const MAX_SESSION_APPROVALS: usize = 64;

/// How approval-required tools receive execution authority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    Ask,
    Allow,
    AllowNetwork,
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
    approved_for_session: BTreeSet<[u8; 32]>,
}

pub(super) struct Approval {
    default_policy: ApprovalPolicy,
    states: Mutex<BTreeMap<String, ApprovalState>>,
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
            accepts_file_attachments: false,
            count: None,
            commands: Vec::new(),
            widgets: vec![widget(self.default_policy)],
            references: Vec::new(),
        }
    }

    pub(super) fn render(&self, event: &EventMsg) -> Option<FrontendBlock> {
        match event {
            EventMsg::ExecApprovalRequest(request) => Some(FrontendBlock {
                id: None,
                group: None,
                update: crate::protocol::FrontendBlockUpdate::Replace,
                state: crate::protocol::FrontendBlockState::Complete,
                role: crate::protocol::FrontendBlockRole::Approval,
                title: "Approval required".into(),
                text: format!("{}\n{}", request.reason, approval_tools(&request.calls)),
                symbol: None,
                files: Vec::new(),
                format: crate::protocol::FrontendBlockFormat::PlainText,
                tone: FrontendTone::Warning,
            }),
            _ => None,
        }
    }

    pub(super) fn session_start(&self, session_id: &str) -> Result<Vec<FrontendEvent>> {
        self.states
            .lock()
            .map_err(|_| state_lock_error())?
            .insert(session_id.into(), ApprovalState::default());
        Ok(vec![FrontendEvent::Widget {
            capability: CAPABILITY.into(),
            item: widget(self.default_policy),
        }])
    }

    pub(super) fn authorize(
        &self,
        session_id: &str,
        calls: &[ToolCall],
        mutation_call_ids: &[String],
    ) -> Result<SandboxAuthorization> {
        let approved_for_session = {
            let states = self.states.lock().map_err(|_| state_lock_error())?;
            let state = states.get(session_id).ok_or_else(state_not_initialized)?;
            state.approved_for_session.clone()
        };
        let policy = self.default_policy;
        let calls_by_id = calls
            .iter()
            .map(|call| (call.call_id.as_str(), call))
            .collect::<BTreeMap<_, _>>();
        let mut approved = Vec::new();
        let mut requested = Vec::new();
        for call_id in mutation_call_ids {
            let call = calls_by_id
                .get(call_id.as_str())
                .ok_or_else(|| Error::Tool(format!("unknown mutation call `{call_id}`")))?;
            if policy != ApprovalPolicy::Ask
                || approved_for_session.contains(&call_key(session_id, call)?)
            {
                approved.push(call_id.clone());
            } else {
                requested.push(call_id.clone());
            }
        }
        let permissions = SandboxPermissions::new(
            session_id,
            policy.sandbox_mode(),
            policy.network_access(),
            approved,
        );
        if requested.is_empty() {
            return Ok(SandboxAuthorization::Execute(permissions));
        }
        let request = SandboxApprovalRequest {
            id: Uuid::new_v4().to_string(),
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
        session_id: &str,
        calls: &[ToolCall],
        approval_call_ids: &[String],
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
            .map(|call| (call.call_id.as_str(), call))
            .collect::<BTreeMap<_, _>>();
        for call_id in approval_call_ids {
            if !calls_by_id.contains_key(call_id.as_str()) {
                return Err(Error::Tool(format!(
                    "approval references unknown call `{call_id}`"
                )));
            }
        }
        permissions.allow_mutations(approval_call_ids.iter().cloned());
        if !matches!(decision, ReviewDecision::ApprovedForSession) {
            return Ok(permissions);
        }
        let keys = approval_call_ids
            .iter()
            .map(|call_id| call_key(session_id, calls_by_id[call_id.as_str()]))
            .collect::<Result<Vec<_>>>()?;
        let mut states = self.states.lock().map_err(|_| state_lock_error())?;
        let state = states
            .get_mut(session_id)
            .ok_or_else(state_not_initialized)?;
        for key in keys {
            if state.approved_for_session.len() >= MAX_SESSION_APPROVALS {
                state.approved_for_session.clear();
            }
            state.approved_for_session.insert(key);
        }
        Ok(permissions)
    }

    pub(super) fn session_end(&self, session_id: &str) -> Result<()> {
        self.states
            .lock()
            .map_err(|_| state_lock_error())?
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

fn call_key(session_id: &str, call: &ToolCall) -> Result<[u8; 32]> {
    let value = serde_json::to_vec(&(session_id, &call.name, &call.arguments))?;
    Ok(Sha256::digest(value).into())
}

fn state_lock_error() -> Error {
    Error::Stopped("approval state lock poisoned".into())
}

fn state_not_initialized() -> Error {
    Error::Stopped("approval state is not initialized".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_policy_values_parse_in_core() {
        for (value, expected) in [
            ("ask", ApprovalPolicy::Ask),
            ("allow", ApprovalPolicy::Allow),
            ("allow_network", ApprovalPolicy::AllowNetwork),
            ("full_access", ApprovalPolicy::FullAccess),
        ] {
            let policy = value.parse::<ApprovalPolicy>().expect("policy");
            assert_eq!(policy, expected);
            assert_eq!(Approval::new(policy).policy(), policy);
        }
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::Ask);
        assert!("auto_approve".parse::<ApprovalPolicy>().is_err());
    }

    #[test]
    fn authorized_execution_modes_assign_backend_network_access() {
        assert_eq!(ApprovalPolicy::Ask.network_access(), NetworkAccess::Allowed);
        assert_eq!(
            ApprovalPolicy::Allow.network_access(),
            NetworkAccess::Denied
        );
        assert_eq!(
            ApprovalPolicy::AllowNetwork.network_access(),
            NetworkAccess::Allowed
        );
        assert_eq!(
            ApprovalPolicy::FullAccess.network_access(),
            NetworkAccess::Allowed
        );
        assert_eq!(
            ApprovalPolicy::FullAccess.sandbox_mode(),
            SandboxMode::DangerFullAccess
        );
    }

    #[test]
    fn full_access_authorizes_mutations_without_review() {
        let approval = Approval::new(ApprovalPolicy::FullAccess);
        approval.session_start("session").expect("session start");
        let calls = [ToolCall {
            call_id: "write".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "a"}),
        }];

        let SandboxAuthorization::Execute(permissions) = approval
            .authorize("session", &calls, &["write".into()])
            .expect("authorization")
        else {
            panic!("full access must execute without review");
        };
        let permissions = permissions.for_call("write");

        assert_eq!(permissions.sandbox_mode, SandboxMode::DangerFullAccess);
        assert_eq!(permissions.network_access, NetworkAccess::Allowed);
        assert!(permissions.mutation);
    }

    #[test]
    fn approval_rendering_is_frontend_neutral() {
        let block = Approval::new(ApprovalPolicy::Ask)
            .render(&EventMsg::ExecApprovalRequest(
                crate::protocol::ExecApprovalRequestEvent {
                    id: "approval".into(),
                    turn_id: "turn".into(),
                    calls: vec![crate::protocol::ApprovalCall {
                        call_id: "call".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "true"}),
                    }],
                    reason: "command execution".into(),
                },
            ))
            .expect("approval block");

        assert_eq!(block.title, "Approval required");
        assert!(block.text.starts_with("command execution\n"));
        assert!(!block.text.contains('[') && !block.text.contains(']'));
    }

    #[test]
    fn approval_grants_only_the_reviewed_call() {
        let approval = Approval::new(ApprovalPolicy::Ask);
        approval.states.lock().expect("approval state").insert(
            "session".into(),
            ApprovalState {
                approved_for_session: BTreeSet::new(),
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
            .authorize("session", &calls, &["write".into()])
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
                "session",
                &calls,
                &request.call_ids,
                &ReviewDecision::Approved,
                permissions,
            )
            .expect("resolution");
        assert!(permissions.for_call("write").mutation);
    }
}
