use super::*;

#[test]
fn gateway_rejects_nested_session_history() {
    assert!(
        validate_gateway_event(&EventMsg::SessionHistory(
            mobius::protocol::SessionHistoryEvent { events: Vec::new() }
        ))
        .is_err()
    );
    assert!(validate_gateway_event(&EventMsg::ContextCompacted).is_ok());
}

#[test]
fn projected_preview_drops_the_raw_nested_event_duplicate() {
    let mut event = EventMsg::Frontend(FrontendEvent::Preview {
        id: "/root/reviewer".into(),
        title: "reviewer".into(),
        subtitle: "Full context".into(),
        page_id: "/root/reviewer:latest".into(),
        update: mobius::protocol::FrontendPreviewUpdate::Replace,
        events: vec![FrontendPreviewEvent {
            recorded_at_ms: 1,
            event: EventMsg::ContextCompacted,
        }],
        next: None,
    });

    clear_projected_preview_events(&mut event);

    assert!(matches!(
        event,
        EventMsg::Frontend(FrontendEvent::Preview {
            id,
            events,
            ..
        }) if id == "/root/reviewer" && events.is_empty()
    ));
}

#[test]
fn router_reuse_ignores_local_recipe_changes_but_not_provider_changes() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&state_dir).expect("state directory");
    let base = ChatSpec::for_bot(
        &workspace,
        &crate::wire::BotRecord {
            id: Uuid::new_v4().to_string(),
            handle: "fixture".into(),
            name: "Fixture".into(),
            description: "Own fixture work.".into(),
            tint: crate::wire::ProviderTint::default(),
            config: crate::wire::VersionedAgentConfig {
                revision: 1,
                config: AgentComposition::default(),
            },
        },
        &state_dir,
        None,
    )
    .expect("chat spec");

    let changes: [fn(&mut AgentComposition); 2] = [
        |config: &mut AgentComposition| config.system_prompt.push_str(" updated"),
        |config: &mut AgentComposition| config.max_model_steps += 1,
    ];
    for change in changes {
        let mut next = base.clone();
        change(&mut next.agent.config);
        assert!(provider_config_unchanged(&base, &next));
    }

    let mut provider_changed = base.clone();
    provider_changed
        .agent
        .config
        .provider
        .model
        .push_str("-changed");
    assert!(!provider_config_unchanged(&base, &provider_changed));
}

#[test]
fn journal_delivery_accepts_loaded_records_and_rejects_gaps() {
    assert_eq!(
        classify_journal_sequence(5, 3, JournalDelivery::LoadedStartup).expect("loaded record"),
        JournalSequence::AlreadyLoaded
    );
    assert_eq!(
        classify_journal_sequence(5, 5, JournalDelivery::LoadedStartup).expect("loaded high-water"),
        JournalSequence::AlreadyLoaded
    );
    assert_eq!(
        classify_journal_sequence(5, 6, JournalDelivery::Live).expect("next record"),
        JournalSequence::Next
    );
    assert!(
        classify_journal_sequence(5, 7, JournalDelivery::Live)
            .expect_err("sequence gap")
            .to_string()
            .contains("expected 6")
    );
    assert!(
        classify_journal_sequence(5, 5, JournalDelivery::Live)
            .expect_err("stale live record")
            .to_string()
            .contains("expected 6")
    );
}

#[test]
fn widget_snapshot_is_namespaced_updated_and_removed() {
    let widget = |id: &str, text: &str| mobius::protocol::FrontendWidget {
        id: id.into(),
        slot: mobius::protocol::FrontendSlot::Header,
        text: text.into(),
        tone: mobius::protocol::FrontendTone::Neutral,
        symbol: None,
        icon_only: false,
        progress: None,
        content: None,
        action: None,
    };
    let mut widgets = SessionWidgets::new();
    update_widgets(
        &mut widgets,
        &EventMsg::Frontend(FrontendEvent::Widget {
            capability: "tasks".into(),
            item: widget("z-older", "one"),
        }),
    );
    update_widgets(
        &mut widgets,
        &EventMsg::Frontend(FrontendEvent::Widget {
            capability: "tasks".into(),
            item: widget("a-newer", "two"),
        }),
    );
    update_widgets(
        &mut widgets,
        &EventMsg::Frontend(FrontendEvent::Widget {
            capability: "subagents".into(),
            item: widget("status", "other"),
        }),
    );
    update_widgets(
        &mut widgets,
        &EventMsg::Frontend(FrontendEvent::Widget {
            capability: "tasks".into(),
            item: widget("z-older", "updated"),
        }),
    );
    update_widgets(
        &mut widgets,
        &EventMsg::Frontend(FrontendEvent::RemoveWidget {
            capability: "subagents".into(),
            id: "status".into(),
        }),
    );

    assert_eq!(
        widgets
            .into_iter()
            .map(|((capability, id), item)| (capability, id, item.text))
            .collect::<Vec<_>>(),
        vec![
            ("tasks".into(), "z-older".into(), "updated".into()),
            ("tasks".into(), "a-newer".into(), "two".into())
        ]
    );
}

