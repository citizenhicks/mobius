use serde_json::json;

use super::*;

fn identity(name: &str, arguments: &Value) -> DefaultHasher {
    RepeatGuard::call_identity(name, arguments)
}

#[test]
fn repeats_warn_once_then_stop_and_reset_with_the_turn() {
    let guard = RepeatGuard::new(2, 3);
    let same = json!({"command": "echo ok"});
    let call = |turn| {
        guard
            .observe(turn, Some(identity("bash", &same)), Some(1), None)
            .unwrap()
    };

    assert_eq!(call("a"), RepeatDecision::Continue);
    // Real work between repeats does not reset the count.
    assert_eq!(
        guard
            .observe(
                "a",
                Some(identity("bash", &json!({"command": "ls"}))),
                Some(1),
                None,
            )
            .unwrap(),
        RepeatDecision::Continue
    );
    assert_eq!(call("a"), RepeatDecision::Warn);
    assert!(!guard.should_stop("a").unwrap());
    assert_eq!(call("a"), RepeatDecision::Stop);
    assert!(guard.should_stop("a").unwrap());

    assert!(!guard.should_stop("b").unwrap());
    assert_eq!(call("b"), RepeatDecision::Continue);
    guard.finish_turn("a").unwrap();
    assert_eq!(call("b"), RepeatDecision::Warn);
    guard.finish_turn("b").unwrap();
    assert_eq!(call("b"), RepeatDecision::Continue);
}

#[test]
fn interleaved_parent_and_child_turns_keep_their_counts() {
    let parent = RepeatGuard::new(2, 3);
    let child = RepeatGuard::new(2, 3);
    let arguments = json!({"command": "echo ok"});
    let call = |guard: &RepeatGuard| {
        guard
            .observe("turn", Some(identity("bash", &arguments)), Some(1), None)
            .unwrap()
    };

    assert_eq!(call(&parent), RepeatDecision::Continue);
    assert_eq!(call(&child), RepeatDecision::Continue);
    assert_eq!(call(&parent), RepeatDecision::Warn);
    assert_eq!(call(&child), RepeatDecision::Warn);
    assert_eq!(call(&parent), RepeatDecision::Stop);
    assert!(parent.should_stop("turn").unwrap());
    assert!(!child.should_stop("turn").unwrap());
    assert_eq!(call(&child), RepeatDecision::Stop);
    child.finish_turn("turn").unwrap();
    assert!(parent.should_stop("turn").unwrap());
    assert!(!child.should_stop("turn").unwrap());
}

#[test]
fn hosted_completions_count_once_per_attempt_and_keep_unknown_results_distinct() {
    let guard = RepeatGuard::new(2, 3);
    let arguments = json!({"query":"rust docs"});
    let call = |step, id, output| {
        guard
            .observe(
                "turn",
                Some(identity("web_search", &arguments)),
                output,
                Some((step, id)),
            )
            .unwrap()
    };
    assert_eq!(call("step-1", "call", None), RepeatDecision::Continue);
    assert_eq!(call("step-1", "call", None), RepeatDecision::Duplicate);
    assert_eq!(
        call("step-1", "different", Some(0)),
        RepeatDecision::Continue
    );
    assert_eq!(call("step-2", "call", None), RepeatDecision::Warn);
    assert_eq!(call("step-2", "call", None), RepeatDecision::Duplicate);
    assert_eq!(call("step-3", "call", None), RepeatDecision::Stop);
}

#[test]
fn hosted_rewording_with_the_same_evidence_does_not_count_as_progress() {
    let guard = RepeatGuard::new(2, 3);
    let call = |id, arguments: &Value, result| {
        guard
            .observe(
                "turn",
                RepeatGuard::hosted_identity("web_search", Some(arguments), result),
                result,
                Some(("step", id)),
            )
            .unwrap()
    };
    assert_eq!(
        call("1", &json!({"query":"rust docs"}), Some(1)),
        RepeatDecision::Continue
    );
    assert_eq!(
        call("2", &json!({"query":"rust documentation"}), Some(1)),
        RepeatDecision::Warn
    );
    assert_eq!(
        call("3", &json!({"query":"rust docs"}), Some(2)),
        RepeatDecision::Continue
    );
    assert_eq!(
        call("4", &json!({"query":"rust reference"}), Some(1)),
        RepeatDecision::Stop
    );
    let code = |source| {
        RepeatGuard::hosted_identity("code_execution", Some(&json!({"code": source})), Some(1))
            .unwrap()
            .finish()
    };
    assert_ne!(code("write_a()"), code("write_b()"));
}

