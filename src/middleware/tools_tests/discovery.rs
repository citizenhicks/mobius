use super::*;

struct NamedTool {
    name: String,
    description: String,
    exposure: ToolExposure,
}

impl NamedTool {
    fn new(name: &str, description: &str, exposure: ToolExposure) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            exposure,
        }
    }
}

impl Tool for NamedTool {
    fn definition(&self) -> ToolDefinition {
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
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Ok("executed".into()) })
    }
}

struct DefaultDeferredTool;

impl Tool for DefaultDeferredTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "default_deferred".into(),
            description: "default exposure".into(),
            parameters: serde_json::json!({}),
        }
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Ok(String::new()) })
    }
}

fn names(definitions: &[ToolDefinition]) -> Vec<&str> {
    definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect()
}

fn function_call(name: &str) -> ToolCall {
    ToolCall {
        call_id: format!("call-{name}"),
        name: name.into(),
        arguments: serde_json::json!({}),
    }
}

#[test]
fn only_core_tools_are_direct_by_default() {
    assert_eq!(DefaultDeferredTool.exposure(), ToolExposure::Deferred);
    for exposure in [
        ReadFile.exposure(),
        WriteFile.exposure(),
        ApplyPatch.exposure(),
        Bash.exposure(),
        StartCommand.exposure(),
        PollCommand.exposure(),
        StopCommand.exposure(),
    ] {
        assert_eq!(exposure, ToolExposure::Direct);
    }
}

#[test]
fn finalization_partitions_exposure_and_adds_search_only_when_needed() {
    let mut catalog = Catalog::default();
    catalog.register(Arc::new(ReadFile)).expect("direct tool");
    catalog
        .register(Arc::new(DefaultDeferredTool))
        .expect("deferred tool");
    catalog
        .register(Arc::new(NamedTool::new(
            "internal",
            "hidden tool",
            ToolExposure::Hidden,
        )))
        .expect("hidden tool");
    catalog.finalize().expect("finalize catalog");

    assert_eq!(
        names(&catalog.direct_definitions()),
        ["read_file", TOOLS_SEARCH_NAME]
    );
    assert_eq!(names(&catalog.deferred_definitions()), ["default_deferred"]);
    assert_eq!(
        names(&catalog.registered_definitions()),
        [
            "default_deferred",
            "internal",
            "read_file",
            TOOLS_SEARCH_NAME
        ]
    );
    assert_eq!(catalog.revision().expect("catalog revision").len(), 64);

    let mut direct_only = Catalog::default();
    direct_only
        .register(Arc::new(ReadFile))
        .expect("direct tool");
    direct_only.finalize().expect("finalize catalog");
    assert_eq!(names(&direct_only.direct_definitions()), ["read_file"]);
}

#[test]
fn catalog_revision_is_order_independent_and_schema_sensitive() {
    let catalog = |tools: &[(&str, &str)]| {
        let mut catalog = Catalog::default();
        for (name, description) in tools {
            catalog
                .register(Arc::new(NamedTool::new(
                    name,
                    description,
                    ToolExposure::Deferred,
                )))
                .expect("register tool");
        }
        catalog.finalize().expect("finalize catalog");
        catalog
    };
    let first = catalog(&[("alpha", "one"), ("beta", "two")]);
    let reordered = catalog(&[("beta", "two"), ("alpha", "one")]);
    let changed = catalog(&[("alpha", "changed"), ("beta", "two")]);

    assert_eq!(
        first.revision().expect("first revision"),
        reordered.revision().expect("reordered revision")
    );
    assert_ne!(
        first.revision().expect("first revision"),
        changed.revision().expect("changed revision")
    );
}

#[test]
fn deferred_search_is_ranked_deterministic_and_bounded() {
    let mut catalog = Catalog::default();
    for (name, description) in [
        ("swarm", "exact"),
        ("swarm_post", "name match"),
        ("board", "post to the swarm"),
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
            "direct_swarm",
            "swarm",
            ToolExposure::Direct,
        )))
        .expect("direct tool");
    catalog
        .register(Arc::new(NamedTool::new(
            "hidden_swarm",
            "swarm",
            ToolExposure::Hidden,
        )))
        .expect("hidden tool");

    assert_eq!(
        names(
            &catalog
                .search_deferred(
                    " SWARM ",
                    &BTreeSet::from([
                        "swarm".to_string(),
                        "swarm_post".to_string(),
                        "board".to_string(),
                        "direct_swarm".to_string(),
                        "hidden_swarm".to_string(),
                    ]),
                )
                .expect("search"),
        ),
        ["swarm", "swarm_post", "board"]
    );
    assert_eq!(
        names(
            &catalog
                .search_deferred("swarm", &BTreeSet::from(["board".to_string()]))
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
    catalog.finalize().expect("finalize catalog");

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
    visible.finalize().expect("finalize visible catalog");
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
    hidden.finalize().expect("finalize hidden catalog");

    let result = execute_batch(
        &hidden,
        &[bound],
        test_sandbox(),
        &test_permissions(&[]),
        "turn",
    )
    .await
    .pop()
    .expect("dispatch result");

    assert!(result.is_error);
    assert!(!result.handler_executed);
    assert_eq!(result.output, "tool `changing` is hidden from the model");
}

#[tokio::test]
async fn tools_search_executes_as_a_normal_bound_tool_and_reports_loaded_names() {
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(NamedTool::new(
            "swarm_post",
            "post a message to swarm peers",
            ToolExposure::Deferred,
        )))
        .expect("deferred tool");
    catalog.finalize().expect("finalize catalog");
    let call = catalog
        .bind_call(
            ToolCall {
                call_id: "search-1".into(),
                name: TOOLS_SEARCH_NAME.into(),
                arguments: serde_json::json!({"query": "swarm"}),
            },
            &BTreeSet::new(),
            &BTreeSet::from(["swarm_post".to_string()]),
        )
        .expect("bind search");

    let result = execute_batch(
        &catalog,
        &[call],
        test_sandbox(),
        &test_permissions(&[]),
        "turn",
    )
    .await
    .pop()
    .expect("search result");

    let load = ToolLoad::from_input(&result.additional_input[0])
        .expect("valid load")
        .expect("tool load");
    assert_eq!(load.tools, ["swarm_post"]);
    assert!(matches!(result.events.as_slice(), [EventMsg::ToolLoad(_)]));
    assert_eq!(result.output, r#"{"loaded_tools":["swarm_post"]}"#);
}
