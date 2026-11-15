# ADR-108: A phone is a paired window onto your own computer: it reads and posts through your runtime over the Hub, and push wakes it

**Status:** Proposed. Runtime and Hub side implemented (phases 1–3); desktop and the app are not.
**Date:** 2026-11-14
**Builds on:** [ADR-095](ADR-095-symmetric-dms-that-follow-devices-and-moves.md) (a person's DMs are kept whole on every runtime of their account; D7, your own peko is one conversation everywhere), [ADR-099](ADR-099-owner-decides-approvals-from-any-of-their-computers.md) (the owner decides approvals from any of their computers), [ADR-091](ADR-091-remote-dm-history-from-the-host.md) (D6, the Hub's proxy for clients without a runtime; D7, the Hub only relays), [ADR-078](ADR-078-acting-for-stamps-and-trust-floor.md) (D7, approvals are the owner's own action, never a message), [ADR-081](ADR-081-actor-identity-ownership-and-single-online-copy.md) (the local user and their Hub account are one actor).
**Counterpart:** PekoHub (device records, a device stream, relay of the device messages, push delivery; a hub ADR of its own). Peko desktop (pairing, a Devices list, read state from the runtime). A new mobile client.

---

## 1. Context

Remote chat is now the main way people use Peko: talking to their own pekos from another computer, groups with other people and their pekos, and approvals decided from anywhere. A phone is where people are when a peko writes back or needs a decision. Peko has no phone client, and the one path the Hub offers clients without a runtime was not built for one. Verified on `master` @ `caddd457` (runtime) and the PekoHub gap map of 2026-11-01:

| # | Fact | Where |
|---|---|---|
| F1 | Every person-facing surface (DMs, groups, boards, approvals, stop) is IPC on the person's own runtime. The channel verbs the desktop uses refuse callers that are not local (Unix socket, named pipe, loopback). | `peko_auth::caller` `CallerContext::is_local`; `ipc/handlers/channel.rs` (`[permission_denied] local callers only`) |
| F2 | A person's DMs are on every runtime registered to their account, each a full copy (ADR-095 D1, D5). Their own peko is the same conversation on every device (ADR-095 D7). A person's other runtimes are routes for their groups (ADR-092 phase 4). | ADR-095; ADR-092 |
| F3 | A client without a runtime has one path: the Hub's public chat proxy and its history route, one peko at a time (ADR-091 D6). Each message is one SSE stream, and the Hub keeps no conversation. A reply that comes after the stream closes (a parked run, ADR-089) or while the phone is in the background is lost. There is no push. The gap map marks it "not finished for mobile". | `tunnel/dispatcher.rs` `handle_proxied_request`; PekoHub `docs/GAP_MAP_2026-11.md` §2 |
| F4 | The proxy reaches pekos only. It has no groups, no boards, no DMs between people (ADR-095 D6), no approvals, and no stop. | PekoHub `routes/api/public-pekos.ts` |
| F5 | Approvals are decided only by the owner's own action, on the peko's computer or on another runtime of the owner's account, over a runtime-signed request (ADR-099 D2). | `principal/policy_gate.rs` `decide_request`; `tunnel/remote_approvals.rs` |
| F6 | Unread counts and OS notifications are worked out in the desktop's webview: read positions live in its `localStorage`, and a notification fires when a peko writes while its chat is not on screen. The runtime's read marks are per session (what a peko's run has been shown), not per person. | `peko-desktop/src/hooks/useDmActivity.ts`; `peko-rs/channel/src/read_marks.rs` |
| F7 | The Hub signs people in with OAuth: PKCE for the desktop, the device grant for the CLI (hub ADR-007). It vouches that a runtime speaks for its account (ADR-090 D3). It holds no messages and no list of whom a person talks to (ADR-091 D7, ADR-095 D5). | PekoHub `services/oauth.ts`, `speaksForItsOwnPerson` |
| F8 | The Hub keeps an in-app notification list for each account and sends nothing out: no email, no APNs, no FCM. | PekoHub `routes/api/notifications.ts` |

Two things follow. First, everything a phone should show is already on the person's own computer. Second, the phone cannot reach it, because only local callers may ask.

## 2. Decision

### D1 — The phone acts through one of your own computers, as you

A phone runs no runtime. It is **paired** with one or more of the person's computers, and every request it makes goes over the Hub to one of them. That runtime carries it out as its **local user** (the same actor as `user:hub:<account>`, ADR-081), exactly as it would for the desktop on that computer.

- There is one conversation record. A post from the phone is the local user's post in the existing channel. It fans out to every route as a desktop post does, and pekos see it with the same `Owner` stamp the desktop's post gets.
- The phone gets every conversation the person has on that computer: DMs with their own pekos, with other people's pekos, and with people; groups and their boards; approvals for the pekos hosted there; and, through ADR-099, approvals for their own pekos elsewhere.
- A phone paired with several computers uses whichever is online. They hold the same DMs (F2), so which one answers does not matter for DMs. For groups, any route serves.
- The Hub's public chat proxy (ADR-091 D6) stays as it is, for a client of someone who has no computer of their own. That case is out of scope here (§6).

### D2 — Pairing: the owner enrolls the phone on the computer, and the phone holds a key of its own

The phone's requests carry owner authority from outside the computer: it can post as the owner, approve calls, and stop runs. A Hub sign-in alone would make the Hub able to forge all of that, the trust ADR-091 §4 left "for later". For the phone, this ADR closes it:

1. **On the phone.** The person signs in to the Hub with OAuth, as a new first-party public client `peko-mobile` (authorization code + PKCE, custom-scheme redirect; hub ADR-007 gains the client). The phone generates a **device key pair** in its keystore. One signing key (ed25519) signs its requests. One agreement key (X25519) receives encrypted notifications (D6).
2. **On the computer.** The owner chooses *Add a phone* in the desktop, or runs `peko device pair`. The runtime mints a single-use pairing: an id and a 32-byte secret, valid for ten minutes. It shows them as a QR code together with the runtime's id (its `did:key`) and the Hub URL. The secret goes from the screen to the camera and nowhere else.
3. **The phone scans it** and sends `tunnel_device_pair { requestId, runtimeId, deviceId, proof }` through the Hub. `proof` is a compact JWS by the phone's signing key over `{ v, pairing_id, runtime_id, device_id, name, platform, signing_key, agreement_key, iat, exp, mac }` (keys are raw 32 bytes, base64url), where `mac` is HMAC-SHA256 under the pairing secret over the version tag, the pairing id, the runtime id, the device id, and both keys, one per line. The Hub relays it only when the phone's token and the runtime are on the same account. The JWS proves the phone holds the key it enrolls. The MAC proves the keys came from whoever saw the QR code: the Hub relays the proof but never sees the secret, so it cannot enroll a key of its own.
4. **The runtime checks** the pairing (unused, unexpired, for this runtime, and the MAC). Three wrong proofs end a pairing. It records the device in `<config>/runtime/devices.json` (`{ deviceId, name, platform, signingKey, agreementKey, pairedAt, lastSeen }`, readable by the owner only; at most 16 devices) and answers `tunnel_device_paired { requestId, deviceId, runtimeId, ok, error?, signature }`. Pairing is a Security audit event. The desktop's Devices list then shows the phone, with *Remove*.

The phone keeps the runtime's id from the QR code. Everything the runtime sends a device (`tunnel_device_paired`, responses, events, notifications) is signed with the runtime's key, and the phone accepts nothing that is not. So the Hub can neither act as the owner on the computer nor show the owner something the computer did not say (an approval request with a changed description, for one).

From then on the runtime accepts a phone's request only when **both** of these hold: the Hub relayed it from that account (as today), and it is signed by a device key recorded in `devices.json`. A compromised Hub can still read what it relays (ADR-091 D7 is unchanged), but it can no longer act as the owner on a computer. Removing a device deletes its record: on the computer (*Remove*, `peko device remove`), or from the phone ("Sign out of this computer", the `unpair` verb). The runtime tells the Hub `tunnel_device_removed { deviceId, runtimeId }`, which drops the pairing and tells the phone. The device's next request is refused.

### D3 — What a phone may ask: a short allow-list, not IPC

The phone does not get IPC. Each request is one **device verb**, carried by `tunnel_device_request { requestId, deviceId, runtimeId, signature }` and answered by `tunnel_device_response { requestId, deviceId, runtimeId, signature }`. Each `signature` is a compact JWS, as on every other signed tunnel message: the request's, by the device key, over `{ v, request_id, device_id, runtime_id, verb, args, iat, exp }`; the response's, by the runtime key, over `{ v, request_id, device_id, runtime_id, ok, body | error, iat }`. The runtime verifies the request against `devices.json`, checks freshness (five minutes, as ADR-099), and refuses a `requestId` it has already seen within that window, and any request issued before the runtime started (the seen list does not outlive a restart). It then carries the verb out as the local user. A phone never names an IPC request: the daemon has one table of device verbs (`daemon/state/devices.rs`), each built from fixed calls, and a verb exists only if this list names it. A refusal carries a tag the app can act on: `[unpaired]`, `[stale]`, `[replayed]`, `[unknown_verb]`, `[not_found]`, `[forbidden]`, `[bad_request]`.

| Verb | Does | Backed by |
|---|---|---|
| `conversations` | The local user's DMs and groups: name, the other member of a DM, last post, unread count and read mark (D5), and whether a run is working in it; and the group invitations waiting | channel store listing, `channel_invitations` |
| `read { channel, before?, after?, limit }` | One page of posts by `msg_id` cursor, oldest first, at most 100 | `channel_peek` / `history_page` |
| `post { channel, text, reply_to?, client_msg_id }` | Posts as the local user; in a DM with a peko hosted here, the peko's turn starts as for the owner's post from another computer (ADR-090 D5). The post's `msg_id` is derived from the device, the channel and `client_msg_id`, so a repeat finds the first post and posts nothing, however much later | channel port `post` |
| `mark_read { channel, msg_id }` | Moves the person's read mark (D5) | new |
| `approvals` / `approval_decide { peko \| channel, id, approve, always }` | Pending requests of the pekos the person owns, and the decision: here by the peko's name through `decide_request`; on another of their computers by the DM with that peko, through ADR-099 from this computer (one that does not answer in five seconds is listed as unreachable) | `approval_list` / `approval_decide`; `channel_peko_approvals` / `channel_peko_approval_decide` |
| `stop { channel }` | Stops the run answering in that conversation: in a DM, the person's thread with the peko; in a group, the runs of the pekos they own | `principal_stop` (ADR-048) |
| `board { channel }` | A group's board, read-only | `peko_channel::board` |
| `watch` / `unwatch` | Live events (D4). A `watch` repeated within 90 s keeps it open | the channel store's event signal |
| `unpair` | Removes this device's record on this computer (D2) | new |

Everything else is out of reach from the phone: creating, removing, exporting, or importing pekos; settings, models, credentials, quotas, and the vault; registering the runtime; accepting setup steps (ADR-104); and anything that runs `Bash` or reads files. A verb joins the list only through an ADR amendment. Every decision a phone makes is audited with `via_device`, like ADR-099's `via_runtime`.

### D4 — Live while open: a resumable stream through the Hub

While the app is in the foreground, the phone holds one WebSocket to the Hub (`/v1/devices/stream`, with its OAuth token) and sends a signed `watch` to each paired computer that is online.

- The runtime sends `tunnel_device_event { deviceId, runtimeId, signature }`, signed over `{ v, device_id, runtime_id, seq, event, iat }`, for what changes the phone's screen: a post in any of the person's conversations, a new conversation or invitation, a run starting or finishing in a conversation, an approval pending or decided, and a read mark moving. The Hub forwards it to the device's socket.
- The Hub tells the runtime `tunnel_device_gone { deviceId }` when the socket closes. The runtime ends the watch. A watch the runtime does not hear from for 90 s also ends.
- Events are `{ kind: "event", channel, event }` (a line appended to a conversation), `{ kind: "read", channel, msg_id }`, `{ kind: "run", channel, working }`, `{ kind: "approvals" }` (the list changed: ask again), and `{ kind: "resync" }` (events were missed: read again). Runs and approval requests are looked at every two seconds.
- **Resuming** needs nothing stored at the Hub. The phone calls `conversations`, then `read after=<newest msg_id it holds>` for each conversation that changed, and watches again. Missing events are only ever a gap the next `read` fills, so `seq` is for ordering, not for replay.

### D5 — Read state belongs to the person, kept on the runtime

Unread counts are worked out on the runtime, not in a webview (F6), so the phone and the desktop agree:

- Each channel gains the **person's read mark**, the newest `msg_id` the local user has seen, beside the sessions' marks (`read_marks.json`, key `person`). `mark_read` moves it forward only. The mark is the post's `msg_id`, and posts are ordered by their time, as history pages are (copies of a conversation number their lines differently). The desktop's IPC gets the same verb and stops keeping read positions in `localStorage`.
- A person reading on one computer marks it read on their others: the runtime sends `tunnel_read_mark { channel, msg_id }` to the account's other routes for that channel. The Hub checks it like a person's post (the source runtime speaks for the account). It wakes nothing and is never shown in the conversation. A mark that names a post the receiving runtime does not have yet is kept and applied when the post arrives.
- **Unread** means posts after the mark that the person did not write.

### D6 — Push when closed: the runtime decides, the Hub delivers ciphertext

The phone registers its push token with the Hub (`PUT /v1/me/devices/:deviceId { platform, pushToken }`). Nothing else about the phone's conversations is stored there.

- **When.** The runtime notifies a paired device on: a post by the other member in a DM; a post in a group that mentions the person, or a board item added `for` them (ADR-101); and an approval request for one of their pekos. It does not notify when the person's read mark already covers the post. This is the desktop's rule (F6), moved to the runtime so a closed app gets it too.
- **What.** The runtime signs `{ v, title, body, channel, msg_id, iat }` with its key, seals that to the device's agreement key (HPKE base mode, RFC 9180: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305; `info` is `peko-device-notify/1`; `sealed` is base64url of the 32-byte encapsulated key and the ciphertext), and sends `tunnel_device_notify { deviceId, runtimeId, collapseKey, sealed }`. The Hub hands the opaque blob to APNs (a `mutable-content` alert that the app's notification service extension decrypts) or to FCM (a data message). Apple, Google, and the Hub learn only that a notification happened, and when.
- **No double alerts.** The runtime holds a notification for five seconds and sends nothing if the person's read mark covers the post by then (they are reading on a computer). The Hub drops a notify for a device whose stream is open, since the app shows it live. `collapseKey` is the channel, so a burst replaces rather than stacks. When the read mark moves past a notified post (read on the desktop), the runtime sends a silent clear (`tunnel_device_notify` with `clear: true` and no `sealed`), and the phone removes the notification.
- **Approvals** open the app on the request, where *Approve* / *Deny* are the D3 verb. A notification action never decides anything: deciding stays an action inside the signed-in app (ADR-078 D7, and an OS lock screen is not the app).

### D7 — When the computer is asleep

A peko runs on a computer. When no paired computer is online, the phone has nothing to ask and no peko there would answer. The Hub knows which of the account's runtimes are online and tells the phone over the stream.

- The phone shows the conversations it last read, marked as possibly out of date, and says the computer is asleep and its pekos will answer when it wakes. The words are the desktop's: "computer", never "runtime" or "daemon".
- **Posts written meanwhile wait on the phone**, in order, keyed by `client_msg_id`. They are sent when a paired computer comes online and the app is open. The Hub stores nothing (ADR-095 §3 keeps that a separate decision). The phone shows each one as waiting until it is sent.
- How to keep a peko reachable while its computer sleeps (keep-awake, a computer that stays on, a hosted runtime) is a separate decision. This ADR only makes the state visible.

### D8 — The Hub: device records, a stream, relays, push

The Hub stores, per device: `deviceId`, account, the runtimes it is paired with (learned from the `tunnel_device_paired` and `tunnel_device_removed` it relays), platform, push token, and last seen. It relays the D2–D6 messages only between a device and a runtime **of the same account**, rate-limits per device, and keeps no message content, conversation list, or notification text. The account's existing notification list (F8) is unchanged. Hub details (schema, socket handling, APNs/FCM credentials) go in a hub ADR.

### D9 — The client

One app for iOS and Android, in a new repository, written with React Native (Expo) in TypeScript. It shares the wire types with the desktop through a package generated from the runtime's contract samples (`tunnel/fixtures/tunnel_messages.json`), as the desktop's `check:ipc` already checks. Push decryption needs native code (an iOS notification service extension, an Android messaging service), which Expo supports through config plugins. The app shows the same conversation, board, and approval views as the desktop, with nothing about the runtime's internals.

## 3. Alternatives considered

- **Extend the public chat proxy** (ADR-091 D6) with a stream, history, and push. It serves one peko at a time from the peko's host, so groups, people DMs, boards, approvals, and stop would each need a proxy of their own, and a person's conversations would be scattered across hosts. The person's own computer already has all of them (F2).
- **Run a runtime on the phone.** Phones suspend background work, so it would be offline most of the time. It would compete with the computer for the single online copy (ADR-081), and it would duplicate every conversation onto the device that is most often lost.
- **Trust the Hub sign-in alone** (no device key). Simpler pairing, but the Hub, or anyone holding a stolen token, could post as the owner and decide approvals on every one of the person's computers. A key enrolled on the computer costs one QR scan.
- **Give the phone IPC over the tunnel.** IPC has nearly a hundred requests written for local callers, many of them owner-only settings. An allow-list keeps "what may a phone do" one table long.
- **The Hub stores conversations, or unsent posts, for offline phones.** Rejected, as in ADR-091 and ADR-095. The Hub stays a relay.
- **Plain-text push.** Apple or Google would read every message a peko sends.
- **Tauri 2 mobile, reusing the desktop's UI.** The desktop's views are wired to local IPC through Tauri commands, so the reuse is smaller than it looks. Push extensions and background reconnect are weaker there, and they are the core of this client.
- **A web app with web push.** Cheapest to build, but on iOS it needs installing to the home screen, and it cannot run code to decrypt a notification before it is shown.

## 4. Consequences

- Owner authority reaches outside the computer for the first time through something that is not a runtime. It is bounded by the D3 list, by a key the owner enrolled on that computer, and by the Hub's account check. It is visible in the Devices list and in the audit log (`via_device`).
- The phone is as useful as the person's computers are awake. The "computer asleep" state is expected, and it will make an always-reachable peko a sooner question.
- The desktop's unread counts and notifications move to the runtime (D5, D6). The desktop and phone agree, and a person with two computers sees one read state.
- The Hub gains personal data it did not hold before: push tokens and device names. Its privacy text must say so.
- No IPC request, on-disk format, or tunnel wire type changes shape. Everything here is added, so the runtime work can ship in patch releases (ADR-102).

## 5. Implementation (phases, each its own PRs)

1. **Runtime: pairing and the read verbs.** `local/devices.json`. IPC `device_pair_start` → `{ code, qr }`, `device_list`, `device_remove`; CLI `peko device pair|list|remove`. Tunnel `TunnelDevicePair` / `TunnelDevicePaired` / `TunnelDeviceRequest` / `TunnelDeviceResponse` / `TunnelDeviceRemoved` beside the other tunnel messages (`tunnel/protocol.rs`), with contract samples. `tunnel/devices.rs`: verification, the replay window, and `Identity::Device`. The verbs `conversations`, `read`, `post`, `mark_read`, `unpair`. The person read mark in `peko-channel` and `tunnel_read_mark`. A scripted device client for tests (a key pair plus the messages, no app), driven against a local Hub like the collab benchmark.
2. **Hub: devices and relay.** The `peko-mobile` OAuth client, device records, `/v1/devices/stream`, the relays with the same-account check, and rate limits.
3. **Runtime + Hub: live and push.** `watch` / `tunnel_device_event` / `tunnel_device_gone`. The notify rule, HPKE sealing, `tunnel_device_notify`, and silent clears. Hub APNs and FCM senders, and the drop-when-streaming rule. The `approvals`, `approval_decide`, `stop`, and `board` verbs.
4. **Desktop.** *Add a phone* (QR), the Devices list, and unread counts and notifications read from the runtime instead of `localStorage`.
5. **Mobile app.** Sign-in, pairing, conversations, chat, board, approvals, stop, the asleep state with waiting posts, and push with decryption. Released through TestFlight and Play internal testing first.

Phases 1–3 are tested end to end with the scripted device client before phase 5 starts, so the app is written against a contract that already works.

## 6. Open

1. **Key storage.** ed25519 in the software keystore (Keychain, Android Keystore-wrapped) matches the runtime's keys. P-256 in the Secure Enclave or StrongBox is hardware-bound but adds a second signature scheme to the runtime.
2. **Attachments.** Viewing them through the paired computer (ADR-094's chunked pull), and sending photos from the phone. Left for a later amendment.
3. **Owner briefs in groups** (ADR-100 `as_owner`) and **setup acceptance** (ADR-104). Should the phone approve them like approvals, or leave them to the computer?
4. **People with no computer.** A person who only talks to others' pekos and people from a phone has no runtime to act through. The proxy stays for them, unfinished (F3).
5. **Encrypting the device stream end to end**, so the Hub no longer sees content in transit. This is the same open question as for runtime-to-runtime traffic (ADR-091 D7).

## 7. Changed in review (2026-11-14)

The first draft was reviewed against the runtime and the Hub before any code was written. What changed, and why:

1. **The pairing secret never crosses the Hub** (D2). The draft sent the pairing code through the Hub and had the phone sign it. A Hub that saw the code could have enrolled a key of its own, which is the one thing D2 says it cannot do. The secret now stays in the QR code; only a MAC made with it crosses.
2. **The runtime signs what it sends a phone** (D2–D4, D6). Without that, a Hub could show the owner an approval request with a changed description and collect a real approval for it.
3. **A phone can sign itself out, and the Hub is told** (D2, D3): the `unpair` verb and `tunnel_device_removed`. The draft named the action and had no message for it.
4. **A restart does not reopen the replay window** (D3): requests issued before the runtime started are refused.
5. **The read mark is a `msg_id` ordered by time** (D5), because copies of a conversation number their lines differently, and a mark for a post that has not arrived is kept.
6. **A notification waits five seconds** (D6), so someone reading at their computer is not buzzed for every reply.
7. **No new caller kind** (D3). The draft's `CallerContext::Device` would have made every IPC handler decide what a device may do. The verbs are one table in the daemon instead.
8. **Where things live.** Tunnel messages are in `tunnel/protocol.rs` (not `peko-protocol`), and `devices.json` is beside the runtime's other files.

Not done here, and still open: avatars in `conversations`; the desktop (phase 4) still keeps its own read positions; push has not been sent to Apple's or Google's services.

## 8. Live run (2026-11-15)

A scripted phone, written from this document with Node's own crypto and sharing no code with the runtime, was run against a local Hub and three isolated runtimes (two on one account, one on another) with a mock model. Pairing, every verb, live events, the read mark, approvals and push (to a stand-in for Apple's service, the notification opened and its signature checked by the phone) all work together. The run found and fixed:

1. **A turn started by a post from a phone could not be stopped and did not show as working.** `human_dm_post` (also the path for a post from the person's other computer, ADR-090 D5) did not register the turn. It registers now, so `stop`, `peko stop` and the desktop's stop button reach it.
2. **A read mark that is not a post's id was kept as a mark to apply later.** It is refused (`[bad_request]`).
3. **An approval request buzzed twice**: once for the request and once for the peko's notice of it in the owner's DM. The notice is not notified.

Seen in the run and left as they are:

- A request is refused as issued before the start only when it is more than 60 seconds older than the start, the same allowance clock skew gets. In that window a request captured before a restart can be sent once more.
- A notification sent before a restart is not cleared by a later read on the computer, because the runtime remembers what it notified in memory. The app clears on open.
- A computer that joins the account later starts with no read mark for conversations it copies; the mark arrives the next time it moves.
- Not from the phone work, fixed afterwards (ADR-095 D5): a person's own post made on one computer was refused by their other computer when forwarded live, and arrived only with the next history sync. Posts queued in that form before the fix are still refused until they expire from the outbox.