#[test]
fn search_normalization_preserves_quoted_text_urls_and_filter_changes() {
    for (query, expected) in [
        ("  rust\t docs  ", "rust docs"),
        ("rust  \"a  b\"  docs", "rust \"a  b\" docs"),
        ("site:'a  b'  docs", "site:'a  b' docs"),
        ("what's  new", "what's new"),
        ("a\\  b", "a\\  b"),
        (
            " https://example.test/A?q=B&c=D  ",
            "https://example.test/A?q=B&c=D",
        ),
    ] {
        assert_eq!(normalize_query(query), expected);
    }
    let hash = |name, arguments: &Value| {
        RepeatGuard::hosted_identity(name, Some(arguments), None)
            .unwrap()
            .finish()
    };
    let first = json!({"queries":["rust  docs"],"filter":"one"});
    let equivalent = json!({"queries":[" rust docs "],"filter":"one"});
    let changed = json!({"queries":["rust docs"],"filter":"two"});
    assert_eq!(hash("web_search", &first), hash("web_search", &equivalent));
    assert_ne!(hash("web_search", &first), hash("web_search", &changed));
    assert_ne!(
        hash("find_in_page", &first),
        hash("find_in_page", &equivalent)
    );
}

#[test]
fn opaque_hosted_arguments_do_not_claim_repetition_and_ingress_is_bounded() {
    let guard = RepeatGuard::new(1, 1);
    assert!(RepeatGuard::hosted_identity("web_search", None, Some(42)).is_none());
    for index in 0..MAX_HOSTED_EXECUTIONS_PER_TURN {
        assert_eq!(
            guard
                .observe("turn", None, None, Some(("step", &index.to_string())))
                .unwrap(),
            RepeatDecision::Continue
        );
    }
    assert!(
        guard
            .observe("turn", None, None, Some(("step", "overflow")))
            .is_err()
    );
    assert!(guard.should_stop("turn").unwrap());
}

#[test]
fn a_changed_result_is_not_a_repeat() {
    let guard = RepeatGuard::new(1, 2);
    let arguments = json!({"command": "date"});
    assert_eq!(
        guard
            .observe("a", Some(identity("bash", &arguments)), Some(1), None,)
            .unwrap(),
        RepeatDecision::Warn
    );
    assert_eq!(
        guard
            .observe("a", Some(identity("bash", &arguments)), Some(2), None)
            .unwrap(),
        RepeatDecision::Warn
    );
    assert!(!guard.should_stop("a").unwrap());
}

#[test]
fn concurrent_turns_do_not_clobber_each_other() {
    let guard = RepeatGuard::new(2, 3);
    let args = json!({"command": "pwd"});
    assert_eq!(
        guard
            .observe("turn-a", Some(identity("bash", &args)), Some(1), None)
            .unwrap(),
        RepeatDecision::Continue
    );
    assert_eq!(
        guard
            .observe("turn-b", Some(identity("bash", &args)), Some(1), None)
            .unwrap(),
        RepeatDecision::Continue
    );
    assert_eq!(
        guard
            .observe("turn-a", Some(identity("bash", &args)), Some(1), None)
            .unwrap(),
        RepeatDecision::Warn
    );
    assert_eq!(
        guard
            .observe("turn-a", Some(identity("bash", &args)), Some(1), None)
            .unwrap(),
        RepeatDecision::Stop
    );
    assert!(guard.should_stop("turn-a").unwrap());
    assert!(!guard.should_stop("turn-b").unwrap());
    guard.finish_turn("turn-a").unwrap();
    assert!(!guard.should_stop("turn-a").unwrap());
}
