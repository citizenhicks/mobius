//! Shared ASCII identifier grammar; domain owners retain their prefix and reserved-name rules.

/// Letter case accepted by an ASCII identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsciiCase {
    /// Uppercase or lowercase ASCII letters.
    Any,
    /// Lowercase ASCII letters only.
    Lower,
}

/// Whether a nonempty identifier fits its byte limit and contains only the
/// selected ASCII letters, digits, and explicitly permitted separators.
pub fn valid_ascii_identifier(
    value: &str,
    max_bytes: usize,
    case: AsciiCase,
    separators: &[u8],
) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| {
            let letter = match case {
                AsciiCase::Any => byte.is_ascii_alphabetic(),
                AsciiCase::Lower => byte.is_ascii_lowercase(),
            };
            byte.is_ascii() && (letter || byte.is_ascii_digit() || separators.contains(&byte))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_separators_and_byte_limits_remain_distinct() {
        for (value, limit, case, separators, accepted) in [
            ("", 64, AsciiCase::Any, b"_-".as_slice(), false),
            ("a", 0, AsciiCase::Any, b"_-".as_slice(), false),
            ("a", 1, AsciiCase::Any, b"_-".as_slice(), true),
            ("A_1", 3, AsciiCase::Any, b"_".as_slice(), true),
            ("A_1", 3, AsciiCase::Lower, b"_".as_slice(), false),
            ("a-1", 3, AsciiCase::Lower, b"_".as_slice(), false),
            ("a-1", 3, AsciiCase::Lower, b"_-".as_slice(), true),
            ("1_a", 3, AsciiCase::Lower, b"_".as_slice(), true),
            ("é", 2, AsciiCase::Any, b"_-".as_slice(), false),
            ("é", 2, AsciiCase::Any, "é".as_bytes(), false),
            ("a b", 3, AsciiCase::Any, b"_-".as_slice(), false),
        ] {
            assert_eq!(
                valid_ascii_identifier(value, limit, case, separators),
                accepted
            );
        }
    }
}
