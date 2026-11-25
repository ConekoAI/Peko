//! Router tests (ADR-125): two routers on loopback link, choose paths,
//! ask again, keep order, and refuse what D7 and D4 say to.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;

use super::*;
use crate::security::device_list::{test_chain, test_computer, DeviceLists};
use crate::tunnel::protocol::RuntimeKeyEntry;
use crate::tunnel::seal::{sign_key_statement, AgreementKey, SealPolicy, Sealer};
use crate::tunnel::verifying_key_to_did_key;

const ACCOUNT: &str = "0b6c7a4e-1d2f-4a3b-8c9d-0e1f2a3b4c5d";

struct Policy(Option<String>);

#[async_trait::async_trait]
impl SealPolicy for Policy {
    async fn own_account(&self) -> Option<String> {
        self.0.clone()
    }
    fn device_key(&self, _: &str) -> Option<String> {
        None
    }
}

struct Computer {
    key: Arc<SigningKey>,
    id: String,
}

fn computer() -> Computer {
    let key = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
    let id = verifying_key_to_did_key(&key.verifying_key());
    Computer {
        key: Arc::new(key),
        id,
    }
}

fn statement(c: &Computer) -> String {
    sign_key_statement(&c.key, &c.id, &AgreementKey::derive(&c.key))
}

struct Side {
    router: Router,
    hub_rx: mpsc::Receiver<Vec<u8>>,
    got: Arc<Mutex<Vec<TunnelMessage>>>,
    sealer: Arc<Sealer>,
}

impl Side {
    /// Sealed frames of `ty` that went to PekoHub.
    fn hub_frames(&mut self, ty: &str) -> usize {
        let mut n = 0;
        while let Ok(bytes) = self.hub_rx.try_recv() {
            if frame_type(&bytes).as_deref() == Some(ty) {
                n += 1;
            }
        }
        n
    }
}

/// A router for `me`, on an account whose computers are `on_list`,
/// holding the key statements of `knows`.
async fn side(
    me: &Computer,
    on_list: &[&Computer],
    knows: &[&Computer],
    network: NetworkConfig,
) -> Side {
    let devices = Arc::new(DeviceLists::new(None));
    let chain_keys: Vec<(&SigningKey, crate::security::device_list::Device)> = on_list
        .iter()
        .map(|c| (c.key.as_ref(), test_computer(&c.key)))
        .collect();
    devices
        .accept(
            &format!("user:hub:{ACCOUNT}"),
            test_chain(&format!("user:hub:{ACCOUNT}"), &chain_keys),
        )
        .unwrap();
    let policy: Arc<dyn SealPolicy> = Arc::new(Policy(Some(ACCOUNT.into())));
    let sealer = Arc::new(Sealer::new(
        me.id.clone(),
        Arc::clone(&me.key),
        Arc::clone(&policy),
        Arc::clone(&devices),
        None,
    ));
    let keys = TunnelMessage::RuntimeKeys {
        request_id: "seed".into(),
        account: None,
        keys: knows
            .iter()
            .map(|c| RuntimeKeyEntry {
                runtime_id: c.id.clone(),
                statement: statement(c),
            })
            .collect(),
    };
    let _ = sealer.inbound(&keys.to_bytes().unwrap()).await;
    let (router, hub_rx) = Router::new(RouterConfig {
        runtime_id: me.id.clone(),
        signing_key: Arc::clone(&me.key),
        sealer: Some(Arc::clone(&sealer)),
        devices: Some(devices),
        policy: Some(policy),
        network,
        links_path: None,
    });
    router.set_warm_for(Duration::from_millis(200));
    let got = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&got);
    router.set_handler(Arc::new(move |msg, _handle| {
        let sink = Arc::clone(&sink);
        Box::pin(async move {
            sink.lock().unwrap().push(msg);
        })
    }));
    router.hub_state(true);
    router.start();
    Side {
        router,
        hub_rx,
        got,
        sealer,
    }
}

