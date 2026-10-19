//! Generic composer-reference token parsing and replacement.

use std::ops::Range;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReferenceToken {
    pub(super) trigger: char,
    pub(super) range: Range<usize>,
    pub(super) query: String,
}

/// Returns the active whitespace-delimited reference for `trigger`.
pub(super) fn active_reference_token(
    input: &str,
    cursor: usize,
    trigger: char,
) -> Option<ReferenceToken> {
    let (before, after) = input.split_at_checked(cursor)?;
    let start = before
        .char_indices()
        .rfind(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    let end = if after.chars().next().is_some_and(char::is_whitespace) {
        cursor
    } else {
        after
            .char_indices()
            .find(|(_, character)| character.is_whitespace())
            .map_or(input.len(), |(index, _)| cursor + index)
    };
    let query = input.get(start..end)?.strip_prefix(trigger)?;

    Some(ReferenceToken {
        trigger,
        range: start..end,
        query: query.to_owned(),
    })
}

/// Replaces an unchanged active token and returns the new UTF-8 byte cursor.
pub(super) fn replace_reference_token(
    input: &mut String,
    token: &ReferenceToken,
    replacement: &str,
) -> Option<usize> {
    let Range { start, end } = token.range;
    if input.get(start..end)?.strip_prefix(token.trigger)? != token.query {
        return None;
    }
    input.replace_range(start..end, replacement);
    Some(start + replacement.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_only_the_active_utf8_reference() {
        let mut input = "é read @old/path then @other".to_owned();
        let token = active_reference_token(&input, 10, '@').expect("active token");
        let cursor = replace_reference_token(&mut input, &token, "\"docs/my file.md\"").unwrap();

        assert_eq!(input, "é read \"docs/my file.md\" then @other");
        assert_eq!(input.get(..cursor), Some("é read \"docs/my file.md\""));
        assert!(active_reference_token("email a@b.test", 10, '@').is_none());
    }
}
