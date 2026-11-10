//! Serves one page to a DevTools client that expects a whole browser. Playwright connects
//! at the browser level, so the router answers that handshake itself, attaches a session of
//! its own to the page, and passes the session's traffic through. Detaching that session
//! when the client leaves resets everything the client changed. Commands that would reach
//! past the page — other tabs, the profile's cookies, files on disk — are refused.

use std::borrow::Cow;
use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};

/// Where a message goes next.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Client(String),
    Page(String),
}

/// Requests the router makes of the page itself; clients number theirs from 1.
const TARGET_INFO: i64 = 2_000_000_001;
const ATTACH: i64 = 2_000_000_002;
const DETACH: i64 = 2_000_000_003;

#[derive(Default)]
pub struct Router {
    /// The session attached to the page for this client.
    session: Option<String>,
    /// Only sessions announced beneath this connection's page may carry commands.
    children: HashSet<String>,
    target: Option<Value>,
    /// The client's `Target.setAutoAttach`, answered once the session exists.
    waiting: Option<i64>,
    /// Browser-level requests the page answers for the client.
    forwarded: HashSet<i64>,
}

/// The fields routing needs, read without copying a message's (possibly large) body.
#[derive(Deserialize)]
struct Envelope<'a> {
    id: Option<i64>,
    #[serde(rename = "sessionId", borrow)]
    session_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    method: Option<Cow<'a, str>>,
}

impl Router {
    /// Routes a message from the client.
    pub fn on_client(&mut self, text: &str) -> Vec<Action> {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        let (Some(id), Some(method)) = (
            message.get("id").and_then(Value::as_i64),
            message.get("method").and_then(Value::as_str),
        ) else {
            return Vec::new();
        };
        let session = message.get("sessionId").and_then(Value::as_str);
        if matches!(id, TARGET_INFO | ATTACH | DETACH) {
            return vec![Action::Client(error(id, session, "Reserved request ID"))];
        }
        let params = message.get("params").unwrap_or(&Value::Null);
        match session {
            Some(session) if !self.owns_session(session) => vec![Action::Client(error(
                id,
                Some(session),
                "This session does not belong to the chat's page",
            ))],
            Some(session) if method == "Target.setAutoAttach" => vec![Action::Page(
                json!({
                    "id": id, "sessionId": session, "method": method,
                    "params": {
                        "autoAttach": params.get("autoAttach"),
                        "waitForDebuggerOnStart": params.get("waitForDebuggerOnStart"),
                        "flatten": true,
                        "filter": [{"type": "iframe"}, {"type": "worker"}, {"exclude": true}],
                    },
                })
                .to_string(),
            )],
            Some(session)
                if method == "Target.detachFromTarget"
                    && !params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| self.children.contains(id)) =>
            {
                vec![Action::Client(error(
                    id,
                    Some(session),
                    "Only this page's child sessions may be detached",
                ))]
            }
            Some(session) => match refusal(method, params) {
                Some(reason) => vec![Action::Client(error(id, Some(session), &reason))],
                None => vec![Action::Page(text.to_owned())],
            },
            None => self.browser_command(id, method, text),
        }
    }

    /// Routes a message from the page: the attached session's traffic goes to the client
    /// unchanged; the router's own replies advance the handshake.
    pub fn on_page(&mut self, text: &str) -> Vec<Action> {
        let Ok(envelope) = serde_json::from_str::<Envelope>(text) else {
            return Vec::new();
        };
        if let Some(session) = envelope.session_id.as_deref() {
            if !self.owns_session(session) || envelope.id == Some(DETACH) {
                return Vec::new();
            }
            if let Some(method @ ("Target.attachedToTarget" | "Target.detachedFromTarget")) =
                envelope.method.as_deref()
            {
                return self.child_event(session, method, text);
            }
            return vec![Action::Client(text.to_owned())];
        }
        match envelope.id {
            Some(TARGET_INFO | ATTACH) => self.handshake(text),
            Some(id) if self.forwarded.remove(&id) => vec![Action::Client(text.to_owned())],
            None if envelope.method.as_deref() == Some("Target.detachedFromTarget") => {
                self.session_detached(text)
            }
            Some(_) | None => Vec::new(),
        }
    }

    fn owns_session(&self, session: &str) -> bool {
        self.session.as_deref() == Some(session) || self.children.contains(session)
    }

    fn child_event(&mut self, session: &str, method: &str, text: &str) -> Vec<Action> {
        let Ok(event) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        let Some(child) = event.pointer("/params/sessionId").and_then(Value::as_str) else {
            return Vec::new();
        };
        if method == "Target.attachedToTarget" {
            // Service/shared workers can control other pages even when announced here.
            if !matches!(
                event
                    .pointer("/params/targetInfo/type")
                    .and_then(Value::as_str),
                Some("iframe" | "worker")
            ) {
                self.children.remove(child);
                return vec![Action::Page(
                    json!({
                        "id": DETACH, "sessionId": session, "method": "Target.detachFromTarget",
                        "params": {"sessionId": child},
                    })
                    .to_string(),
                )];
            }
            self.children.insert(child.to_owned());
        } else if !self.children.remove(child) {
            return Vec::new();
        }
        vec![Action::Client(text.to_owned())]
    }

    /// The message that ends the client's session, undoing its effects on the page.
    pub fn detach(&self) -> Option<String> {
        self.session.as_ref().map(|session| {
            json!({"id": DETACH, "method": "Target.detachFromTarget", "params": {"sessionId": session}})
                .to_string()
        })
    }

    fn browser_command(&mut self, id: i64, method: &str, text: &str) -> Vec<Action> {
        match method {
            "Target.setAutoAttach" if self.session.is_some() => vec![reply_to(id, &json!({}))],
            "Target.setAutoAttach" => {
                self.waiting = Some(id);
                vec![Action::Page(
                    json!({"id": TARGET_INFO, "method": "Target.getTargetInfo"}).to_string(),
                )]
            }
            "Browser.getVersion" if self.forwarded.len() < 32 => {
                self.forwarded.insert(id);
                vec![Action::Page(text.to_owned())]
            }
            "Target.getTargetInfo" => {
                vec![reply_to(id, &json!({"targetInfo": self.target}))]
            }
            // Downloads keep the app's own folder and prompts; targets are only this page.
            "Browser.setDownloadBehavior"
            | "Target.setDiscoverTargets"
            | "Target.activateTarget" => vec![reply_to(id, &json!({}))],
            "Target.closeTarget" => vec![reply_to(id, &json!({"success": false}))],
            _ => vec![Action::Client(error(
                id,
                None,
                &format!("{method} is unavailable: this connection reaches only its chat's page"),
            ))],
        }
    }

    fn handshake(&mut self, text: &str) -> Vec<Action> {
        let (Ok(mut reply), Some(waiting)) = (serde_json::from_str::<Value>(text), self.waiting)
        else {
            return Vec::new();
        };
        if let Some(message) = reply.pointer("/error/message").and_then(Value::as_str) {
            self.waiting = None;
            return vec![Action::Client(error(waiting, None, message))];
        }
        if reply.get("id").is_some_and(|id| *id == TARGET_INFO) {
            let target = reply
                .pointer_mut("/result/targetInfo")
                .map(Value::take)
                .unwrap_or_default();
            let attach = json!({
                "id": ATTACH,
                "method": "Target.attachToTarget",
                "params": {"targetId": target.get("targetId"), "flatten": true},
            });
            self.target = Some(target);
            return vec![Action::Page(attach.to_string())];
        }
        let Some(session) = reply.pointer("/result/sessionId").and_then(Value::as_str) else {
            return Vec::new();
        };
        self.waiting = None;
        self.session = Some(session.to_owned());
        let target = self.target.get_or_insert_default();
        if let Some(fields) = target.as_object_mut() {
            fields.insert("attached".to_owned(), Value::Bool(true));
        } else {
            *target = json!({"attached": true});
        }
        let attached = json!({
            "method": "Target.attachedToTarget",
            "params": {"sessionId": session, "targetInfo": target, "waitingForDebugger": false},
        });
        vec![
            Action::Client(attached.to_string()),
            reply_to(waiting, &json!({})),
        ]
    }

    fn session_detached(&mut self, text: &str) -> Vec<Action> {
        let Ok(event) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        if event.pointer("/params/sessionId").and_then(Value::as_str) != self.session.as_deref() {
            return Vec::new();
        }
        self.session = None;
        self.children.clear();
        vec![Action::Client(text.to_owned())]
    }
}

