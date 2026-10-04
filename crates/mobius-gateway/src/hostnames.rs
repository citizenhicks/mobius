//! ASCII DNS label syntax shared by configured and discovered tunnel endpoints.

pub(crate) fn valid_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && label
            .bytes()
            .next_back()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_syntax_is_case_insensitive_but_rejects_empty_edges_and_non_ascii() {
        for label in ["Gateway", "gateway-1", "1"] {
            assert!(valid_label(label));
        }
        for label in ["", "-gateway", "gateway-", "gateway.example", "gâteway"] {
            assert!(!valid_label(label));
        }
        assert!(valid_label(&"a".repeat(63)));
        assert!(!valid_label(&"a".repeat(64)));
    }
}