#[test]
fn completed_execution_stats_keep_the_flat_wire_shape() {
    let stats = ExecutionStats {
        run_count: 3,
        failed_run_count: 1,
        aborted_run_count: 1,
        model_calls: 5,
        tool_calls: 8,
        failed_tool_calls: 2,
        elapsed_ms: 900,
        usage: TokenUsage {
            total_tokens: 42,
            ..TokenUsage::default()
        },
    };

    let mut expected = serde_json::to_value(&stats).expect("execution totals");
    expected["active"] = serde_json::Value::Null;
    let projected = RunStats {
        completed: stats,
        active: None,
    };
    assert_eq!(
        serde_json::to_value(&projected).expect("run totals"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<RunStats>(expected).expect("decode flat totals"),
        projected
    );
}

#[test]
fn recent_runs_group_under_the_nearest_visible_session_in_source_order() {
    let sessions = vec![
        session_summary("root", None, true, Some("Root preview")),
        session_summary("nested-agent", Some("agent"), false, None),
        session_summary("agent", Some("root"), false, None),
        session_summary("fork", Some("root"), true, None),
        session_summary("fork-agent", Some("fork"), false, None),
    ];
    let records = vec![
        execution_record("nested-agent", "nested", 5),
        execution_record("fork-agent", "fork-agent", 4),
        execution_record("root", "root", 3),
        execution_record("fork", "fork", 2),
    ];
    let mut metadata = SessionCatalogMetadata::new();
    metadata.insert(
        "root".into(),
        catalog::SessionMetadata {
            title: Some("Renamed root".into()),
            ..catalog::SessionMetadata::default()
        },
    );

    let groups = recent_run_groups(records, &sessions, &metadata);
    let projection = groups
        .iter()
        .map(|group| {
            (
                group.session_id.as_str(),
                group.title.as_str(),
                group
                    .runs
                    .iter()
                    .map(|run| (run.session_id.as_str(), run.turn_id.as_str()))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        projection,
        vec![
            (
                "root",
                "Renamed root",
                vec![("nested-agent", "nested"), ("root", "root")]
            ),
            (
                "fork",
                "Untitled",
                vec![("fork-agent", "fork-agent"), ("fork", "fork")]
            )
        ]
    );
}

#[test]
fn recent_runs_omit_metadata_hidden_roots() {
    let sessions = vec![
        session_summary("hidden", None, true, None),
        session_summary("hidden-agent", Some("hidden"), false, None),
        session_summary("shown", None, true, Some("  Shown thread  ")),
    ];
    let records = vec![
        execution_record("hidden-agent", "hidden", 2),
        execution_record("shown", "shown", 1),
    ];
    let mut metadata = SessionCatalogMetadata::new();
    metadata.insert(
        "hidden".into(),
        catalog::SessionMetadata {
            hidden: true,
            ..catalog::SessionMetadata::default()
        },
    );

    let groups = recent_run_groups(records, &sessions, &metadata);

    assert!(matches!(
        groups.as_slice(),
        [SessionRunGroup { session_id, title, runs }]
            if session_id == "shown" && title == "Shown thread" && runs[0].turn_id == "shown"
    ));
}

fn session_summary(
    session_id: &str,
    parent_session_id: Option<&str>,
    catalog_visible: bool,
    first_user_message: Option<&str>,
) -> SessionSummary {
    SessionSummary {
        session_id: session_id.into(),
        session_context: SessionContext {
            bot_id: "test-bot".into(),
            ..SessionContext::default()
        },
        parent_session_id: parent_session_id.map(str::to_owned),
        parent_sequence: parent_session_id.map(|_| 0),
        sequence: 0,
        catalog_visible,
        first_user_message: first_user_message.map(str::to_owned),
        execution_stats: ExecutionStats::default(),
        created_at: 0,
        updated_at: 0,
    }
}

fn execution_record(session_id: &str, turn_id: &str, started_at_ms: i64) -> ExecutionRecord {
    ExecutionRecord {
        session_id: session_id.into(),
        submission_id: format!("submission-{turn_id}"),
        turn_id: turn_id.into(),
        started_at_ms,
        finished_at_ms: started_at_ms,
        elapsed_ms: 0,
        outcome: ExecutionOutcome::Completed,
        model_calls: 0,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: TokenUsage::default(),
    }
}
