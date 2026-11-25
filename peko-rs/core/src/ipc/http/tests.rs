//! Tests for the HTTP door (ADR-121). The door runs against a fake
//! [`DoorHost`] whose `call` hands requests to the real `auth` and
//! `device` handlers (over fake hosts), so the owner scope is checked
//! where the engine checks it.

use std::sync::Mutex;

use super::*;
use crate::ipc::handlers::auth::{AuthHandler, AuthHost};
use crate::ipc::handlers::device::{DeviceHandler, DeviceHost};
use crate::ipc::handlers::RequestHandler;
use crate::ipc::packet::{ChannelReadInfo, DevicesView, ResponsePacket};
use crate::ipc::send_response::send_response;
use peko_auth::types::ApiKeyScope;

pub(super) const OWNER_KEY: &str = "pkr_ownerkey-secret";
pub(super) const WRITE_KEY: &str = "pkr_writekey-secret";
pub(super) const ADMIN_KEY: &str = "pkr_adminkey-secret";

struct Paths(std::path::PathBuf);
impl peko_auth::host::RuntimePaths for Paths {
    fn runtime_dir(&self) -> std::path::PathBuf {
        self.0.clone()
    }
}

/// An `AuthConfig` with API keys on or off, from a temp runtime dir.
pub(super) fn auth_config(api_keys: bool) -> (tempfile::TempDir, AuthConfig) {
    let dir = tempfile::tempdir().unwrap();
    let file = peko_auth::types::AuthConfigFile {
        enable_api_key: api_keys,
        ..Default::default()
    };
    std::fs::write(
        dir.path().join("auth_config.toml"),
        toml::to_string(&file).unwrap(),
    )
    .unwrap();
    let config = AuthConfig::load(&Paths(dir.path().to_path_buf())).unwrap();
    (dir, config)
}

struct FakeAuthHost {
    config: AuthConfig,
    store: peko_auth::api_key::ApiKeyStore,
}
impl AuthHost for FakeAuthHost {
    fn auth_config(&self) -> AuthConfig {
        self.config.clone()
    }
    fn api_key_store(&self) -> Option<peko_auth::api_key::ApiKeyStore> {
        Some(self.store.clone())
    }
}

struct FakeDeviceHost;
#[async_trait]
impl DeviceHost for FakeDeviceHost {
    async fn show_code(&self) -> Result<crate::tunnel::device_sync::ShownCode, String> {
        Err("not in this test".into())
    }
    async fn join(&self, _code: &str) -> Result<(), String> {
        Err("not in this test".into())
    }
    async fn devices(&self) -> DevicesView {
        unimplemented!()
    }
    async fn identity_code(&self, _of: &str) -> (Option<String>, Option<String>) {
        (None, None)
    }
    async fn remove_device(&self, _device_id: &str) -> Result<bool, String> {
        Err("not in this test".into())
    }
    async fn decide_security(&self, _id: &str, _approve: bool) -> Result<String, String> {
        Err("not in this test".into())
    }
    async fn mark_read(
        &self,
        _channel: &str,
        _msg_id: Option<&str>,
        _at: Option<&str>,
    ) -> Result<bool, String> {
        Err("not in this test".into())
    }
    async fn read_state(&self) -> Result<Vec<ChannelReadInfo>, String> {
        Ok(Vec::new())
    }
}

pub(super) struct FakeHost {
    pub admit: bool,
    /// A peko's workspace and a channel's file store, in `_dir`.
    pub workspace: std::path::PathBuf,
    pub files_dir: std::path::PathBuf,
    pub records: Mutex<Vec<(Option<String>, String, serde_json::Value)>>,
    auth: AuthHandler,
    device: DeviceHandler,
    pub _dir: tempfile::TempDir,
}

