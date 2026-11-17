# ADR-110: What crosses PekoHub is sealed to the computer it is for; the Hub routes envelopes it cannot read

**Status:** Accepted. Implemented on `feat/adr-110-sealed-envelopes` (runtime, PekoHub), `feat/adr-110-key-change-card` (desktop) and `feat/adr-110-sealed-device-traffic` (peko-mobile); not merged. Live run §7.
**Date:** 2026-11-17
**Builds on:** [ADR-058](ADR-058-origin-signed-messaging.md) (D2/D3: envelopes are compact JWS by the runtime and the author; D8 deferred content confidentiality to this ADR), [ADR-091](ADR-091-remote-dm-history-from-the-host.md) (D7: the Hub only relays, and sees what it relays), [ADR-094](ADR-094-attachments-cross-runtimes.md) (files pulled in 256 KiB pieces through the Hub), [ADR-108](ADR-108-a-phone-is-a-paired-window-onto-your-computer.md) (D2: a phone's keys are enrolled from the computer's screen; D6: push is already sealed with HPKE; §6.5), [ADR-109](ADR-109-a-computer-speaks-as-you-once-you-approve-it.md) (the owner approves their own computers), [ADR-072](ADR-072-human-started-runs-and-durable-delivery.md) (an unacknowledged post stays in the sender's outbox), [ADR-102](ADR-102-plain-versions-and-separate-releases.md) (a wire break goes out in a minor bump).
**Counterpart:** PekoHub (relays the new `sealed` field; keeps and serves key statements, vouches and host statements). Peko mobile (seals device requests, opens responses and events). Peko desktop: no protocol change; it shows the new key-change approval.

---

## 1. Context

The 2026-11-17 audit of PekoHub and the tunnel found that what Peko says about the Hub ("it keeps no messages") cannot be checked by anyone who uses it:

| # | Fact | Where |
|---|---|---|
| F1 | Every cross-runtime envelope carries its content in readable JSON inside TLS: `tunnel_channel_event.event` (post text, attachment names and digests, board ops), `tunnel_channel_invite` (channel name, members), `tunnel_channel_history.events`, `tunnel_attachment_chunk.data`, `tunnel_account_dms`, the approval requests and decisions (ADR-099, whose descriptions carry tool parameters), and the phone's requests, answers, and live events (ADR-108 D3/D4). | `tunnel/protocol.rs` |
| F2 | The `signature` field does not hide anything either: it is a compact JWS with an *embedded* payload (ADR-058 D3), so it carries every envelope field again, base64url-encoded. | `tunnel/tunnel_channel_signature.rs` |
| F3 | The Hub reads only routing fields of a relayed envelope: `type`, `sourceRuntimeId`, `recipientRuntimeId` / `recipientAccount` / `deviceId`, `sourcePrincipalDid` (the account checks of ADR-090 D3 and ADR-109), and `channelId` (logs). It verifies JWS only on registration, proof of possession, and group join tokens, never on a relayed message. | PekoHub `services/tunnel-manager.ts`, `services/tunnel-crypto.ts` |
| F4 | The Hub stores no message content today: no message tables, and logs carry ids. But the PekoHub repository is private, and even published code would not show what runs in production. A bad deploy, a stolen Fly token, or the operator could record every conversation. | PekoHub `db/schema.ts` |
| F5 | Only push is sealed: ADR-108 D6 seals each notification to the phone's X25519 key with HPKE (`hpke` 0.12 is already a dependency). | `tunnel/devices.rs` |
| F6 | Fan-out already sends one envelope per recipient runtime (`fanout_event`, `fanout_invite`), so each copy can be sealed to one computer without a shared group key. | `tunnel/tunnel_channel_port.rs` `fanout_event` |
| F7 | Some envelopes are addressed to an account, not a computer (`recipientAccount`: group invitations, a person's DM invites, DM-list sync between one's own computers), and the Hub delivers each to every runtime registered to that account. | PekoHub `services/tunnel-manager.ts` |

The only way to make "the Hub keeps no messages" something people can verify is for the open-source runtime and app to seal content before it leaves the computer, so the Hub never holds it in readable form.

## 2. Decision

### D1 — The Hub sees routing, not content

Every envelope that one computer sends another through the Hub carries two parts:

- **The header**, readable: `type`, `requestId`, `sourceRuntimeId`, the recipient (`recipientRuntimeId`, `recipientAccount`, or `deviceId`), `sourcePrincipalDid`, `channelId`, and the fields the Hub uses to rate-limit (a piece's `offset` and `size`). These are exactly the fields of F3. Nothing the Hub does today needs more.
- **`sealed`**: everything else, including today's `signature` and `authorSignature`.

The types that change are the cross-runtime messages (`tunnel_channel_event`, `tunnel_channel_invite`, `tunnel_channel_history_request`, `tunnel_channel_history`, `tunnel_attachment_request`, `tunnel_attachment_chunk`, `tunnel_account_dms_request`, `tunnel_account_dms`, `tunnel_person_dm_invite`, `tunnel_dm_route`, `tunnel_peko_models_request`, `tunnel_peko_models`, `tunnel_peko_approvals_request`, `tunnel_peko_approvals`, `tunnel_peko_approval_decide`, `tunnel_peko_approval_decided`, `tunnel_read_mark`, `tunnel_group_invitation`, `tunnel_group_join`, `tunnel_group_join_request`) and the phone's (`tunnel_device_request`, `tunnel_device_response`, `tunnel_device_event`). Messages between a runtime and the Hub itself (`runtime_hello`, registration, `instance_*`, `group_register`, `group_roster`, transfers) are addressed to the Hub and stay readable: they are its directory, not anyone's conversation.

### D2 — Each computer has an agreement key, vouched for by its signing key

Each runtime has an X25519 **agreement key**: HPKE `DeriveKeyPair` over the label `peko-agreement-key/1` and its ed25519 signing key's seed. It never signs anything, and the signing key never decrypts anything. Derived, it needs no storage of its own and goes wherever the runtime key goes.

The runtime publishes a **key statement**: a compact JWS by its signing key over `{ v: "peko-runtime-key/1", runtime_id, agreement_key, iat }`. It sends the statement in `runtime_hello`. Once the runtime answers the challenge, the Hub keeps the statement and serves it: `runtime_keys_request { runtimeIds }` or `{ account }` → `runtime_keys { keys: [{ runtimeId, statement, vouches }] }`, and `GET /v1/runtimes/<id>/key` for phones. Every sealed envelope a runtime sends also carries its own statement in the sealed part (`senderKey`), so the recipient can answer without asking. A receiver accepts a statement only when it verifies against the `did:key` it names. So the Hub can withhold a statement or serve an older one, but it cannot make one: a runtime's `did:key` is all anyone needs to pin.

A phone already has an agreement key, enrolled with the QR code (ADR-108 D2). The runtime adds its agreement key to the signed payload of `tunnel_device_paired`, which the phone already checks against the runtime id it read from the screen. A phone paired before this fetches the statement from `GET /v1/runtimes/<id>/key` and checks it the same way.

### D3 — Sealing: sign, then seal to one recipient

The sender builds the envelope as today, signs it as today (ADR-058 D2/D3: runtime JWS, and the author's JWS where there is one), and seals the result to the recipient's agreement key: HPKE base mode (RFC 9180) with the suite ADR-108 D6 already uses, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305. `info` is `peko-tunnel-seal/1`. The associated data is the canonical header (its fields in a fixed order, one per line), so the Hub cannot move a sealed body under another header. `sealed` is base64url of the 32-byte encapsulated key followed by the ciphertext.

The receiver opens `sealed`, verifies the signatures as today, and refuses the envelope unless the signed payload names the same type, ids, source, and recipient as the header. A recipient cannot re-seal a message it received to some third computer and pass it off as addressed there, because the signed payload names the computer it was meant for.

An envelope addressed to an account (F7), and a read mark, becomes one envelope per computer of that account. The sender asks the Hub for the account's key statements (`runtime_keys_request { account }`, kept for a minute), seals a copy to each computer it accepts under D4, and adds `recipientRuntimeId` to the header. The Hub delivers each copy only to the computer it names, and only if that computer is registered to the account. A computer the Hub lists but D4 does not accept gets no copy.

There is one envelope per recipient computer, as today (F6). A group post to five computers is sealed five times; a membership change needs no rekeying, because each post is sealed to the computers of the members at that moment.

### D4 — Which computer is whose

Sealing protects content only if the sender seals to the right computer. The Hub still says which computer hosts a peko and which computers belong to a person. Each case is pinned on the strongest footing it has:

- **A phone.** Its key came from the computer's screen (ADR-108 D2). Fully verifiable.
- **The owner's own computers.** The owner pins each one's `did:key` by approving it on a computer they already use (ADR-109 D3, `own_computers.json`). Messages for the owner's account are sealed only to approved computers. The one exception is the DM-list request (ADR-095 D5), which carries nothing and which a computer not approved yet may get: it is answered only once approved. Fully verifiable.
- **A peko on another computer.** A peko's own signing key (ADR-058 D1) moves with it in snapshots. Its **host statement** is the proof it already makes on every announce (ADR-058 D4: a JWS by the peko's key over `{ runtimeId, principalDid, iat, exp }`). PekoHub keeps it and serves it in every directory answer as `hostStatement`. The runtime uses an answer only when that statement verifies by the peko's key and names the runtime PekoHub named. It pins the peko's key by Hub ID the first time (`runtime/peko_keys.json`). A move to another computer (ADR-081, ADR-095) brings a new statement from the same key, so it is not a key change. An answer with a different key for the same peko is not used. It is held as an approval request (`kind: "key_change"`, desktop: "a new identity") on the owner's pekos, and nothing is sent to that peko until the owner decides.
- **Another person.** The first time this computer seals to their account, it pins every computer the account has then (trust on first use, `runtime/sealing.json`). Later a new computer of theirs is sealed to once a computer of theirs that is already pinned vouches for it: a JWS by that computer over `{ v: "peko-computer-vouch/1", account, runtime_id, iat }`. A runtime publishes vouches (`runtime_vouch`) for every computer its owner approved under ADR-109, on every connect and on every approval. PekoHub keeps them, after checking they are signed by a computer of that account and name one, and serves them with the account's keys. Until a vouch arrives, the new computer gets nothing and the post waits in the outbox (ADR-072). The person is not asked anything.

The first meeting with a peko or person who is not already in a conversation here still rests on the Hub's word, exactly as Signal does when nobody compares safety numbers. A short code that both owners can compare is left open (§6).

### D5 — No plaintext, no downgrade

From the release that ships this ADR, a runtime sends the types of D1 only sealed and refuses them unsealed. A message whose recipient's key is not known yet waits up to 30 seconds for the Hub's answer. A computer whose key statement it cannot get receives nothing: a post stays in the outbox and is resent later; a request fails as unreachable. Nothing falls back to plaintext, so the Hub cannot force plaintext by withholding a key.

This is a wire break and ships in a minor bump (ADR-102). Runtimes on the older minor stop exchanging these messages with newer ones until they update. The Hub drops an envelope of these types that has no `sealed` field.

### D6 — Files

A file's digest and offsets move into the sealed body, so the Hub cannot test whether a known file is being passed around. The request id names the transfer in the header. Each `tunnel_attachment_chunk` seals its piece to the computer that asked. That is one more HPKE operation for each 256 KiB piece, about 40 for a 10 MB file. The receiver checks the assembled file against the digest the signed post carries, as today (ADR-094). The chunk header keeps `size` (the whole file's), so the Hub still enforces 10 MB a file. A piece is measured by its sealed length: at most a 256 KiB piece's base64 with 4 KiB of room. The per-minute byte cap counts 9/16 of the sealed length (base64 inside base64) less 1 KiB.

### D7 — Phones

The phone seals each `tunnel_device_request` to the runtime's agreement key. The runtime seals each `tunnel_device_response` and `tunnel_device_event` to the device's agreement key, as it already does for notifications. The device stream at the Hub stays as it is (ADR-108 D4); only the bodies change. A computer that no longer has a phone on record cannot seal an answer to it, so it sends none. The phone learns it was removed from `tunnel_device_removed`, as before. A phone's key is kept for a minute after it is removed, so the answer to its own `unpair` still reaches it. This closes ADR-108 §6.5.

### D8 — What stays readable, said where people see it

- **Visitor chat through the Hub's proxy** (ADR-091 D6): the Hub serves the page that would seal it, so sealing there would prove nothing. It stays readable by the Hub, and the website says so on the chat.
- **Metadata**: which computers and accounts talk, in which channel, when, and how much. Sizes are not padded.
- **The directory**: names, handles, public peko profiles, group names and rosters registered with the Hub, which are published by design.

The privacy text that claims the Hub keeps no messages names these three cases and links to this ADR.

## 3. Consequences

- The open-source runtime and app show that content leaves a computer sealed to the computer it is for. The claim that the Hub keeps no messages no longer depends on trusting the Hub. A breach of the Hub exposes metadata, not conversations.
- The Hub can still refuse to deliver, delay, or withhold key statements. It cannot read messages, and it cannot make a computer fall back to plaintext.
- No forward secrecy: anyone who records sealed traffic and later steals a computer's agreement key can read what was sealed to that key. Rotation bounds this (§6).
- Hub logs can no longer show what a message said; debugging cross-runtime problems relies on each runtime's own logs and the tunnel audit.
- The older minor stops talking to the newer one for these messages. Peko Desktop already names the runtime version it needs (ADR-102).
- Cost per message: one X25519 operation and an AEAD pass per recipient computer. Fan-out stays one envelope per computer.

## 4. Alternatives

- **Convert the ed25519 runtime key to X25519** (the libsodium conversion). This would save the key statement, but one key would both sign and decrypt. Deriving a separate key from the same seed (D2) keeps them apart at the cost of one lookup, cached for good.
- **Keep the agreement key in the vault beside the runtime key.** That means a second secret to store, back up and carry in snapshots. Derived, it follows the runtime key.
- **MLS (RFC 9420) group keys.** MLS gives forward secrecy and efficient large groups, but adds epochs, key packages, and a commit order between computers that can be offline for days. Peko groups are small and already fan out per computer, and sealing per post needs no state shared between members.
- **Seal posts only.** Approvals carry tool parameters, history carries whole conversations, and phone traffic carries everything the owner reads. All of them pass through the same relay.
- **Publish the PekoHub code instead.** Code in a repository does not show what runs on the server. People still could not check the claim.
- **Keep TLS and a written promise.** That is today's state, and it is why the audit raised this.

## 5. Implementation

All five phases are implemented, runtime and PekoHub together (each side refuses the other's unsealed messages, so they must ship together):

- **Runtime** (`feat/adr-110-sealed-envelopes`): `tunnel/seal.rs` (keys, statements, vouches, frames, and the `Sealer` in the tunnel client's read and write loops, so every handler keeps working on the opened message); `security/peko_keys.rs` and the directory check (`HubAgentDirectoryClient::with_peko_keys`); `key_change` approvals in `principal/policy_gate.rs`; vouches published by the dispatcher. The wire fixture shows sealed types as PekoHub sees them.
- **PekoHub** (`feat/adr-110-sealed-envelopes`): migration `0014_sealed_keys` (`runtimes.key_statement`, `instances.host_statement`, `runtime_vouches`); `services/runtime-keys.ts`; header-only relay; per-computer delivery of account-addressed copies; unsealed frames dropped; `GET /v1/runtimes/:did/key`.
- **Peko mobile** (`feat/adr-110-sealed-device-traffic`): `src/core/seal.ts`; sealed requests, opened answers and events. Tested against frames a real runtime sealed, and the runtime against a request the app sealed.
- **Peko desktop** (`feat/adr-110-key-change-card`): the `key_change` card, in every language.

## 6. Open

1. **Comparing a code.** A short code computed from both computers' keys, shown in the desktop and `peko contact verify`, so two owners can confirm a first meeting the Hub could have intercepted.
2. **Key rotation.** A new agreement key every N days, with the old one kept long enough to open what is still in flight. This bounds what a stolen key exposes.
3. **Padding.** Rounding sealed sizes up to fixed buckets so the Hub learns less from lengths.
4. **People with no computer** (ADR-108 §6.4). They use the proxy, which D8 leaves readable.
5. **Routes a person's computer names.** A group's home seals posts to the computer a member's join came from, and a DM to the computers a DM route names. Those come from a message that computer signed, relayed by PekoHub for that person's account, and are not checked against the account's pins. Checking them would close the last place where PekoHub's word picks the computer for someone else's account.
6. **Rotation.** The agreement key changes only with the runtime key. Rotating it on its own would need a version in the derivation and statements that name it.

## 7. Live run (2026-11-17)

A local PekoHub on this branch, with every frame it sent tapped, and two runtimes on different accounts (maya with a public peko Ada, and noah). Noah opened a DM with `@maya/ada` and posted a message with a marker word. Ada answered, and the answer arrived. Noah then asked maya (the person) to DM. Of 30 frames PekoHub sent, all 13 conversation envelopes (`tunnel_channel_invite`, `tunnel_channel_event`, `tunnel_channel_history_request`, `tunnel_channel_history`, `tunnel_person_dm_invite`) carried only header fields and `sealed`. Neither the marker word nor the answer appeared anywhere in what PekoHub handled. Maya's computer opened the DM request and filed it for her. Noah's computer pinned Ada's key and maya's one computer. Both computers' key statements and Ada's host statement were in PekoHub's database.

The phone library's live test (`peko-mobile/e2e/live.test.ts`) then paired with maya's computer and asked for its conversations. It watched them, posted to the DM with Ada, got Ada's answer as a live event, read the DM, moved its read mark, and unpaired. All 14 of its requests crossed PekoHub sealed, and its text never appeared in what PekoHub handled. The first run found that the answer to `unpair` could not be sealed once the phone's record was gone (the phone waited and timed out). The fix in D7 is the one-minute grace above.
