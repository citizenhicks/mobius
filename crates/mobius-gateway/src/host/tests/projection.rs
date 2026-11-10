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
