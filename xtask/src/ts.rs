//! JSON Schema → TypeScript (M10.6).
//!
//! docs/10 asks for a generated TS SDK "from OpenAPI + AEP schemas". The types have to be
//! *generated* rather than written, for the reason the schemas themselves are generated: a
//! hand-maintained mirror of a protocol is a mirror that is wrong the first week nobody notices.
//!
//! Hand-rolled emitter rather than a Node toolchain in CI. The subset that appears in our schemas
//! is small and closed — objects, refs, nullable `anyOf`, string enums, `oneOf` unions with a
//! `const` discriminator — and the alternative is making a Rust repo's CI depend on npm to check
//! that a file it generates has not drifted.
//!
//! Unknown constructs emit `unknown`, never `any`: `any` silently switches off the checking the
//! file exists to provide, so a construct the emitter does not understand must be visible to the
//! person using it.

use serde_json::Value;
use std::fmt::Write;

/// Render every `$defs` entry plus the root object as exported declarations.
pub fn emit(schema: &Value, root_name: &str, header: &str) -> Result<String, String> {
    let mut out = String::new();
    out.push_str(header);

    // `$defs` first, in schema order — a reader meets a type before the type that uses it, and the
    // file diffs stably when a definition is added in the middle.
    if let Some(defs) = schema.get("$defs").and_then(Value::as_object) {
        for (name, def) in defs {
            declaration(&mut out, name, def)?;
        }
    }
    declaration(&mut out, root_name, schema)?;
    Ok(out)
}

