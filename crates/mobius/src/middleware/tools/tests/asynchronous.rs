use super::*;

/// An asynchronous tool that succeeds with its own output, or fails.
struct Later {
    definition: Arc<ToolDefinition>,
}

impl Tool for Later {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &self.definition
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            if arguments["mode"] == "fail" {
                Err(Error::Tool("rejected".into()))
            } else if arguments["mode"] == "complete" {
                Ok("ready".into())
            } else {
                Ok(crate::protocol::ToolResponse::Deferred(
                    context.deferred_call(),
                ))
            }
        })
    }
}

#[tokio::test]
async fn an_asynchronous_call_answers_pending_and_its_errors_still_end_it() {
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(Later {
            definition: named_tool_definition("later"),
        }))
        .expect("register tool");
    let calls: Vec<_> = ["defer", "fail", "complete"]
        .into_iter()
        .map(|mode| ToolCall {
            call_id: mode.into(),
            name: "later".into(),
            arguments: serde_json::json!({"mode":mode}),
        })
        .collect();
    let calls = finalize_and_bind(&mut catalog, &calls);
    let results = run_calls(&catalog, calls, test_sandbox(), &test_permissions("s", &[])).await;
    let pending: Value = serde_json::from_str(&results[0].output.text()).expect("pending JSON");
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["call_id"], "step/defer");
    assert!(!results[0].is_error);
    assert!(results[1].is_error, "a failed call is not pending");
    assert!(results[1].output.text().contains("rejected"));
    assert_eq!(results[2].output.text(), "ready");
}

#[test]
fn a_later_result_is_a_tool_message_that_steers_its_turn() {
    let call_id = crate::protocol::DeferredToolCall("call-1".into());
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
