//! The JSON Schema of the engine's requests and responses (ADR-121 D6).
//!
//! The schema is generated from the Rust types with `schemars`, behind
//! the `schema` cargo feature, and checked in as
//! `docs/reference/ipc-schema.json`. Every build carries that file
//! ([`SCHEMA_JSON`]) and the HTTP door serves it at `GET /v1/schema`, so
//! the default build needs no `schemars`.
//!
//! Each variant of `RequestPacket` and `ResponsePacket` carries its mark
//! from [`super::stability`] as `x-stability`.
//!
//! Regenerate after changing a wire type:
//!
//! ```text
//! PEKO_WRITE_IPC_SCHEMA=1 cargo test -p peko --lib --features schema ipc::schema
//! ```
//!
//! Without the variable, the same test fails when the file is stale.

/// `docs/reference/ipc-schema.json`, built into the binary.
pub const SCHEMA_JSON: &str = include_str!("../../../../docs/reference/ipc-schema.json");

/// Generate the schema document from the Rust types.
///
/// # Panics
///
/// When a request or response variant has no `type` constant or no mark
/// in `ipc/stability.rs` — both are bugs the tests below catch.
#[cfg(feature = "schema")]
#[must_use]
pub fn generate() -> serde_json::Value {
    use super::packet::{AuthenticatedRequest, RequestPacket, ResponsePacket};
    use serde_json::{json, Value};

    let mut generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let request = generator.subschema_for::<RequestPacket>();
    let response = generator.subschema_for::<ResponsePacket>();
    let envelope = generator.subschema_for::<AuthenticatedRequest>();
    let mut defs = generator.take_definitions(true);

    for (name, mark) in [
        (
            "RequestPacket",
            super::stability::of_request as fn(&str) -> _,
        ),
        ("ResponsePacket", super::stability::of_response),
    ] {
        let variants = defs
            .get_mut(name)
            .and_then(|d| d.get_mut("oneOf"))
            .and_then(Value::as_array_mut)
            .unwrap_or_else(|| panic!("{name} has no oneOf in its schema"));
        for variant in variants {
            let wire = variant
                .pointer("/properties/type/const")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("a {name} variant has no `type` constant: {variant}"))
                .to_string();
            let mark = mark(&wire).unwrap_or_else(|| panic!("{wire} is not in ipc/stability.rs"));
            variant["x-stability"] = Value::from(mark.as_str());
        }
    }

    let _ = (request, response, envelope);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Peko engine requests and responses",
        "description": "Every request a client sends the engine (`RequestPacket`) and every packet it answers with (`ResponsePacket`), over the local socket or the HTTP door (`POST /v1/call`, ADR-121). `AuthenticatedRequest` is a request as the socket carries it, with an optional `auth` header beside its fields. Over HTTP `request_id` may be left out. Each variant's `x-stability` is `stable` (breaks only in a minor version, ADR-102) or `internal` (may change in a patch). Generated from the Rust types: do not edit by hand.",
        "oneOf": [
            {"$ref": "#/$defs/RequestPacket"},
            {"$ref": "#/$defs/ResponsePacket"},
        ],
        "$defs": Value::Object(defs),
    })
}