fn reply_to(id: i64, result: &Value) -> Action {
    Action::Client(json!({"id": id, "result": result}).to_string())
}

fn error(id: i64, session: Option<&str>, message: &str) -> String {
    let mut error = json!({"id": id, "error": {"code": -32000, "message": message}});
    if let (Some(session), Some(fields)) = (session, error.as_object_mut()) {
        fields.insert("sessionId".to_owned(), json!(session));
    }
    error.to_string()
}

/// Why a page-session command is refused: it would act on the browser beyond this page.
fn refusal(method: &str, params: &Value) -> Option<String> {
    let (domain, command) = method.split_once('.')?;
    let beyond_page = match domain {
        "Target" => match command {
            "setAutoAttach" | "detachFromTarget" => false,
            "getTargetInfo" => params.get("targetId").is_some(),
            _ => true,
        },
        "Browser" => command != "getVersion",
        "Storage" => !matches!(command, "getStorageKeyForFrame" | "getStorageKey"),
        "Network" => match command {
            "getAllCookies"
            | "setCookie"
            | "setCookies"
            | "deleteCookies"
            | "clearBrowserCookies"
            | "clearBrowserCache"
            | "enableDeviceBoundSessions"
            | "deleteDeviceBoundSession" => true,
            "getCookies" => params.get("urls").is_some(),
            _ => false,
        },
        "Page" => match command {
            "close" | "crash" | "setDownloadBehavior" | "deleteCookie" => true,
            "navigate" => !web_address(
                params
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            _ => false,
        },
        "DOM" => command == "setFileInputFiles",
        "Input" => {
            command == "dispatchDragEvent"
                && params
                    .pointer("/data/files")
                    .and_then(Value::as_array)
                    .is_some_and(|files| !files.is_empty())
        }
        "Security" => !matches!(command, "enable" | "disable"),
        "Runtime" | "Debugger" | "DOMDebugger" | "DOMSnapshot" | "Accessibility" | "CSS"
        | "Overlay" | "Fetch" | "Emulation" | "Log" | "Performance" | "Inspector" => false,
        // Includes DOMStorage, IndexedDB, CacheStorage and ServiceWorker: they take arbitrary
        // origins/storage keys, not a frame in this page.
        _ => true,
    };
    beyond_page.then(|| format!("{method} is unavailable: it reaches beyond this chat's page"))
}

/// Pages the agent may open: the web, and blank or inline documents, never local files or
/// the browser's own pages.
fn web_address(url: &str) -> bool {
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    matches!(scheme.as_deref(), Some("http" | "https" | "data" | "blob")) || url == "about:blank"
}

#[cfg(test)]
mod tests;
