//! A JSON Schema subset, enough for `json-bench` (docs/19 M19.1).
//!
//! Hand-written and deliberately partial. The alternative is a validator crate, and what a bench
//! needs is not spec compliance but a failure message a person can act on: "missing required field
//! `path`" is a bug report, `#/properties/args: does not match` is a puzzle.
//!
//! Supported: `type` (object/array/string/integer/number/boolean/null), `properties`, `required`,
//! `items`, `enum`, `const`, and nesting of all of them. Anything else in a schema is **ignored**,
//! which is stated here because an ignored constraint is a case scored as passing — so the corpus
//! only uses what this checks, and a test asserts that.

use serde_json::Value;

/// A validation failure, phrased for a human reading a scorecard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError(pub String);

pub fn validate(schema: &Value, value: &Value) -> Result<(), SchemaError> {
    check(schema, value, "")
}

fn check(schema: &Value, value: &Value, path: &str) -> Result<(), SchemaError> {
    let at = |suffix: &str| -> String {
        if path.is_empty() {
            suffix.to_string()
        } else if suffix.is_empty() {
            path.to_string()
        } else {
            format!("{path}.{suffix}")
        }
    };
    let here = if path.is_empty() {
        "the root".to_string()
    } else {
        format!("`{path}`")
    };

    if let Some(expected) = schema.get("const") {
        if value != expected {
            return Err(SchemaError(format!("{here} must be {expected}")));
        }
    }

    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            let allowed: Vec<String> = options.iter().map(|o| o.to_string()).collect();
            return Err(SchemaError(format!(
                "{here} is {value}, which is not one of {}",
                allowed.join(", ")
            )));
        }
        // An enum pins the value; a `type` alongside it adds nothing.
        return Ok(());
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let Some(map) = value.as_object() else {
                return Err(SchemaError(format!("{here} must be an object")));
            };
            for field in schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default()
            {
                if !map.contains_key(field) {
                    return Err(SchemaError(format!(
                        "missing required field `{}`",
                        at(field)
                    )));
                }
            }
            // Only the properties the schema names. Extra keys are not an error: a model that
            // volunteers a field has produced something a caller can still use, and JSON Schema
            // agrees unless `additionalProperties: false` says otherwise.
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (field, subschema) in props {
                    if let Some(child) = map.get(field) {
                        check(subschema, child, &at(field))?;
                    }
                }
            }
            Ok(())
        }
        Some("array") => {
            let Some(items) = value.as_array() else {
                return Err(SchemaError(format!("{here} must be an array")));
            };
            if let Some(subschema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(subschema, item, &at(&format!("[{i}]")))?;
                }
            }
            Ok(())
        }
        Some("string") => value
            .is_string()
            .then_some(())
            .ok_or_else(|| SchemaError(format!("{here} must be a string, got {}", kind(value)))),
        Some("integer") => {
            // A float that is exactly an integer counts: `2.0` from a model that emitted a decimal
            // point is the right answer badly typed, and failing it would measure JSON formatting
            // rather than the shape.
            let ok = value.as_i64().is_some()
                || value.as_u64().is_some()
                || value.as_f64().is_some_and(|f| f.fract() == 0.0);
            ok.then_some(()).ok_or_else(|| {
                SchemaError(format!("{here} must be an integer, got {}", kind(value)))
            })
        }
        Some("number") => value
            .is_number()
            .then_some(())
            .ok_or_else(|| SchemaError(format!("{here} must be a number, got {}", kind(value)))),
        Some("boolean") => value
            .is_boolean()
            .then_some(())
            .ok_or_else(|| SchemaError(format!("{here} must be a boolean, got {}", kind(value)))),
        Some("null") => value
            .is_null()
            .then_some(())
            .ok_or_else(|| SchemaError(format!("{here} must be null, got {}", kind(value)))),
        // No `type`: nothing to check here, but children may still be constrained.
        _ => Ok(()),
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Every keyword this validator understands. `json_bench`'s corpus is asserted against it, so a
/// case can never be scored as passing because of a constraint that was silently ignored.
pub const SUPPORTED_KEYWORDS: &[&str] =
    &["type", "properties", "required", "items", "enum", "const"];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_missing_required_field_names_the_field() {
        // "missing required field `path`" is a bug report; "does not match" is a puzzle.
        let schema = json!({"type": "object", "required": ["path"], "properties": {"path": {"type": "string"}}});
        let err = validate(&schema, &json!({})).unwrap_err();
        assert_eq!(err.0, "missing required field `path`");
    }

    #[test]
    fn a_nested_failure_names_the_whole_path() {
        let schema = json!({"type": "object", "required": ["args"], "properties": {
            "args": {"type": "object", "required": ["path"], "properties": {"path": {"type": "string"}}}
        }});
        let err = validate(&schema, &json!({"args": {"path": 7}})).unwrap_err();
        assert!(err.0.contains("`args.path`"), "{}", err.0);
    }

    #[test]
    fn an_array_element_is_located_by_index() {
        let schema = json!({"type": "array", "items": {"type": "string"}});
        let err = validate(&schema, &json!(["a", 2])).unwrap_err();
        assert!(err.0.contains("[1]"), "{}", err.0);
    }

    #[test]
    fn an_integer_written_with_a_decimal_point_is_still_an_integer() {
        // `2.0` is the right answer badly typed. Failing it would measure formatting, not shape.
        let schema = json!({"type": "integer"});
        assert!(validate(&schema, &json!(2.0)).is_ok());
        assert!(validate(&schema, &json!(2)).is_ok());
        assert!(validate(&schema, &json!(2.5)).is_err());
    }

    #[test]
    fn an_invented_enum_member_is_rejected_and_the_options_are_listed() {
        let schema = json!({"enum": ["low", "medium", "high"]});
        let err = validate(&schema, &json!("critical")).unwrap_err();
        assert!(err.0.contains("critical"), "{}", err.0);
        assert!(err.0.contains("\"high\""), "{}", err.0);
    }

    #[test]
    fn extra_fields_are_not_a_failure() {
        // A model that volunteers a field produced something a caller can still use.
        let schema =
            json!({"type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}});
        assert!(validate(&schema, &json!({"a": "x", "b": 1})).is_ok());
    }

    #[test]
    fn an_absent_optional_field_is_fine_but_a_wrong_one_is_not() {
        let schema = json!({"type": "object", "required": ["a"], "properties": {
            "a": {"type": "string"}, "b": {"type": "integer"}
        }});
        assert!(validate(&schema, &json!({"a": "x"})).is_ok());
        assert!(validate(&schema, &json!({"a": "x", "b": "not a number"})).is_err());
    }
}