fn declaration(out: &mut String, name: &str, schema: &Value) -> Result<(), String> {
    if let Some(doc) = schema.get("description").and_then(Value::as_str) {
        out.push_str(&doc_comment(doc, ""));
    }

    // An object with properties becomes an interface; everything else is a type alias. Both are
    // exported, because a type you cannot name is a type you end up re-declaring.
    if schema.get("properties").is_some() {
        writeln!(out, "export interface {name} {{").map_err(|e| e.to_string())?;
        properties(out, schema, "  ")?;
        out.push_str("}\n\n");
    } else {
        let ty = type_of(schema, "")?;
        // A multi-line union starts on its own line, so the variants line up under each other
        // rather than the first one hanging off the `=`.
        let sep = if ty.starts_with('\n') { "=" } else { "= " };
        writeln!(out, "export type {name} {sep}{ty};\n").map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn properties(out: &mut String, schema: &Value, indent: &str) -> Result<(), String> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    for (field, spec) in props {
        if let Some(doc) = spec.get("description").and_then(Value::as_str) {
            out.push_str(&doc_comment(doc, indent));
        }
        // Optional in the schema means optional in TS. Marking everything required would make the
        // generated types reject payloads the server happily accepts.
        let opt = if required.contains(&field.as_str()) {
            ""
        } else {
            "?"
        };
        let ty = type_of(spec, indent)?;
        writeln!(out, "{indent}{}{opt}: {ty};", quote_key(field)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn type_of(schema: &Value, indent: &str) -> Result<String, String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return Ok(ref_name(reference));
    }
    if let Some(constant) = schema.get("const") {
        return Ok(literal(constant));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        // A string enum becomes a union of literals rather than a TS `enum`: literals are
        // structurally comparable with the JSON that produced them, and a TS `enum` is a runtime
        // object nobody asked for.
        let union: Vec<String> = values.iter().map(literal).collect();
        return Ok(union.join(" | "));
    }
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array) {
        return union_of(variants, indent);
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        return union_of(variants, indent);
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("string") => Ok("string".into()),
        Some("integer") | Some("number") => Ok("number".into()),
        Some("boolean") => Ok("boolean".into()),
        Some("null") => Ok("null".into()),
        Some("array") => {
            let items = schema.get("items").unwrap_or(&Value::Null);
            let inner = if items.is_null() {
                "unknown".to_string()
            } else {
                type_of(items, indent)?
            };
            Ok(format!("{inner}[]"))
        }
        Some("object") => {
            if schema.get("properties").is_some() {
                let mut inline = String::from("{\n");
                properties(&mut inline, schema, &format!("{indent}  "))?;
                write!(inline, "{indent}}}").map_err(|e| e.to_string())?;
                Ok(inline)
            } else {
                // A free-form object. `unknown` values rather than `any`: the caller must narrow.
                Ok("Record<string, unknown>".into())
            }
        }
        // `true`/`{}` — anything goes. Said out loud rather than papered over.
        _ => Ok("unknown".into()),
    }
}

fn union_of(variants: &[Value], indent: &str) -> Result<String, String> {
    // Variants of a multi-line union are rendered two levels in, so `| {` sits under the `=` and
    // the fields sit under the `{`. Rendering them flat is valid TypeScript and unreadable.
    let nested = format!("{indent}    ");
    let mut parts = Vec::new();
    for variant in variants {
        parts.push(type_of(variant, &nested)?);
    }
    parts.dedup();

    // One line for `T | null` — the common case, where a line break per variant would bury the
    // field it belongs to.
    if parts.len() > 2 || parts.iter().any(|p| p.contains('\n')) {
        let mut out = String::new();
        for part in parts {
            out.push_str(&format!("\n{indent}  | {part}"));
        }
        Ok(out)
    } else {
        Ok(parts.join(" | "))
    }
}

fn ref_name(reference: &str) -> String {
    reference
        .rsplit('/')
        .next()
        .unwrap_or(reference)
        .to_string()
}

fn literal(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{s}\""),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// Quote a key that is not a valid TS identifier, so a field named `content-type` is emitted as
/// `"content-type"` rather than as a syntax error.
fn quote_key(field: &str) -> String {
    let ident = !field.is_empty()
        && !field.starts_with(|c: char| c.is_ascii_digit())
        && field
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ident {
        field.to_string()
    } else {
        format!("\"{field}\"")
    }
}

fn doc_comment(text: &str, indent: &str) -> String {
    let mut out = format!("{indent}/**\n");
    for line in text.lines() {
        out.push_str(&format!("{indent} * {line}\n"));
    }
    out.push_str(&format!("{indent} */\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(schema: Value) -> String {
        emit(&schema, "Root", "").unwrap()
    }

    #[test]
    fn required_and_optional_fields_differ() {
        // The failure this prevents: generated types that reject a payload the server accepts.
        let ts = render(json!({
            "type": "object",
            "properties": { "a": {"type": "string"}, "b": {"type": "string"} },
            "required": ["a"],
        }));
        assert!(ts.contains("a: string;"));
        assert!(ts.contains("b?: string;"));
    }

    #[test]
    fn a_nullable_anyof_is_one_line() {
        let ts = render(json!({
            "type": "object",
            "properties": { "turn_id": {"anyOf": [{"$ref": "#/$defs/TurnId"}, {"type": "null"}]} },
        }));
        assert!(ts.contains("turn_id?: TurnId | null;"), "{ts}");
    }

    #[test]
    fn a_multiline_union_is_indented_under_its_declaration() {
        // Valid TypeScript either way; unreadable one of them, and this file is documentation as
        // much as it is types.
        let ts = render(json!({
            "oneOf": [
                {"type": "object", "properties": {"kind": {"const": "user"}}, "required": ["kind"]},
                {"type": "object", "properties": {"kind": {"const": "tool"}, "name": {"type": "string"}}, "required": ["kind", "name"]},
                {"type": "object", "properties": {"kind": {"const": "web"}}, "required": ["kind"]}
            ]
        }));
        assert!(ts.contains("export type Root =\n  | {\n"), "{ts}");
        assert!(ts.contains("\n      kind: \"user\";\n    }"), "{ts}");
    }

    #[test]
    fn a_string_enum_becomes_a_union_of_literals() {
        // Not a TS `enum`: literals compare structurally with the JSON that produced them, and an
        // enum is a runtime object nobody asked for.
        let ts = render(json!({"enum": ["allow", "deny"]}));
        assert!(
            ts.contains(r#"export type Root = "allow" | "deny";"#),
            "{ts}"
        );
    }

    #[test]
    fn a_oneof_with_a_const_tag_becomes_a_discriminated_union() {
        let ts = render(json!({
            "oneOf": [
                {"type": "object", "properties": {"kind": {"const": "text"}, "text": {"type": "string"}}, "required": ["kind", "text"]},
                {"type": "object", "properties": {"kind": {"const": "image"}, "url": {"type": "string"}}, "required": ["kind", "url"]},
                {"type": "object", "properties": {"kind": {"const": "tool"}}, "required": ["kind"]}
            ]
        }));
        assert!(ts.contains(r#"kind: "text";"#), "{ts}");
        assert!(ts.contains(r#"kind: "image";"#), "{ts}");
    }

    #[test]
    fn a_ref_uses_the_definition_name() {
        let ts = render(json!({
            "type": "object",
            "properties": {"session_id": {"$ref": "#/$defs/SessionId"}},
            "required": ["session_id"],
        }));
        assert!(ts.contains("session_id: SessionId;"), "{ts}");
    }

    #[test]
    fn an_unrecognised_construct_is_unknown_not_any() {
        // `any` switches off the checking the file exists to provide. A construct the emitter does
        // not understand has to be visible to whoever uses it.
        let ts = render(json!({"type": "object", "properties": {"x": {}}, "required": ["x"]}));
        assert!(ts.contains("x: unknown;"), "{ts}");
        assert!(!ts.contains(": any"), "{ts}");
    }

    #[test]
    fn a_field_that_is_not_an_identifier_is_quoted() {
        let ts = render(json!({
            "type": "object",
            "properties": {"content-type": {"type": "string"}},
            "required": ["content-type"],
        }));
        assert!(ts.contains(r#""content-type": string;"#), "{ts}");
    }

    #[test]
    fn definitions_come_out_before_the_root() {
        let ts = render(json!({
            "$defs": {"SessionId": {"type": "string"}},
            "type": "object",
            "properties": {"session_id": {"$ref": "#/$defs/SessionId"}},
        }));
        assert!(
            ts.find("export type SessionId").unwrap() < ts.find("export interface Root").unwrap()
        );
    }

    #[test]
    fn descriptions_survive_as_doc_comments() {
        // The generated file is the documentation an SDK user reads; dropping the prose would make
        // it a list of field names.
        let ts = render(json!({
            "type": "object",
            "description": "Every event wears this.",
            "properties": {"seq": {"type": "integer", "description": "Gapless, per-session."}},
            "required": ["seq"],
        }));
        assert!(ts.contains("* Every event wears this."), "{ts}");
        assert!(ts.contains("* Gapless, per-session."), "{ts}");
    }
}
