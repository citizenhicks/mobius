use super::*;

fn page(action: &Action) -> Value {
    match action {
        Action::Page(text) => serde_json::from_str(text).expect("JSON to the page"),
        Action::Client(text) => panic!("expected a page message, got {text}"),
    }
}

fn client(action: &Action) -> Value {
    match action {
        Action::Client(text) => serde_json::from_str(text).expect("JSON to the client"),
        Action::Page(text) => panic!("expected a client message, got {text}"),
    }
}

/// Runs Playwright's connect sequence against a page answering like Chromium.
fn attached() -> Router {
    let mut router = Router::default();
    let ask = router.on_client(r#"{"id":1,"method":"Target.setAutoAttach","params":{"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}}"#);
    assert_eq!(page(&ask[0])["method"], "Target.getTargetInfo");
    let info = json!({"id": TARGET_INFO, "result": {"targetInfo": {"targetId": "T", "type": "page", "url": "https://example.com/", "attached": true}}});
    let attach = router.on_page(&info.to_string());
    let attach = page(&attach[0]);
    assert_eq!(attach["method"], "Target.attachToTarget");
    assert_eq!(attach["params"], json!({"targetId": "T", "flatten": true}));
    let done = router.on_page(&json!({"id": ATTACH, "result": {"sessionId": "S"}}).to_string());
    let announced = client(&done[0]);
    assert_eq!(announced["method"], "Target.attachedToTarget");
    assert_eq!(announced["params"]["sessionId"], "S");
    assert_eq!(announced["params"]["targetInfo"]["targetId"], "T");
    assert_eq!(announced["params"]["waitingForDebugger"], false);
    assert_eq!(client(&done[1]), json!({"id": 1, "result": {}}));
    router
}

#[test]
fn sessions_must_be_announced_by_this_pages_connection() {
    let mut router = attached();
    let command = |session| {
        json!({"id": 4, "method": "Runtime.evaluate", "sessionId": session,
        "params": {"expression": "1+1"}})
        .to_string()
    };
    assert!(client(&router.on_client(&command("other"))[0])["error"].is_object());
    assert!(
        router
            .on_page(r#"{"sessionId":"other","method":"Runtime.consoleAPICalled"}"#)
            .is_empty()
    );
    // Another client's session cannot lend us one of its children.
    router.on_page(r#"{"sessionId":"other","method":"Target.attachedToTarget","params":{"sessionId":"child"}}"#);
    assert!(client(&router.on_client(&command("child"))[0])["error"].is_object());
    router.on_page(
        r#"{"sessionId":"S","method":"Target.attachedToTarget","params":{"sessionId":"child","targetInfo":{"type":"worker"}}}"#,
    );
    assert_eq!(
        router.on_client(&command("child")),
        vec![Action::Page(command("child"))]
    );
    router.on_page(
        r#"{"sessionId":"S","method":"Target.detachedFromTarget","params":{"sessionId":"child"}}"#,
    );
    assert!(client(&router.on_client(&command("child"))[0])["error"].is_object());
    let detach = r#"{"id":5,"sessionId":"S","method":"Target.detachFromTarget","params":{"sessionId":"other"}}"#;
    assert!(client(&router.on_client(detach)[0])["error"].is_object());
}

#[test]
fn auto_attach_cannot_include_shared_or_unflattened_targets() {
    let mut router = attached();
    let actions = router.on_client(r#"{"id":4,"sessionId":"S","method":"Target.setAutoAttach","params":{"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":false,"filter":[{}]}}"#);
    let command = page(&actions[0]);
    assert_eq!(command["sessionId"], "S");
    assert_eq!(
        command["params"],
        json!({
            "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true,
            "filter": [{"type": "iframe"}, {"type": "worker"}, {"exclude": true}],
        })
    );
    for kind in [
        "iframe",
        "worker",
        "service_worker",
        "shared_worker",
        "page",
        "browser",
        "unknown",
    ] {
        let event = json!({"sessionId": "S", "method": "Target.attachedToTarget", "params": {
            "sessionId": "child", "targetInfo": {"type": kind}, "waitingForDebugger": true,
        }})
        .to_string();
        let actions = router.on_page(&event);
        let child_command = r#"{"id":5,"sessionId":"child","method":"Runtime.evaluate","params":{"expression":"self.clients"}}"#;
        let detached = r#"{"sessionId":"S","method":"Target.detachedFromTarget","params":{"sessionId":"child"}}"#;
        if matches!(kind, "iframe" | "worker") {
            assert_eq!(actions, vec![Action::Client(event)]);
            assert!(matches!(
                router.on_client(child_command)[0],
                Action::Page(_)
            ));
            assert_eq!(
                router.on_page(detached),
                vec![Action::Client(detached.to_owned())]
            );
        } else {
            assert_eq!(actions.len(), 1);
            assert_eq!(
                page(&actions[0]),
                json!({
                    "id": DETACH, "sessionId": "S", "method": "Target.detachFromTarget", "params": {"sessionId": "child"},
                })
            );
            assert!(client(&router.on_client(child_command)[0])["error"].is_object());
            assert!(
                router
                    .on_page(&json!({"id": DETACH, "sessionId": "S", "result": {}}).to_string())
                    .is_empty()
            );
            assert!(router.on_page(detached).is_empty());
        }
    }
}

#[test]
fn browser_level_commands_beyond_the_page_are_refused() {
    let mut router = attached();
    for (id, method) in [
        (5, "Target.createTarget"),
        (6, "Storage.getCookies"),
        (7, "Browser.close"),
    ] {
        let refused =
            router.on_client(&json!({"id": id, "method": method, "params": {}}).to_string());
        let refused = client(&refused[0]);
        assert_eq!(refused["id"], id);
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains(method)
        );
    }
    let closed =
        router.on_client(r#"{"id":8,"method":"Target.closeTarget","params":{"targetId":"T"}}"#);
    assert_eq!(client(&closed[0])["result"]["success"], false);
    let downloads = router.on_client(r#"{"id":9,"method":"Browser.setDownloadBehavior","params":{"behavior":"allow","downloadPath":"/tmp"}}"#);
    assert_eq!(client(&downloads[0]), json!({"id": 9, "result": {}}));
}

#[test]
fn page_session_commands_stay_within_the_page() {
    let mut router = attached();
    let refused = [
        json!({"method": "Target.getTargets"}),
        json!({"method": "Target.attachToTarget", "params": {"targetId": "other"}}),
        json!({"method": "Target.getTargetInfo", "params": {"targetId": "other"}}),
        json!({"method": "Browser.grantPermissions", "params": {"permissions": ["geolocation"]}}),
        json!({"method": "Browser.setDownloadBehavior", "params": {"behavior": "allow", "downloadPath": "/Users"}}),
        json!({"method": "Network.getAllCookies"}),
        json!({"method": "Network.getCookies", "params": {"urls": ["https://mail.example"]}}),
        json!({"method": "Network.clearBrowserCookies"}),
        json!({"method": "Network.enableDeviceBoundSessions", "params": {"enable": true}}),
        json!({"method": "Network.deleteDeviceBoundSession", "params": {"site": "https://bank.example", "sessionId": "other"}}),
        json!({"method": "Storage.clearDataForOrigin", "params": {"origin": "https://bank.example"}}),
        json!({"method": "DOMStorage.getDOMStorageItems", "params": {"storageId": {"securityOrigin": "https://bank.example", "isLocalStorage": true}}}),
        json!({"method": "IndexedDB.requestDatabaseNames", "params": {"securityOrigin": "https://bank.example"}}),
        json!({"method": "CacheStorage.requestCacheNames", "params": {"securityOrigin": "https://bank.example"}}),
        json!({"method": "Storage.overrideQuotaForOrigin", "params": {"origin": "https://bank.example", "quotaSize": 0}}),
        json!({"method": "Page.navigate", "params": {"url": "file:///etc/passwd"}}),
        json!({"method": "Page.navigate", "params": {"url": "chrome://settings"}}),
        json!({"method": "Page.close"}),
        json!({"method": "Page.deleteCookie", "params": {"cookieName": "session", "url": "https://bank.example"}}),
        json!({"method": "Tracing.requestMemoryDump"}),
        json!({"method": "Security.setOverrideCertificateErrors", "params": {"override": true}}),
        json!({"method": "NewBrowserDomain.futureGlobalCommand"}),
        json!({"method": "DOM.setFileInputFiles", "params": {"files": ["/Users/me/.ssh/id_ed25519"]}}),
        json!({"method": "Input.dispatchDragEvent", "params": {"type": "drop", "data": {"items": [], "files": ["/etc/hosts"]}}}),
    ];
    for (index, mut command) in refused.into_iter().enumerate() {
        command["id"] = json!(100 + index);
        command["sessionId"] = json!("S");
        let answer = router.on_client(&command.to_string());
        let answer = client(&answer[0]);
        assert_eq!(answer["sessionId"], "S", "{command}");
        assert!(answer["error"].is_object(), "{command} was not refused");
    }
    let allowed = [
        json!({"method": "Page.navigate", "params": {"url": "https://example.com/"}}),
        json!({"method": "Page.navigate", "params": {"url": "about:blank"}}),
        json!({"method": "Network.getCookies"}),
        json!({"method": "Target.getTargetInfo"}),
        json!({"method": "Browser.getVersion"}),
        json!({"method": "Input.dispatchDragEvent", "params": {"type": "drop", "data": {"items": []}}}),
    ];
    for command in allowed {
        let mut command = command;
        command["id"] = json!(200);
        command["sessionId"] = json!("S");
        let text = command.to_string();
        assert!(
            matches!(router.on_client(&text).as_slice(), [Action::Page(sent)] if *sent == text),
            "{command} was refused"
        );
    }
}
