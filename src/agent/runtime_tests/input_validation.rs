//! Agent input-boundary validation tests.

use crate::protocol::{Op, Submission};

#[tokio::test]
async fn checked_submissions_preserve_ingress_order_and_raw_input_still_fails_early() {
    use crate::agent::{ValidatedSubmission, submission_channel};
    let (sender, mut inbox) = submission_channel(2);
    let invalid = Submission {
        id: "".into(),
        op: Op::Interrupt {
            turn_id: "turn".into(),
        },
    };
    assert!(sender.send(invalid).is_err());
    let checked = ValidatedSubmission::new(Submission {
        id: "checked".into(),
        op: Op::Interrupt {
            turn_id: "turn".into(),
        },
    })
    .unwrap();
    sender.send_validated(checked).unwrap();
    assert_eq!(&*inbox.recv().await.unwrap().submission.id, "checked");
    assert_eq!(inbox.last_sequence, 1);
}