impl FakeHost {
    pub(super) fn new() -> Self {
        let (dir, config) = auth_config(true);
        let store =
            peko_auth::api_key::ApiKeyStore::load(&Paths(dir.path().to_path_buf())).unwrap();
        let workspace = dir.path().join("pekos/desk");
        let files_dir = dir.path().join("channels/dm_1/files");
        std::fs::create_dir_all(workspace.join("attachments/2026-11-24")).unwrap();
        std::fs::create_dir_all(workspace.join("kb")).unwrap();
        std::fs::create_dir_all(&files_dir).unwrap();
        Self {
            admit: true,
            workspace,
            files_dir,
            records: Mutex::default(),
            auth: AuthHandler::new(Arc::new(FakeAuthHost { config, store })),
            device: DeviceHandler::new(Arc::new(FakeDeviceHost)),
            _dir: dir,
        }
    }
}

#[async_trait]
impl DoorHost for FakeHost {
    async fn caller_for_key(&self, key: &str) -> Option<CallerContext> {
        let scopes = match key {
            OWNER_KEY => vec![ApiKeyScope::Owner],
            WRITE_KEY => vec![ApiKeyScope::Read, ApiKeyScope::Write],
            ADMIN_KEY => vec![ApiKeyScope::Admin],
            _ => return None,
        };
        Some(CallerContext::from_api_key(
            peko_auth::api_key::ApiKeyStore::extract_key_id(key),
            scopes,
        ))
    }

    async fn admit(&self, _caller: &CallerContext) -> bool {
        self.admit
    }

    async fn call(
        &self,
        request: RequestPacket,
        caller: CallerContext,
        sink: HttpSink,
        peer: PeerAddr,
    ) -> anyhow::Result<()> {
        if self.auth.matches(&request) {
            return self.auth.handle(request, &caller, &sink, &peer).await;
        }
        if self.device.matches(&request) {
            return self.device.handle(request, &caller, &sink, &peer).await;
        }
        let request_id = request.request_id();
        match request {
            RequestPacket::Ping { .. } => {
                send_response(&sink, ResponsePacket::Heartbeat { request_id }).await?;
                send_response(
                    &sink,
                    ResponsePacket::Pong {
                        request_id,
                        uptime_secs: 1,
                        version: "test".into(),
                    },
                )
                .await
            }
            // Stands in for a request that streams.
            RequestPacket::Status { .. } => {
                for chunk in ["a", "b", "c"] {
                    send_response(
                        &sink,
                        ResponsePacket::Text {
                            request_id,
                            chunk: chunk.into(),
                            seq: 0,
                        },
                    )
                    .await?;
                }
                send_response(
                    &sink,
                    ResponsePacket::Done {
                        request_id,
                        success: true,
                        error: None,
                    },
                )
                .await
            }
            // Stands in for an answer too large for a datagram.
            RequestPacket::RuntimeId { .. } => {
                send_response(
                    &sink,
                    ResponsePacket::Text {
                        request_id,
                        chunk: "x".repeat(200_000),
                        seq: 0,
                    },
                )
                .await
            }
            _ => anyhow::bail!("not in this test"),
        }
    }

    async fn workspaces(&self) -> Vec<std::path::PathBuf> {
        vec![self.workspace.clone()]
    }

    async fn channel_files_dir(&self, channel: &str) -> Option<std::path::PathBuf> {
        (channel == "dm_1").then(|| self.files_dir.clone())
    }

    async fn record(&self, caller: &CallerContext, event: &str, details: serde_json::Value) {
        self.records.lock().unwrap().push((
            caller.api_key_id().map(str::to_string),
            event.to_string(),
            details,
        ));
    }
}

fn door(host: Arc<FakeHost>, hosts: &[&str]) -> Door {
    let config = DoorConfig {
        addr: "127.0.0.1:11436".parse().unwrap(),
        hosts: hosts.iter().map(|h| h.to_string()).collect(),
    };
    Door {
        host,
        allowed_hosts: config.allowed_hosts("127.0.0.1:11436".parse().unwrap()),
        next_request_id: AtomicU64::new(1),
        uploads: Uploads::new(std::env::temp_dir().join("peko-door-unused")),
    }
}

