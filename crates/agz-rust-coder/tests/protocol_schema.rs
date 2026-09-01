use std::collections::{BTreeMap, BTreeSet};

use agz_rust_coder::{Config, server::tool_definitions};
use serde_json::{Value, json};

fn fingerprint(value: &Value) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in serde_json::to_vec(value).expect("schema serializes") {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn tool_schema_snapshot() -> BTreeMap<String, Value> {
    tool_definitions(&Config::defaults_at("/workspace"))
        .into_iter()
        .map(|tool| {
            let value = serde_json::to_value(&tool).expect("tool definition serializes");
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .expect("tool name")
                .to_owned();
            let snapshot = json!({
                "inputSchemaHash": fingerprint(value.get("inputSchema").expect("input schema")),
                "outputSchemaHash": fingerprint(value.get("outputSchema").expect("output schema")),
                "annotations": value.get("annotations").cloned().unwrap_or(Value::Null),
            });
            (name, snapshot)
        })
        .collect()
}

fn annotations(config: &Config, name: &str) -> Value {
    tool_definitions(config)
        .into_iter()
        .find(|tool| tool.name == name)
        .and_then(|tool| serde_json::to_value(tool.annotations).ok())
        .unwrap_or(Value::Null)
}

#[test]
fn tool_schemas_match_frozen_fixture() {
    let expected: BTreeMap<String, Value> = serde_json::from_str(include_str!(
        "../../../tests/reference/rust-tool-schema-fingerprints.json"
    ))
    .expect("frozen schema fixture is valid JSON");

    assert_eq!(tool_schema_snapshot(), expected);
}

#[test]
fn semantic_annotations_are_conservative_when_workspace_code_is_allowed() {
    let deny = Config::defaults_at("/workspace");
    let mut allow = deny.clone();
    allow.rust_analyzer.workspace_code = agz_rust_coder::config::WorkspaceCode::Allow;

    for name in [
        "symbol",
        "references",
        "definition",
        "symbols",
        "implementations",
        "hierarchy",
        "rename",
        "refactor",
    ] {
        assert_eq!(
            annotations(&deny, name),
            json!({
                "idempotentHint": true,
                "openWorldHint": false,
                "readOnlyHint": true,
            })
        );
        assert_eq!(
            annotations(&allow, name),
            json!({
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true,
                "readOnlyHint": false,
            })
        );
    }
}

#[test]
fn rust_inputs_preserve_the_legacy_schema_except_optional_dir() {
    let reference: BTreeMap<String, Value> =
        serde_json::from_str(include_str!("../../../tests/reference/tool-schemas.json"))
            .expect("legacy schema fixture is valid JSON");

    for tool in tool_definitions(&Config::defaults_at("/workspace")) {
        let reference_name = match tool.name.as_ref() {
            "references" | "definition" => "symbol",
            name => name,
        };
        let expected = reference
            .get(reference_name)
            .unwrap_or_else(|| panic!("missing legacy schema for {}", tool.name));
        let actual = Value::Object(tool.input_schema.as_ref().clone());

        assert_eq!(actual["type"], expected["type"], "{} type", tool.name);
        assert_eq!(
            property_names(&actual),
            property_names(expected),
            "{} properties",
            tool.name
        );
        assert_eq!(actual["additionalProperties"], Value::Bool(false));
        assert_eq!(
            required_without_dir(&actual),
            required_without_dir(expected),
            "{} required properties",
            tool.name
        );
        assert!(!required(&actual).contains("dir"));

        for property in property_names(expected) {
            if expected["properties"][&property].get("enum").is_some() {
                assert_eq!(
                    enum_values(&actual, &property),
                    enum_values(expected, &property),
                    "{} {property} enum",
                    tool.name
                );
            }
        }
    }
}

fn property_names(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .expect("object schema properties")
        .keys()
        .cloned()
        .collect()
}

fn required(schema: &Value) -> BTreeSet<&str> {
    schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn required_without_dir(schema: &Value) -> BTreeSet<&str> {
    required(schema)
        .into_iter()
        .filter(|field| *field != "dir")
        .collect()
}

fn enum_values<'a>(schema: &'a Value, property: &str) -> &'a Value {
    let property_schema = &schema["properties"][property];
    if property_schema.get("enum").is_some() {
        return &property_schema["enum"];
    }
    let definition = property_schema["$ref"]
        .as_str()
        .and_then(|reference| reference.strip_prefix("#/$defs/"))
        .expect("enum property is inline or a local definition");
    &schema["$defs"][definition]["enum"]
}