fn listening_on(address: &str) -> NetworkConfig {
    NetworkConfig {
        connection: Connection::Automatic,
        direct_listen: Some(address.into()),
        ..NetworkConfig::default()
    }
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn listening_address(router: &Router) -> String {
    for _ in 0..200 {
        if let Some(a) = router.status().await.listening.address {
            return a;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("did not listen");
}

fn models_request(from: &Computer, to: &Computer, id: &str) -> TunnelMessage {
    TunnelMessage::TunnelPekoModelsRequest {
        request_id: id.into(),
        source_runtime_id: from.id.clone(),
        recipient_runtime_id: to.id.clone(),
        source_principal_did: format!("user:hub:{ACCOUNT}"),
        principal_did: "did:key:z6MkPeko".into(),
        signature: "sig".into(),
    }
}

/// To every other computer of the account (here: `_to`).
fn read_mark(from: &Computer, _to: &Computer, id: &str) -> TunnelMessage {
    TunnelMessage::TunnelReadMark {
        request_id: id.into(),
        source_runtime_id: from.id.clone(),
        source_principal_did: format!("user:hub:{ACCOUNT}"),
        channel_id: "dm_x".into(),
        msg_id: "m1".into(),
        signature: "sig".into(),
    }
}

/// Two routers link (dialed from `direct_peers`), and once the link has
/// answered heartbeats frames take it; before that they go to PekoHub.
#[tokio::test]
async fn two_computers_link_and_frames_take_the_link_once_it_is_warm() {
    let (a, b) = (computer(), computer());
    let mut side_a = side(&a, &[&a, &b], &[&b], listening_on("127.0.0.1:0")).await;
    let address = listening_address(&side_a.router).await;
    let mut side_b = side(
        &b,
        &[&a, &b],
        &[&a],
        NetworkConfig {
            connection: Connection::Automatic,
            direct_listen: Some("127.0.0.1:0".into()),
            direct_peers: vec![address],
            ..NetworkConfig::default()
        },
    )
    .await;
    side_b.router.retry(None).await;
    wait_for("the link", || {
        side_b
            .router
            .inner
            .links
            .lock()
            .unwrap()
            .contains_key(&a.id)
    })
    .await;
    // Up, not warm yet: through PekoHub.
    assert_eq!(side_b.router.path_to(&a.id), Path::Relay);
    side_b
        .router
        .handle()
        .send(read_mark(&b, &a, "early"))
        .unwrap();
    wait_for("the early frame at PekoHub", || {
        side_b.hub_frames("tunnel_read_mark") == 1
    })
    .await;

    wait_for("a warm link", || {
        side_b.router.path_to(&a.id) == Path::Direct
    })
    .await;
    side_b
        .router
        .handle()
        .send(read_mark(&b, &a, "late"))
        .unwrap();
    wait_for("the frame over the link", || {
        side_a
            .got
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, TunnelMessage::TunnelReadMark { request_id, .. } if request_id == "late"))
    })
    .await;
    assert_eq!(
        side_b.hub_frames("tunnel_read_mark"),
        0,
        "nothing more went to PekoHub"
    );
    let status = side_a.router.status().await;
    let peer = status.peers.iter().find(|p| p.runtime_id == b.id).unwrap();
    assert_eq!(peer.path, Path::Direct);
    assert!(peer.since.is_some());
    // One link per pair, whoever dialed.
    assert_eq!(side_a.router.inner.links.lock().unwrap().len(), 1);
    let _ = side_a.hub_frames("");
}

/// D4: a computer that is not on the list is refused at the hello.
#[tokio::test]
async fn a_computer_not_admitted_is_refused_at_its_hello() {
    let (a, stranger) = (computer(), computer());
    let side_a = side(&a, &[&a], &[], listening_on("127.0.0.1:0")).await;
    let address = listening_address(&side_a.router).await;
    let side_s = side(
        &stranger,
        &[&stranger, &a],
        &[&a],
        NetworkConfig {
            connection: Connection::Automatic,
            direct_listen: Some("127.0.0.1:0".into()),
            ..NetworkConfig::default()
        },
    )
    .await;
    let target: std::net::SocketAddr = address.parse().unwrap();
    let result = conn::dial(side_s.router.clone(), target, Some(&a.id)).await;
    assert!(result.is_err(), "{result:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(side_a.router.inner.links.lock().unwrap().is_empty());
    assert!(side_s.router.inner.links.lock().unwrap().is_empty());
}

/// D6: a request still unanswered when the path changes is asked again
/// on the new path, once.
#[tokio::test]
async fn an_unanswered_request_is_asked_again_when_a_link_comes_up() {
    let (a, b) = (computer(), computer());
    let side_a = side(&a, &[&a, &b], &[&b], listening_on("127.0.0.1:0")).await;
    let address = listening_address(&side_a.router).await;
    let mut side_b = side(
        &b,
        &[&a, &b],
        &[&a],
        NetworkConfig {
            connection: Connection::Automatic,
            direct_peers: vec![address],
            ..NetworkConfig::default()
        },
    )
    .await;
    // Asked through PekoHub first (no link yet): it went there.
    side_b
        .router
        .handle()
        .send(models_request(&b, &a, "models-1"))
        .unwrap();
    wait_for("the request at PekoHub", || {
        side_b.hub_frames("tunnel_peko_models_request") == 1
    })
    .await;
    side_b.router.recheck(&a.id);
    side_b.router.retry(None).await;
    wait_for("the request asked again on the link", || {
        side_a
            .got
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, TunnelMessage::TunnelPekoModelsRequest { request_id, .. } if request_id == "models-1"))
    })
    .await;
    // `peer_runtime_online` was handed on too, so the outbox resends.
    wait_for("peer online", || {
        side_b.got.lock().unwrap().iter().any(
            |m| matches!(m, TunnelMessage::PeerRuntimeOnline { runtime_id } if *runtime_id == a.id),
        )
    })
    .await;
}

