use super::*;

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
