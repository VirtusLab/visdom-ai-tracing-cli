//! OpenAPI schema of the wire types (`schema` feature). Run with
//! `cargo test -p visdom-ai-tracing-protocol --features schema`.
#![cfg(feature = "schema")]

use serde_json::{json, Value};
use tracevault_protocol::streaming::{BillingMode, StreamEventRequest};
use utoipa::PartialSchema;

fn schema_json<T: PartialSchema>() -> Value {
    serde_json::to_value(T::schema()).unwrap()
}

/// `billing_mode` is optional in the published schema, so generated clients
/// and validators keep accepting events from CLIs that predate it.
#[test]
fn billing_mode_is_an_optional_request_property() {
    let schema = schema_json::<StreamEventRequest>();
    assert!(
        schema["properties"].get("billing_mode").is_some(),
        "billing_mode missing from schema: {schema}"
    );
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        !required.contains(&"billing_mode"),
        "billing_mode must not be required, required = {required:?}"
    );
}

#[test]
fn billing_mode_schema_lists_the_wire_values() {
    let schema = schema_json::<BillingMode>();
    assert_eq!(
        schema["enum"],
        json!(["gateway", "api_key", "subscription", "unknown"]),
        "unexpected BillingMode schema: {schema}"
    );
}
