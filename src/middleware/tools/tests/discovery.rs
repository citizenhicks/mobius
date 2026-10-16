use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct NamedTool {
    name: String,
    description: String,
    exposure: ToolExposure,
    definitions: AtomicUsize,
}

impl NamedTool {
    fn new(name: &str, description: &str, exposure: ToolExposure) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            exposure,
            definitions: AtomicUsize::new(0),
        }
    }
}

impl Tool for NamedTool {
    fn definition(&self) -> ToolDefinition {
        self.definitions.fetch_add(1, Ordering::Relaxed);
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false
            }),
        }
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async { Ok("executed".into()) })
    }
}

fn names(definitions: &[Arc<ToolDefinition>]) -> Vec<&str> {
    definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect()
}

fn finalize(catalog: &mut Catalog) {
    catalog.register_search().expect("register tools_search");
    catalog.finalize().expect("finalize catalog");
}

fn function_call(name: &str) -> ToolCall {
    ToolCall {
        call_id: format!("call-{name}"),
        name: name.into(),
        arguments: serde_json::json!({}),
    }
}

#[test]
fn deferred_search_is_relevant_scoped_and_bounded() {
    let mut catalog = Catalog::default();
    for (name, description) in [
        ("notebook", "exact"),
        ("notebook_post", "name match"),
        ("board", "post to the notebook"),
    ] {
        catalog
            .register(Arc::new(NamedTool::new(
                name,
                description,
                ToolExposure::Deferred,
            )))
            .expect("deferred tool");
    }
    catalog
        .register(Arc::new(NamedTool::new(
            "direct_notebook",
            "notebook",
            ToolExposure::Direct,
        )))
        .expect("direct tool");
    catalog
        .register(Arc::new(NamedTool::new(
            "hidden_notebook",
            "notebook",
            ToolExposure::Hidden,
        )))
        .expect("hidden tool");

    assert_eq!(
        names(
            &catalog
                .search_deferred(
                    " NOTEBOOK ",
                    &BTreeSet::from([
                        "notebook".to_string(),
                        "notebook_post".to_string(),
                        "board".to_string(),
                        "direct_notebook".to_string(),
                        "hidden_notebook".to_string(),
                    ]),
                )
                .expect("search"),
        ),
        ["notebook", "board", "notebook_post"]
    );
    assert_eq!(
        names(
            &catalog
                .search_deferred("notebook", &BTreeSet::from(["board".to_string()]))
                .expect("scoped search"),
        ),
        ["board"]
    );
    assert_eq!(
        catalog
            .search_deferred(" ", &BTreeSet::new())
            .expect_err("blank query")
            .to_string(),
        "tool error: tools_search query cannot be empty"
    );
    assert_eq!(
        catalog
            .search_deferred(
                &"x".repeat(MAX_TOOL_SEARCH_QUERY_BYTES + 1),
                &BTreeSet::new(),
            )
            .expect_err("oversized query")
            .to_string(),
        "tool error: tools_search query exceeds 512 bytes"
    );

    let mut bounded = Catalog::default();
    for index in 0..MAX_TOOL_SEARCH_RESULTS + 2 {
        bounded
            .register(Arc::new(NamedTool::new(
                &format!("tool_{index:02}"),
                "needle",
                ToolExposure::Deferred,
            )))
            .expect("bounded search tool");
    }
    assert_eq!(
        bounded
            .search_deferred(
                "needle",
                &bounded
                    .deferred_definitions()
                    .iter()
                    .map(|definition| definition.name.clone())
                    .collect(),
            )
            .expect("search")
            .len(),
        MAX_TOOL_SEARCH_RESULTS
    );
}

#[test]
fn binding_enforces_current_exposure_and_step_materialization() {
    let mut catalog = Catalog::default();
    for (name, exposure) in [
        ("direct", ToolExposure::Direct),
        ("deferred", ToolExposure::Deferred),
        ("hidden", ToolExposure::Hidden),
    ] {
        catalog
            .register(Arc::new(NamedTool::new(name, "tool", exposure)))
            .expect("register tool");
    }
    finalize(&mut catalog);

    catalog
        .bind_call(function_call("direct"), &BTreeSet::new(), &BTreeSet::new())
        .expect("direct call");
    assert_eq!(
        catalog
            .bind_call(
                function_call("deferred"),
                &BTreeSet::new(),
                &BTreeSet::from(["deferred".to_string()]),
            )
            .expect_err("unmaterialized deferred call")
            .to_string(),
        "tool error: tool `deferred` was not materialized for this model step"
    );
    catalog
        .bind_call(
            function_call("deferred"),
            &BTreeSet::from(["deferred".to_string()]),
            &BTreeSet::from(["deferred".to_string()]),
        )
        .expect("materialized deferred call");
    assert_eq!(
        catalog
            .bind_call(
                function_call("deferred"),
                &BTreeSet::from(["deferred".to_string()]),
                &BTreeSet::new(),
            )
            .expect_err("inactive deferred call")
            .to_string(),
        "tool error: tool `deferred` is not available for this model step"
    );
    assert_eq!(
        catalog
            .bind_call(
                function_call("hidden"),
                &BTreeSet::from(["hidden".to_string()]),
                &BTreeSet::from(["hidden".to_string()]),
            )
            .expect_err("hidden call")
            .to_string(),
        "tool error: tool `hidden` is hidden from the model"
    );
    assert_eq!(
        catalog
            .bind_call(function_call("missing"), &BTreeSet::new(), &BTreeSet::new(),)
            .expect_err("unknown call")
            .to_string(),
        "tool error: unknown tool `missing`"
    );
    let prepared = catalog
        .prepare((&[]).into(), Cow::Owned(BTreeSet::from(["direct".into()])))
        .expect("active tool catalog");
    let calls = [function_call("missing")];
    let (bound, callable, mut rejected) = catalog.bind_live_batch(&calls, &prepared, 8);
    assert!(bound.is_empty());
    assert!(callable.is_empty());
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].call_id, calls[0].call_id);
    assert!(rejected[0].is_error);
    assert_eq!(rejected[0].output.text(), "tool err");
    rejected[0].replace("éééééé");
    assert_eq!(rejected[0].output.text(), "éééé");
}

#[tokio::test]
async fn dispatch_rechecks_exposure_in_the_current_catalog() {
    let mut visible = Catalog::default();
    visible
        .register(Arc::new(NamedTool::new(
            "changing",
            "deferred tool",
            ToolExposure::Deferred,
        )))
        .expect("visible tool");
    finalize(&mut visible);
    let bound = visible
        .bind_call(
            function_call("changing"),
            &BTreeSet::from(["changing".to_string()]),
            &BTreeSet::from(["changing".to_string()]),
        )
        .expect("bind visible call");

    let mut hidden = Catalog::default();
    hidden
        .register(Arc::new(NamedTool::new(
            "changing",
            "hidden tool",
            ToolExposure::Hidden,
        )))
        .expect("hidden tool");
    finalize(&mut hidden);

    let result = execute_batch(
        &hidden,
        [bound],
        test_sandbox(),
        &test_permissions(&[]),
        "turn",
        "test",
        &crate::protocol::MessageAuthor::User,
    )
    .await
    .pop()
    .expect("dispatch result");

    assert!(result.is_error);
    assert!(!result.handler_executed);
    assert_eq!(
        result.output.text(),
        "tool `changing` is hidden from the model"
    );
}