/// The generated document as the checked-in file holds it.
#[cfg(feature = "schema")]
#[must_use]
pub fn generate_text() -> String {
    let mut text = serde_json::to_string_pretty(&generate()).expect("a schema serializes");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_schema_is_json() {
        let doc: serde_json::Value = serde_json::from_str(SCHEMA_JSON).unwrap();
        assert!(doc["$defs"]["RequestPacket"].is_object());
        assert!(doc["$defs"]["ResponsePacket"].is_object());
    }

    #[cfg(feature = "schema")]
    mod generated {
        use super::super::*;
        use crate::ipc::packet::{PrincipalLogMessage, RequestPacket, ResponsePacket, SendAs};
        use serde_json::{json, Value};

        /// The checked-in file is what the types generate. Run with
        /// `PEKO_WRITE_IPC_SCHEMA=1` to rewrite it.
        #[test]
        fn the_checked_in_schema_is_current() {
            let fresh = generate_text();
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/reference/ipc-schema.json"
            );
            if std::env::var_os("PEKO_WRITE_IPC_SCHEMA").is_some() {
                std::fs::write(path, &fresh).unwrap();
                return;
            }
            let on_disk = std::fs::read_to_string(path).unwrap_or_default();
            assert!(
                on_disk == fresh,
                "docs/reference/ipc-schema.json is stale; regenerate it with \
                 PEKO_WRITE_IPC_SCHEMA=1 cargo test -p peko --lib --features schema ipc::schema"
            );
        }

        #[test]
        fn every_variant_carries_its_mark() {
            let doc: Value = serde_json::from_str(&generate_text()).unwrap();
            for (name, count) in [("RequestPacket", 129), ("ResponsePacket", 115)] {
                let variants = doc["$defs"][name]["oneOf"].as_array().unwrap();
                assert_eq!(variants.len(), count, "{name} variants");
                for v in variants {
                    let mark = v["x-stability"].as_str().unwrap();
                    assert!(mark == "stable" || mark == "internal", "{v}");
                }
            }
            let ping = doc["$defs"]["RequestPacket"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["properties"]["type"]["const"] == "ping")
                .unwrap();
            assert_eq!(ping["x-stability"], "stable");
        }

        /// A validator for one definition of the generated document.
        fn validator(def: &str) -> jsonschema::Validator {
            let mut doc: Value = serde_json::from_str(&generate_text()).unwrap();
            doc["oneOf"] = Value::Null;
            let doc = json!({
                "$schema": doc["$schema"],
                "$ref": format!("#/$defs/{def}"),
                "$defs": doc["$defs"],
            });
            jsonschema::validator_for(&doc).unwrap()
        }

        fn assert_valid(validator: &jsonschema::Validator, packet: &Value) {
            let errors: Vec<String> = validator
                .iter_errors(packet)
                .map(|e| e.to_string())
                .collect();
            assert!(errors.is_empty(), "{packet}\n  {}", errors.join("\n  "));
        }

        fn wire<T: serde::Serialize>(value: &T) -> Value {
            serde_json::to_value(value).unwrap()
        }

        /// Real packets, serialized by the engine's own types, validate.
        #[test]
        fn real_requests_validate() {
            let requests = validator("RequestPacket");
            let packets = [
                RequestPacket::Ping { request_id: 1 },
                RequestPacket::PrincipalSend {
                    request_id: 2,
                    name: "desk".into(),
                    message: "here is my note".into(),
                    override_model: None,
                    attachments: vec!["upload:5ccd2a7eb22547b4b588d99d2561df8f".into()],
                    send_as: Some(SendAs {
                        actor: "visitor:+15550100".into(),
                        name: Some("Ada".into()),
                    }),
                    answers: None,
                    reply_to: None,
                },
                RequestPacket::PrincipalSendStream {
                    request_id: 3,
                    name: "desk".into(),
                    message: "hi".into(),
                    override_model: Some("haiku".into()),
                    attachments: Vec::new(),
                    send_as: None,
                    answers: None,
                    reply_to: None,
                },
                // Built from its wire form: it has many optional fields.
                serde_json::from_value(json!({
                    "type": "principal_log",
                    "request_id": 4,
                    "name": "desk",
                    "peer": {"kind": "visitor", "id": "local:+15550100"},
                    "limit": 20,
                }))
                .unwrap(),
            ];
            for packet in &packets {
                assert_valid(&requests, &wire(packet));
            }
            // The socket's envelope: an `auth` header beside the fields.
            let envelope = validator("AuthenticatedRequest");
            assert_valid(&envelope, &json!({"type": "ping", "request_id": 1}));
            assert_valid(
                &envelope,
                &json!({"auth": {"credential": {"type": "api_key", "token": "pkr_x"}}, "type": "ping", "request_id": 1}),
            );
            // And what the wire refuses, the schema refuses.
            for bad in [
                json!({"type": "ping"}),
                json!({"type": "no_such_request", "request_id": 1}),
                json!({"type": "principal_send", "request_id": 1, "name": "desk"}),
                json!({"type": "principal_log", "request_id": 1, "name": "d", "peer": "visitor:local:x"}),
            ] {
                assert!(!requests.is_valid(&bad), "accepted {bad}");
            }
        }

        #[test]
        fn real_responses_validate() {
            let responses = validator("ResponsePacket");
            let attachment = peko_protocol::channel::Attachment {
                name: "note.txt".into(),
                path: "/data/pekos/desk/contacts/dm_1/attachments/2026-11-24/709cc861-note.txt"
                    .into(),
                mime: "text/plain".into(),
                size: 23,
                sha256: None,
            };
            let mut message = PrincipalLogMessage::new(
                peko_auth::ActorRef::Visitor("+15550100".into()),
                "here is my note",
                None,
            );
            message.attachments = vec![attachment.clone()];
            let event: peko_protocol::channel::ChannelEvent = serde_json::from_value(json!({
                "kind": "posted",
                "channel": "dm_1",
                "author": "visitor:local:+15550100",
                "parent": null,
                "text": "here is my note",
                "at": "2026-11-24T03:52:32Z",
                "attachments": [wire(&attachment)],
            }))
            .unwrap();
            let packets = [
                ResponsePacket::Pong {
                    request_id: 1,
                    uptime_secs: 3,
                    version: "0.5.0".into(),
                },
                // A streamed reply, packet by packet.
                ResponsePacket::Heartbeat { request_id: 2 },
                ResponsePacket::PrincipalSentChunk {
                    request_id: 2,
                    delta: "We open at nine".into(),
                },
                ResponsePacket::Done {
                    request_id: 2,
                    success: false,
                    error: Some("[queued] desk is waiting".into()),
                },
                ResponsePacket::Error {
                    request_id: 3,
                    message: "[permission_denied] local callers only".into(),
                },
                // Posts with files.
                ResponsePacket::PrincipalSentAppended {
                    request_id: 4,
                    to: peko_auth::ActorRef::Visitor("local:+15550100".into()),
                    channel: "dm_1".into(),
                    message: message.clone(),
                },
                ResponsePacket::ChannelEventReceived {
                    request_id: 5,
                    channel: peko_protocol::channel::ChannelId("dm_1".into()),
                    event,
                },
            ];
            for packet in &packets {
                assert_valid(&responses, &wire(packet));
            }
        }
    }
}