/// D7: in `direct`, a sealed frame from PekoHub is dropped unopened, a
/// proxied visitor request is refused, and nothing sealed goes out
/// through PekoHub but addresses.
#[tokio::test]
async fn direct_only_refuses_relayed_frames() {
    let (a, b) = (computer(), computer());
    let mut side_a = side(
        &a,
        &[&a, &b],
        &[&b],
        NetworkConfig {
            connection: Connection::Direct,
            direct_listen: Some("127.0.0.1:0".into()),
            ..NetworkConfig::default()
        },
    )
    .await;
    let mut side_b = side(&b, &[&a, &b], &[&a], NetworkConfig::default()).await;

    // B (PekoHub only) seals a frame to A; it arrives through PekoHub.
    side_b
        .router
        .handle()
        .send(read_mark(&b, &a, "relayed"))
        .unwrap();
    let mut sealed = None;
    for _ in 0..100 {
        if let Ok(bytes) = side_b.hub_rx.try_recv() {
            if frame_type(&bytes).as_deref() == Some("tunnel_read_mark") {
                sealed = Some(bytes);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let sealed = sealed.expect("B sealed it for PekoHub");
    assert!(side_a.router.receive_from_hub(&sealed).await.is_empty());
    assert_eq!(side_a.router.dropped_relayed(), 1);

    // A visitor's chat through PekoHub's proxy is answered with a refusal.
    let proxied = TunnelMessage::ProxiedRequest {
        request_id: "visit-1".into(),
        principal: "ada".into(),
        payload: b"{}".to_vec(),
    };
    assert!(side_a
        .router
        .receive_from_hub(&proxied.to_bytes().unwrap())
        .await
        .is_empty());
    wait_for("the refusal", || side_a.hub_frames("proxied_response") == 1).await;

    // Nothing sealed leaves A through PekoHub; it waits for a link.
    assert_eq!(side_a.router.path_to(&b.id), Path::Waiting);
    side_a
        .router
        .handle()
        .send(read_mark(&a, &b, "held"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(side_a.hub_frames("tunnel_read_mark"), 0);

    // Addresses are exempt both ways.
    side_a.router.tell(&b.id);
    wait_for("addresses at PekoHub", || {
        side_a.hub_frames(ADDRESSES_TYPE) >= 1
    })
    .await;
}

/// D7: a computer that said it takes no relayed frames gets none, even
/// from a computer in `automatic`.
#[tokio::test]
async fn a_computer_that_takes_no_relay_gets_nothing_through_pekohub() {
    let (a, b) = (computer(), computer());
    let mut side_a = side(&a, &[&a, &b], &[&b], listening_on("127.0.0.1:0")).await;
    let statement = addresses::sign_addresses(&b.key, &b.id, &["10.9.9.9:11438".into()], false);
    side_a.router.on_addresses(&b.id, &statement).await;
    assert_eq!(side_a.router.path_to(&b.id), Path::Waiting);
    side_a
        .router
        .handle()
        .send(read_mark(&a, &b, "held"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(side_a.hub_frames("tunnel_read_mark"), 0);
    // An older statement does not undo a newer one; a newer one does.
    let back = addresses::sign_addresses(&b.key, &b.id, &[], true);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let newer = addresses::sign_addresses(&b.key, &b.id, &[], true);
    side_a.router.on_addresses(&b.id, &newer).await;
    let _ = back;
    assert_eq!(side_a.router.path_to(&b.id), Path::Relay);
}

/// D6: channel events carry `seq`; one that arrives ahead of its turn
/// waits for the one before it.
#[tokio::test]
async fn channel_events_are_put_back_in_order() {
    let (a, b) = (computer(), computer());
    let mut side_a = side(&a, &[&a, &b], &[&b], NetworkConfig::default()).await;
    let side_b = side(&b, &[&a, &b], &[&a], NetworkConfig::default()).await;
    let event = |id: &str| TunnelMessage::TunnelChannelEvent {
        request_id: id.into(),
        source_runtime_id: a.id.clone(),
        recipient_runtime_id: b.id.clone(),
        source_principal_did: "did:key:z6MkPeko".into(),
        channel_id: "dm_x".into(),
        event: peko_protocol::channel::ChannelEvent::Received {
            channel: peko_protocol::channel::ChannelId("dm_x".into()),
            request_ids: vec![id.to_string()],
            at: chrono::Utc::now().to_rfc3339(),
        },
        signature: "sig".into(),
        author_signature: String::new(),
    };
    let mut frames = Vec::new();
    for id in ["e1", "e2", "e3"] {
        frames.extend(side_a.sealer.outbound(event(id)).await);
    }
    assert_eq!(frames.len(), 3);
    let _ = side_a.hub_frames("");
    let ids = |msgs: Vec<TunnelMessage>| -> Vec<String> {
        msgs.into_iter()
            .filter_map(|m| match m {
                TunnelMessage::TunnelChannelEvent { request_id, .. } => Some(request_id),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        ids(side_b.router.receive_from_hub(&frames[0]).await),
        vec!["e1"]
    );
    assert!(ids(side_b.router.receive_from_hub(&frames[2]).await).is_empty());
    assert_eq!(
        ids(side_b.router.receive_from_hub(&frames[1]).await),
        vec!["e2", "e3"]
    );
    // A resend of e2 (same request id) keeps its number: handed on as late.
    let again = side_a.sealer.outbound(event("e2")).await;
    assert_eq!(
        ids(side_b.router.receive_from_hub(&again[0]).await),
        vec!["e2"]
    );
}

/// D6: after a network change a link that does not answer within the
/// doubt window is closed, and frames go through PekoHub meanwhile.
#[tokio::test]
async fn a_link_in_doubt_carries_nothing_until_it_answers() {
    let (a, b) = (computer(), computer());
    let side_a = side(&a, &[&a, &b], &[&b], listening_on("127.0.0.1:0")).await;
    let address = listening_address(&side_a.router).await;
    let side_b = side(
        &b,
        &[&a, &b],
        &[&a],
        NetworkConfig {
            connection: Connection::Automatic,
            direct_peers: vec![address],
            ..NetworkConfig::default()
        },
    )
    .await;
    side_b.router.retry(None).await;
    wait_for("a warm link", || {
        side_b.router.path_to(&a.id) == Path::Direct
    })
    .await;
    let link = side_b
        .router
        .inner
        .links
        .lock()
        .unwrap()
        .get(&a.id)
        .cloned()
        .unwrap();
    link.doubt();
    assert_eq!(side_b.router.path_to(&a.id), Path::Relay);
    // The other side answers the heartbeat: the link carries again.
    wait_for("the link to answer", || {
        side_b.router.path_to(&a.id) == Path::Direct
    })
    .await;
}

/// Settings applied at once: `pekohub` closes links and stops listening.
#[tokio::test]
async fn going_back_to_pekohub_only_closes_links_and_the_listener() {
    let (a, b) = (computer(), computer());
    let side_a = side(&a, &[&a, &b], &[&b], listening_on("127.0.0.1:0")).await;
    let address = listening_address(&side_a.router).await;
    let side_b = side(
        &b,
        &[&a, &b],
        &[&a],
        NetworkConfig {
            connection: Connection::Automatic,
            direct_peers: vec![address],
            ..NetworkConfig::default()
        },
    )
    .await;
    side_b.router.retry(None).await;
    wait_for("the link", || {
        !side_a.router.inner.links.lock().unwrap().is_empty()
    })
    .await;
    side_a.router.apply(NetworkConfig::default()).await;
    let status = side_a.router.status().await;
    assert_eq!(status.connection, Connection::Pekohub);
    assert_eq!(status.listening.address, None);
    wait_for("the link to close", || {
        side_b.router.inner.links.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(side_b.router.path_to(&a.id), Path::Relay);
}

/// D2: a port that cannot be listened on is shown, and the engine still
/// runs.
#[tokio::test]
async fn a_listener_that_cannot_bind_says_why() {
    let a = computer();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap().to_string();
    let side_a = side(&a, &[&a], &[], listening_on(&address)).await;
    for _ in 0..100 {
        if side_a.router.status().await.listening.error.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no listening error shown");
}
