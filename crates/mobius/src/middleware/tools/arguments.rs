//! Tool arguments read against the tool's own JSON schema before any handler sees them.
//!
//! Some providers re-encode arguments on the way out of the model: XML-style tool calls turn
//! arrays into `{"item": …}` maps and numbers or booleans into strings, and others send a
//! nested array or object as a JSON string. Each repair below happens only where the schema
//! names a different type, so well-formed arguments pass through untouched. What still does
//! not match is reported with its path, so the model can fix that one field.

use std::fmt::Write as _;

use serde_json::{Map, Number, Value};

/// Repairs `value` in place against `schema` and reports whether anything changed, or
/// names the first field of the wrong type.
pub(crate) fn normalize(value: &mut Value, schema: &Value) -> Result<bool, String> {
    walk(value, schema, &mut String::new())
}

fn walk(value: &mut Value, schema: &Value, path: &mut String) -> Result<bool, String> {
    let schema = branch(value, schema);
    let types = types(schema);
    let mut changed = false;
    if !types.is_empty() && !types.iter().any(|kind| matches(value, kind)) {
        if let Some(repaired) = types.iter().find_map(|kind| repair(value, kind)) {
            *value = repaired;
            changed = true;
        } else {
            let field = if path.is_empty() {
                "arguments"
            } else {
                path.as_str()
            };
            return Err(format!(
                "`{field}`: expected {}, got {}",
                expected(&types),
                kind_of(value)
            ));
        }
    }
    match value {
        Value::Array(items) => {
            let Some(item_schema) = schema.get("items") else {
                return Ok(changed);
            };
            for (index, item) in items.iter_mut().enumerate() {
                let length = path.len();
                write!(path, "[{index}]").map_err(|error| error.to_string())?;
                changed |= walk(item, item_schema, path)?;
                path.truncate(length);
            }
        }
        Value::Object(fields) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return Ok(changed);
            };
            for (name, field) in fields.iter_mut() {
                let Some(field_schema) = properties.get(name) else {
                    continue;
                };
                let length = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(name);
                changed |= walk(field, field_schema, path)?;
                path.truncate(length);
            }
        }
        _ => {}
    }
    Ok(changed)
}

/// The `anyOf`/`oneOf` branch `value` belongs to: the one whose `type` property it names,
/// else the only branch whose type it already has. Ambiguous unions stay with the handler.
fn branch<'a>(value: &Value, schema: &'a Value) -> &'a Value {
    let Some(branches) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
    else {
        return schema;
    };
    let tag = value.get("type").and_then(Value::as_str);
    let names = |branch: &Value| {
        let tag_schema = branch.get("properties")?.get("type")?;
        tag_schema
            .get("const")
            .and_then(Value::as_str)
            .map(|name| tag == Some(name))
            .or_else(|| {
                let names = tag_schema.get("enum")?.as_array()?;
                Some(names.iter().any(|name| name.as_str() == tag))
            })
    };
    if let Some(branch) = branches.iter().find(|branch| names(branch) == Some(true)) {
        return branch;
    }
    let mut matching = branches
        .iter()
        .filter(|branch| types(branch).iter().any(|kind| matches(value, kind)));
    match (matching.next(), matching.next()) {
        (Some(branch), None) => branch,
        _ => schema,
    }
}