fn request(method: Method, host: Option<&str>, key: Option<&str>) -> hyper::http::request::Builder {
    let mut builder = Request::builder().method(method).uri("/v1/call");
    if let Some(host) = host {
        builder = builder.header(HOST, host);
    }
    if let Some(key) = key {
        builder = builder.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    builder
}

async fn refusal(door: &Door, req: Request<()>) -> StatusCode {
    match door.admit(&req).await {
        Ok(_) => StatusCode::OK,
        Err(response) => {
            assert!(
                !response
                    .headers()
                    .keys()
                    .any(|k| k.as_str().starts_with("access-control-")),
                "no CORS header is ever sent"
            );
            response.status()
        }
    }
}

// --- D4: startup refusals -------------------------------------------------

#[test]
fn the_door_is_off_by_default() {
    let (_dir, auth) = auth_config(true);
    assert_eq!(
        DoorConfig::from_config(&NetworkConfig::default(), &auth).unwrap(),
        None
    );
}

#[test]
fn a_loopback_door_needs_no_names() {
    let (_dir, auth) = auth_config(true);
    let network = NetworkConfig {
        http: Some("127.0.0.1:11436".into()),
        http_hosts: vec![],
        ..NetworkConfig::default()
    };
    let config = DoorConfig::from_config(&network, &auth).unwrap().unwrap();
    assert_eq!(config.addr, "127.0.0.1:11436".parse().unwrap());
}

#[test]
fn a_door_beyond_this_computer_needs_http_hosts() {
    let (_dir, auth) = auth_config(true);
    let mut network = NetworkConfig {
        http: Some("0.0.0.0:11436".into()),
        http_hosts: vec![],
        ..NetworkConfig::default()
    };
    let err = DoorConfig::from_config(&network, &auth).unwrap_err();
    assert!(err.to_string().contains("http_hosts"), "{err}");

    network.http_hosts = vec!["Peko:11436".into()];
    let config = DoorConfig::from_config(&network, &auth).unwrap().unwrap();
    assert_eq!(config.hosts, vec!["peko:11436".to_string()]);
}

#[test]
fn the_door_needs_api_keys_enabled() {
    let (_dir, auth) = auth_config(false);
    let network = NetworkConfig {
        http: Some("127.0.0.1:11436".into()),
        http_hosts: vec![],
        ..NetworkConfig::default()
    };
    let err = DoorConfig::from_config(&network, &auth).unwrap_err();
    assert!(err.to_string().contains("enable_api_key"), "{err}");
}

#[test]
fn a_bad_address_is_refused() {
    let (_dir, auth) = auth_config(true);
    let network = NetworkConfig {
        http: Some("localhost".into()),
        http_hosts: vec![],
        ..NetworkConfig::default()
    };
    assert!(DoorConfig::from_config(&network, &auth).is_err());
}

// --- D4: request guards; D3: the key --------------------------------------

#[tokio::test]
async fn host_must_be_the_bound_address_or_a_listed_name() {
    let door = door(Arc::new(FakeHost::new()), &["peko:11436"]);
    let ok = |host: &str| {
        request(Method::POST, Some(host), Some(OWNER_KEY))
            .body(())
            .unwrap()
    };
    assert_eq!(refusal(&door, ok("127.0.0.1:11436")).await, StatusCode::OK);
    assert_eq!(refusal(&door, ok("localhost:11436")).await, StatusCode::OK);
    assert_eq!(refusal(&door, ok("PEKO:11436")).await, StatusCode::OK);
    assert_eq!(
        refusal(&door, ok("evil.example:11436")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        refusal(&door, ok("127.0.0.1:80")).await,
        StatusCode::FORBIDDEN
    );
    let no_host = request(Method::POST, None, Some(OWNER_KEY))
        .body(())
        .unwrap();
    assert_eq!(refusal(&door, no_host).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn origin_and_options_are_refused() {
    let door = door(Arc::new(FakeHost::new()), &[]);
    let with_origin = request(Method::POST, Some("127.0.0.1:11436"), Some(OWNER_KEY))
        .header(ORIGIN, "http://127.0.0.1:11436")
        .body(())
        .unwrap();
    assert_eq!(refusal(&door, with_origin).await, StatusCode::FORBIDDEN);
    let options = request(Method::OPTIONS, Some("127.0.0.1:11436"), Some(OWNER_KEY))
        .body(())
        .unwrap();
    assert_eq!(
        refusal(&door, options).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn a_missing_or_wrong_key_is_refused() {
    let door = door(Arc::new(FakeHost::new()), &[]);
    let host = Some("127.0.0.1:11436");
    let none = request(Method::POST, host, None).body(()).unwrap();
    assert_eq!(refusal(&door, none).await, StatusCode::UNAUTHORIZED);
    let wrong = request(Method::POST, host, Some("pkr_nope"))
        .body(())
        .unwrap();
    assert_eq!(refusal(&door, wrong).await, StatusCode::UNAUTHORIZED);
    let basic = request(Method::POST, host, None)
        .header(AUTHORIZATION, format!("Basic {OWNER_KEY}"))
        .body(())
        .unwrap();
    assert_eq!(refusal(&door, basic).await, StatusCode::UNAUTHORIZED);
    let ok = request(Method::POST, host, Some(OWNER_KEY))
        .body(())
        .unwrap();
    let caller = door.admit(&ok).await.unwrap();
    assert!(caller.is_local() && !caller.is_terminal());
}

#[tokio::test]
async fn the_rate_limit_applies_per_key() {
    let mut host = FakeHost::new();
    host.admit = false;
    let door = door(Arc::new(host), &[]);
    let req = request(Method::POST, Some("127.0.0.1:11436"), Some(OWNER_KEY))
        .body(())
        .unwrap();
    assert_eq!(refusal(&door, req).await, StatusCode::TOO_MANY_REQUESTS);
}

// --- Over a real connection ------------------------------------------------

pub(super) async fn serve_fake(
    host: Arc<FakeHost>,
) -> (String, tokio::sync::broadcast::Sender<()>) {
    let uploads = Uploads::new(host._dir.path().join("run/uploads"));
    serve_with(host, uploads).await
}

pub(super) async fn serve_with(
    host: Arc<FakeHost>,
    uploads: Arc<Uploads>,
) -> (String, tokio::sync::broadcast::Sender<()>) {
    let (tx, rx) = tokio::sync::broadcast::channel(1);
    let config = DoorConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        hosts: vec![],
    };
    let (bound, _) = start(host, config, uploads, rx).await.unwrap();
    (format!("http://{bound}"), tx)
}

pub(super) fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn call(
    base: &str,
    key: &str,
    body: serde_json::Value,
    accept: Option<&str>,
) -> reqwest::Response {
    let mut req = client()
        .post(format!("{base}/v1/call"))
        .bearer_auth(key)
        .json(&body);
    if let Some(accept) = accept {
        req = req.header("accept", accept);
    }
    req.send().await.unwrap()
}

fn sse_packets(body: &str) -> Vec<serde_json::Value> {
    body.split("\n\n")
        .filter_map(|event| event.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[tokio::test]
async fn one_packet_answers_as_json_and_the_call_is_recorded() {
    let host = Arc::new(FakeHost::new());
    let (base, _stop) = serve_fake(Arc::clone(&host)).await;
    let response = call(&base, OWNER_KEY, serde_json::json!({"type": "ping"}), None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert!(!response
        .headers()
        .keys()
        .any(|k| k.as_str().starts_with("access-control-")));
    let packet: serde_json::Value = response.json().await.unwrap();
    assert_eq!(packet["type"], "pong", "heartbeats are skipped: {packet}");
    assert!(
        packet["request_id"].as_u64().is_some(),
        "request_id filled in"
    );

    let records = host.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0.as_deref(), Some("pkr_ownerkey"));
    assert_eq!(records[0].1, "http.call");
    assert_eq!(records[0].2["request"], "ping");
    assert_eq!(records[0].2["key_id"], "pkr_ownerkey");
}

/// The door is not a datagram: an answer over the socket's 60,000 bytes
/// arrives whole, where the socket's own serializer refuses it.
#[tokio::test]
async fn an_answer_larger_than_a_datagram_arrives_whole() {
    let host = Arc::new(FakeHost::new());
    let (base, _stop) = serve_fake(Arc::clone(&host)).await;
    let response = call(
        &base,
        OWNER_KEY,
        serde_json::json!({"type": "runtime_id"}),
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let packet: serde_json::Value = response.json().await.unwrap();
    assert_eq!(packet["chunk"].as_str().map(str::len), Some(200_000));

    let same = ResponsePacket::Text {
        request_id: 1,
        chunk: "x".repeat(200_000),
        seq: 0,
    };
    assert!(same.to_bytes().is_err(), "the socket still refuses it");
    assert!(same.to_bytes_within(HTTP_MAX_PACKET).is_ok());
}

#[tokio::test]
async fn a_request_that_streams_answers_as_an_event_stream() {
    let (base, _stop) = serve_fake(Arc::new(FakeHost::new())).await;
    let response = call(
        &base,
        OWNER_KEY,
        serde_json::json!({"type": "status", "request_id": 7}),
        None,
    )
    .await;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let packets = sse_packets(&response.text().await.unwrap());
    let kinds: Vec<_> = packets
        .iter()
        .map(|p| p["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["text", "text", "text", "done"]);
    assert!(packets.iter().all(|p| p["request_id"] == 7));
}

#[tokio::test]
async fn accept_event_stream_streams_even_one_packet() {
    let (base, _stop) = serve_fake(Arc::new(FakeHost::new())).await;
    let response = call(
        &base,
        OWNER_KEY,
        serde_json::json!({"type": "ping"}),
        Some("text/event-stream"),
    )
    .await;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let packets = sse_packets(&response.text().await.unwrap());
    let kinds: Vec<_> = packets
        .iter()
        .map(|p| p["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["heartbeat", "pong"]);
}

#[tokio::test]
async fn the_body_must_be_one_json_request_within_the_packet_budget() {
    let (base, _stop) = serve_fake(Arc::new(FakeHost::new())).await;
    let post = || {
        client()
            .post(format!("{base}/v1/call"))
            .bearer_auth(OWNER_KEY)
    };

    let text = post()
        .header("content-type", "text/plain")
        .body(r#"{"type":"ping"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(text.status(), 415);
    let form = post()
        .header("content-type", "application/x-www-form-urlencoded")
        .body("type=ping")
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), 415);

    let charset = post()
        .header("content-type", "application/json; charset=utf-8")
        .body(r#"{"type":"ping"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(charset.status(), 200);

    let big = serde_json::json!({"type": "ping", "pad": "x".repeat(HTTP_MAX_PACKET)});
    assert_eq!(post().json(&big).send().await.unwrap().status(), 413);

    let not_json = post()
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(not_json.status(), 400);
    let unknown = post()
        .json(&serde_json::json!({"type": "no_such_request"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 400);
    let array = post().json(&serde_json::json!([1])).send().await.unwrap();
    assert_eq!(array.status(), 400);

    let get = client()
        .get(format!("{base}/v1/call"))
        .bearer_auth(OWNER_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 405);
    let elsewhere = client()
        .get(format!("{base}/v2/whatever"))
        .bearer_auth(OWNER_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(elsewhere.status(), 404);
}

#[tokio::test]
async fn a_key_in_the_url_is_not_a_key() {
    let (base, _stop) = serve_fake(Arc::new(FakeHost::new())).await;
    let response = client()
        .post(format!(
            "{base}/v1/call?key={OWNER_KEY}&access_token={OWNER_KEY}"
        ))
        .json(&serde_json::json!({"type": "ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

/// D3: an `owner` key passes a handler's local-only check; read, write
/// and admin keys do not.
#[tokio::test]
async fn only_an_owner_key_passes_a_local_only_handler() {
    let (base, _stop) = serve_fake(Arc::new(FakeHost::new())).await;
    let body = serde_json::json!({"type": "channel_read_state"});
    let owner: serde_json::Value = call(&base, OWNER_KEY, body.clone(), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(owner["type"], "channel_read_state_result", "{owner}");

    for key in [WRITE_KEY, ADMIN_KEY] {
        let packet: serde_json::Value = call(&base, key, body.clone(), None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(packet["type"], "error", "{packet}");
        assert!(packet["message"]
            .as_str()
            .unwrap()
            .contains("local callers only"));
    }
}

/// D3: an `owner` key cannot make, list or revoke keys.
#[tokio::test]
async fn key_management_is_refused_to_an_owner_key() {
    let (base, _stop) = serve_fake(Arc::new(FakeHost::new())).await;
    for body in [
        serde_json::json!({"type": "auth_api_key_create", "name": "more", "scopes": ["owner"]}),
        serde_json::json!({"type": "auth_api_key_list"}),
        serde_json::json!({"type": "auth_api_key_revoke", "key_id": "pkr_ownerkey"}),
    ] {
        let packet: serde_json::Value = call(&base, OWNER_KEY, body.clone(), None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(packet["type"], "error", "{body} -> {packet}");
        assert!(
            packet["message"].as_str().unwrap().contains("local access"),
            "{packet}"
        );
    }
}

// --- D5: files ----------------------------------------------------------------

fn upload_req(base: &str, key: &str, name: &str, body: &'static [u8]) -> reqwest::RequestBuilder {
    client()
        .post(format!("{base}/v1/files"))
        .query(&[("name", name)])
        .bearer_auth(key)
        .header("content-type", "application/octet-stream")
        .body(body)
}

#[tokio::test]
async fn an_upload_answers_its_name_for_a_send() {
    let host = Arc::new(FakeHost::new());
    let uploads_dir = host._dir.path().join("run/uploads");
    let (base, _stop) = serve_fake(Arc::clone(&host)).await;
    let response = upload_req(&base, OWNER_KEY, "photo one.png", b"PNGDATA")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let answer: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        answer["name"], "photo_one.png",
        "made safe as any attachment name"
    );
    assert_eq!(answer["size"], 7);
    let file = answer["file"].as_str().unwrap();
    let (paths, used) = crate::ipc::uploads::resolve(&uploads_dir, &[file.to_string()]).unwrap();
    assert_eq!(std::fs::read(&paths[0]).unwrap(), b"PNGDATA");
    assert_eq!(used.len(), 1);
    assert!(host
        .records
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.1 == "http.upload" && r.2["file"] == file));
}

#[tokio::test]
async fn uploads_are_refused_without_an_owner_key_a_name_or_the_right_type() {
    let (base, _stop) = serve_fake(Arc::new(FakeHost::new())).await;
    let write = upload_req(&base, WRITE_KEY, "a.txt", b"x")
        .send()
        .await
        .unwrap();
    assert_eq!(write.status(), 403);
    let admin = upload_req(&base, ADMIN_KEY, "a.txt", b"x")
        .send()
        .await
        .unwrap();
    assert_eq!(admin.status(), 403);
    let json = client()
        .post(format!("{base}/v1/files?name=a.txt"))
        .bearer_auth(OWNER_KEY)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(json.status(), 415);
    let unnamed = client()
        .post(format!("{base}/v1/files"))
        .bearer_auth(OWNER_KEY)
        .header("content-type", "application/octet-stream")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(unnamed.status(), 400);
}

#[tokio::test]
async fn uploads_over_the_limits_are_refused() {
    let host = Arc::new(FakeHost::new());
    let uploads = Arc::new(Uploads::with_limits(
        host._dir.path().join("run/uploads"),
        8,
        12,
        crate::ipc::uploads::TTL,
    ));
    let (base, _stop) = serve_with(host, uploads).await;
    let big = upload_req(&base, OWNER_KEY, "big", b"0123456789")
        .send()
        .await
        .unwrap();
    assert_eq!(big.status(), 413);
    let ok = upload_req(&base, OWNER_KEY, "a", b"01234567")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let full = upload_req(&base, OWNER_KEY, "b", b"012345")
        .send()
        .await
        .unwrap();
    assert_eq!(full.status(), 507);
}

async fn get_file(base: &str, key: &str, query: &[(&str, &str)]) -> reqwest::Response {
    client()
        .get(format!("{base}/v1/files"))
        .query(query)
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_posts_file_comes_back_by_path_only_from_attachments() {
    let host = Arc::new(FakeHost::new());
    let ws = host.workspace.clone();
    let inside = ws.join("attachments/2026-11-24/ab12cd34-report.pdf");
    std::fs::write(&inside, b"%PDF-1.4 report").unwrap();
    let secret = ws.join("kb/secret.md");
    std::fs::write(&secret, b"secret").unwrap();
    let outside = host._dir.path().join("runtime-vault.json");
    std::fs::write(&outside, b"vault").unwrap();
    let (base, _stop) = serve_fake(Arc::clone(&host)).await;

    let ok = get_file(&base, OWNER_KEY, &[("path", inside.to_str().unwrap())]).await;
    assert_eq!(ok.status(), 200);
    assert!(ok.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains("report.pdf"));
    assert_eq!(ok.headers()["x-content-type-options"], "nosniff");
    assert_eq!(&ok.bytes().await.unwrap()[..], b"%PDF-1.4 report");

    for path in [
        secret.clone(),
        outside.clone(),
        ws.join("attachments/2026-11-24/../../kb/secret.md"),
        ws.join("attachments/missing.png"),
        ws.join("attachments"),
    ] {
        let refused = get_file(&base, OWNER_KEY, &[("path", path.to_str().unwrap())]).await;
        assert_eq!(refused.status(), 404, "{}", path.display());
    }

    // A link inside attachments/ to a file outside is followed, then refused.
    #[cfg(unix)]
    {
        let link = ws.join("attachments/2026-11-24/link.json");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let refused = get_file(&base, OWNER_KEY, &[("path", link.to_str().unwrap())]).await;
        assert_eq!(refused.status(), 404);
    }

    let write = get_file(&base, WRITE_KEY, &[("path", inside.to_str().unwrap())]).await;
    assert_eq!(write.status(), 403);
}

#[tokio::test]
async fn a_channels_file_comes_back_by_digest() {
    let host = Arc::new(FakeHost::new());
    let digest = "a".repeat(64);
    std::fs::create_dir_all(host.files_dir.join(&digest)).unwrap();
    std::fs::write(host.files_dir.join(&digest).join("note.txt"), b"hello").unwrap();
    let (base, _stop) = serve_fake(Arc::clone(&host)).await;

    let ok = get_file(
        &base,
        OWNER_KEY,
        &[("channel", "dm_1"), ("sha256", &digest)],
    )
    .await;
    assert_eq!(ok.status(), 200);
    assert_eq!(&ok.bytes().await.unwrap()[..], b"hello");

    let other = "b".repeat(64);
    let missing = get_file(&base, OWNER_KEY, &[("channel", "dm_1"), ("sha256", &other)]).await;
    assert_eq!(missing.status(), 404);
    let no_channel = get_file(
        &base,
        OWNER_KEY,
        &[("channel", "dm_2"), ("sha256", &digest)],
    )
    .await;
    assert_eq!(no_channel.status(), 404);
    let not_digest = get_file(
        &base,
        OWNER_KEY,
        &[("channel", "dm_1"), ("sha256", "../../x")],
    )
    .await;
    assert_eq!(not_digest.status(), 400);
    let neither = get_file(&base, OWNER_KEY, &[]).await;
    assert_eq!(neither.status(), 400);
}

// --- D6: the schema -------------------------------------------------------------

#[tokio::test]
async fn the_schema_is_served_to_any_valid_key() {
    let (base, _stop) = serve_fake(Arc::new(FakeHost::new())).await;
    for key in [OWNER_KEY, WRITE_KEY] {
        let response = client()
            .get(format!("{base}/v1/schema"))
            .bearer_auth(key)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(
            response.text().await.unwrap(),
            crate::ipc::schema::SCHEMA_JSON
        );
    }
    let anonymous = client()
        .get(format!("{base}/v1/schema"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
}
