//! `PrincipalAccessGet` / `PrincipalAccessSet`: a peko's visitor rules
//! (ADR-113) for a sharing screen, so its owner changes what visitors
//! reach without editing `ACCESS.toml` by hand.

use std::collections::BTreeMap;

use peko_auth::caller::CallerContext;
use peko_auth::ownership::{check_permission, principal_resource, Permission};
use peko_auth::ActorRef;
use peko_channel::ChannelPort;
use peko_protocol::channel::ConversationKind;

use super::{host_keys, PrincipalHost};
use crate::ipc::packet::{AccessKnown, AccessPerson, ResponsePacket};
use crate::principal::Principal;
use crate::security::access::{self, Shared};

/// The owner, from this computer: the only caller who changes or reads
/// what visitors reach. A visitor seeing the rules would learn what
/// others were given.
async fn owner_only(
    caller: &CallerContext,
    principal: &Principal,
    host: &dyn PrincipalHost,
) -> Result<(), String> {
    let refusal = "only this peko's owner, from this computer, sets what visitors reach";
    if !caller.is_operator() {
        return Err(refusal.into());
    }
    let config = principal.config.read().await;
    let resource = principal_resource(&*config);
    check_permission(
        &resource,
        Permission::ManageSettings,
        &caller.permission_subject(resource.owner()),
        &*host_keys(host),
    )
    .map_err(|_| refusal.to_string())
}

pub(super) async fn get(
    request_id: u64,
    name: &str,
    caller: &CallerContext,
    host: &dyn PrincipalHost,
) -> ResponsePacket {
    let Some(principal) = host.principal_manager().get_by_name(name).await else {
        return error(request_id, format!("Peko '{name}' not loaded"));
    };
    if let Err(refusal) = owner_only(caller, &principal, host).await {
        return error(request_id, refusal);
    }
    view(request_id, name, &principal, host).await
}

pub(super) async fn set(
    request_id: u64,
    name: &str,
    visitors: &Shared,
    people: &[AccessPerson],
    caller: &CallerContext,
    host: &dyn PrincipalHost,
) -> ResponsePacket {
    let Some(principal) = host.principal_manager().get_by_name(name).await else {
        return error(request_id, format!("Peko '{name}' not loaded"));
    };
    if let Err(refusal) = owner_only(caller, &principal, host).await {
        return error(request_id, refusal);
    }
    let sections: Vec<(String, Shared)> = people
        .iter()
        .map(|p| (p.address.trim().to_string(), p.rule.clone()))
        .collect();
    if let Err(e) = access::set_shared(&principal.workspace_path, visitors, &sections) {
        return error(request_id, e);
    }
    if let Some(audit) = host.observability() {
        let _ = audit
            .audit_security_with_caller(
                Some(&caller.subject()),
                "principal.access_changed",
                Some(&principal.id.0),
                serde_json::json!({
                    "visitors": visitors,
                    "people": sections.iter().map(|(a, _)| a).collect::<Vec<_>>(),
                }),
            )
            .await;
    }
    view(request_id, name, &principal, host).await
}

/// The rules as the file has them now, with names for the people.
async fn view(
    request_id: u64,
    name: &str,
    principal: &Principal,
    host: &dyn PrincipalHost,
) -> ResponsePacket {
    let workspace = &principal.workspace_path;
    let servers = access::mcp_servers(workspace);
    let port = host.principal_manager().channel_port();
    let known = match &port {
        Some(port) => known_people(principal, port).await,
        None => BTreeMap::new(),
    };
    let (visitors, people, problem) = match access::shared_as_written(workspace) {
        Ok((visitors, people)) => (visitors, people, None),
        Err(problem) => (Shared::default(), Vec::new(), Some(problem)),
    };
    let mut named = Vec::with_capacity(people.len());
    for (address, rule) in people {
        let display_name = match known.get(&address) {
            Some(name) => name.clone(),
            None => match (&port, address.parse::<ActorRef>()) {
                (Some(port), Ok(actor)) => port.actor_info(&actor).await.name,
                _ => None,
            },
        };
        named.push(AccessPerson {
            address,
            display_name,
            rule,
        });
    }
    ResponsePacket::PrincipalAccess {
        request_id,
        name: name.to_string(),
        visitors,
        people: named,
        known: known
            .into_iter()
            .map(|(address, display_name)| AccessKnown {
                address,
                display_name,
            })
            .collect(),
        servers,
        problem,
    }
}

/// Everyone the peko has a one-to-one conversation with, other than its
/// owner, by canonical address.
async fn known_people(
    principal: &Principal,
    port: &std::sync::Arc<dyn ChannelPort>,
) -> BTreeMap<String, Option<String>> {
    let own = port.key_of(&ActorRef::from(&principal.id));
    let owner = principal.config.read().await.owner.clone();
    let owner_key = port.key_of(&owner);
    let local_user = port.key_of(&ActorRef::User("local".into()));
    let mut out = BTreeMap::new();
    let Ok(channels) = port.list_for_principal(&principal.id).await else {
        return out;
    };
    for channel in channels {
        if !matches!(
            port.conversation_kind(&channel).await,
            Ok(ConversationKind::Dm)
        ) {
            continue;
        }
        let Ok(members) = port.list_members(&channel).await else {
            continue;
        };
        for member in members {
            let key = port.key_of(&member);
            if key == own || key == owner_key || key == local_user {
                continue;
            }
            let info = port.actor_info(&member).await;
            out.entry(member.canonical_address()).or_insert(info.name);
        }
    }
    out
}

fn error(request_id: u64, message: String) -> ResponsePacket {
    ResponsePacket::Error {
        request_id,
        message,
    }
}