fn types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn matches(value: &Value, kind: &str) -> bool {
    match kind {
        "array" => value.is_array(),
        "object" => value.is_object(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        _ => true,
    }
}

/// `value` re-read as `kind`, when it is one of the encodings providers are known to produce.
fn repair(value: &mut Value, kind: &str) -> Option<Value> {
    match (kind, value) {
        ("array", Value::Object(fields)) => unwrap_item(fields),
        ("array" | "object", Value::String(text)) => serde_json::from_str::<Value>(text.trim())
            .ok()
            .filter(|parsed| matches(parsed, kind)),
        ("integer", Value::String(text)) => {
            let text = text.trim();
            text.parse::<i64>()
                .map(Value::from)
                .or_else(|_| text.parse::<u64>().map(Value::from))
                .ok()
        }
        ("number", Value::String(text)) => text.trim().parse::<Number>().ok().map(Value::Number),
        ("boolean", Value::String(text)) => match text.trim() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    }
}

/// `{"item": [a, b]}` is `[a, b]` and `{"item": a}` is `[a]`.
fn unwrap_item(fields: &mut Map<String, Value>) -> Option<Value> {
    if fields.len() != 1 {
        return None;
    }
    fields.remove("item").map(|item| match item {
        Value::Array(_) => item,
        _ => Value::Array(vec![item]),
    })
}

fn expected(types: &[&str]) -> String {
    let names = types.iter().map(|kind| article(kind)).collect::<Vec<_>>();
    names.join(" or ")
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn article(kind: &str) -> &'static str {
    match kind {
        "array" => "an array",
        "object" => "an object",
        "string" => "a string",
        "boolean" => "a boolean",
        "integer" => "an integer",
        "number" => "a number",
        "null" => "null",
        _ => "a value",
    }
}

#[cfg(test)]
mod tests {
    use super::normalize;
    use serde_json::json;

    fn show_ui() -> serde_json::Value {
        let mut definition: serde_json::Value =
            toml::from_str(include_str!("../intelligent_ui.toml")).expect("UI manifest");
        definition["show_ui"]["tool"]["parameters"].take()
    }

    #[test]
    fn unwraps_item_maps_at_every_depth() {
        let mut value = json!({
            "title": "Tokyo",
            "blocks": {"item": [
                {"type": "text", "markdown": "Day 1"},
                {"type": "list", "items": {"item": {"title": "Senso-ji"}}}
            ]}
        });
        normalize(&mut value, &show_ui()).expect("repaired");
        assert_eq!(
            value,
            json!({
                "title": "Tokyo",
                "blocks": [
                    {"type": "text", "markdown": "Day 1"},
                    {"type": "list", "items": [{"title": "Senso-ji"}]}
                ]
            })
        );
    }

    #[test]
    fn reads_strings_as_the_type_the_schema_names() {
        let mut value = json!({
            "blocks": "[{\"type\": \"text\", \"text\": \"hi\"}]",
            "limit": "240000",
            "wait": "true"
        });
        normalize(&mut value, &json!({"type": "object", "properties": {
            "blocks": {"type": "array"}, "limit": {"type": "integer"}, "wait": {"type": "boolean"}
        }})).expect("repaired");
        assert_eq!(value["blocks"][0]["text"], "hi");
        assert_eq!(value["limit"], 240_000);
        assert_eq!(value["wait"], true);
    }

    #[test]
    fn number_repair_keeps_large_integers_exact() {
        let mut value = json!("9007199254740993");
        normalize(&mut value, &json!({"type": "number"})).expect("number");
        assert_eq!(value, json!(9_007_199_254_740_993_u64));
    }

    #[test]
    fn does_not_choose_between_ambiguous_object_branches() {
        let mut value = json!({"value": 3});
        normalize(
            &mut value,
            &json!({"anyOf": [
                {"type": "object", "properties": {"value": {"type": "string"}}},
                {"type": "object", "properties": {"value": {"type": "integer"}}}
            ]}),
        )
        .expect("valid second branch");
        assert_eq!(value, json!({"value": 3}));
    }

    #[test]
    fn leaves_well_formed_arguments_alone_and_names_the_bad_field() {
        let mut value = json!({"blocks": [{"type": "list", "items": [{"title": 3}]}]});
        assert_eq!(
            normalize(&mut value, &show_ui()),
            Err("`blocks[0].items[0].title`: expected a string, got a number".into())
        );
        let mut value = json!({"title": "ok", "blocks": [{"type": "text", "markdown": "x"}]});
        let before = value.clone();
        normalize(&mut value, &show_ui()).expect("valid");
        assert_eq!(value, before);
    }
}
