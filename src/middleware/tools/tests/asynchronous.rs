use super::*;

/// An asynchronous tool that succeeds with its own output, or fails.
struct Later {
    definition: Arc<ToolDefinition>,
    fails: bool,
}

impl Tool for Later {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &self.definition
    }

    fn asynchronous(&self) -> bool {
        true
    }

    fn call(
        &self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        let fails = self.fails;
        Box::pin(async move {
            if fails {
                Err(Error::Tool("rejected".into()))
            } else {
                Ok("the tool's own output".into())
            }
        })
    }
}

#[tokio::test]
async fn an_asynchronous_call_answers_pending_and_its_errors_still_end_it() {
    let mut catalog = Catalog::default();
    for (name, fails) in [("later", false), ("broken", true)] {
        catalog
            .register(Arc::new(Later {
                definition: named_tool_definition(name),
                fails,
            }))
            .expect("register tool");
    }
    let calls: Vec<_> = ["later", "broken"]
        .into_iter()
        .map(|name| ToolCall {
            call_id: format!("{name}-call").into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        })
        .collect();
    let calls = finalize_and_bind(&mut catalog, &calls);
    let results = run_calls(&catalog, calls, test_sandbox(), &test_permissions("s", &[])).await;
    let pending: Value = serde_json::from_str(&results[0].output.text()).expect("pending JSON");
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["call_id"], "later-call");
    assert!(!results[0].is_error);
    assert!(results[1].is_error, "a failed call is not pending");
    assert!(results[1].output.text().contains("rejected"));
}

#[test]
fn a_later_result_is_a_tool_message_that_steers_its_turn() {
    let call_id: Arc<str> = "call-1".into();
    let submission = async_tool_result(
        &call_id,
        "ask_user",
        &serde_json::json!({"status": "no_answer"}),
        Some("turn-1".into()),
    );
    let crate::protocol::Op::Message { message } = submission.op else {
        panic!("a message");
    };
    assert_eq!(message.text, r#"{"status":"no_answer"}"#);
    assert_eq!(message.target_turn_id.as_deref(), Some("turn-1"));
    assert!(matches!(
        message.author,
        crate::protocol::MessageAuthor::Source {
            source: crate::protocol::MessageSource::Tool { ref call_id },
            ref handle,
            ..
        } if &**call_id == "call-1" && &**handle == "ask_user"
    ));
}
