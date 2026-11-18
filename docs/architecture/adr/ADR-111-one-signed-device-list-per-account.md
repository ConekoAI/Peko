# ADR-111: Each account has one signed list of its devices; a device joins by a code from one already on it, and every check asks that list

**Status:** Accepted (2026-11-18). Implemented in the runtime, PekoHub, Peko desktop and Peko mobile on `feat/adr-111-device-list`; §7 says where the implementation differs from the first draft.
**Date:** 2026-11-18
**Builds on:** [ADR-108](ADR-108-a-phone-is-a-paired-window-onto-your-computer.md) (D2: a phone enrolls with a secret shown on the computer's screen), [ADR-109](ADR-109-a-computer-speaks-as-you-once-you-approve-it.md) (a computer speaks as you only once approved), [ADR-110](ADR-110-what-crosses-pekohub-is-sealed-to-the-computer-it-is-for.md) (D4: whose computer is whose; §6.5), [ADR-078](ADR-078-acting-for-stamps-and-trust-floor.md) (D7: deciding is the owner's own action).
**Supersedes, once implemented:** ADR-109 D1–D4 (per-computer approval lists), ADR-108 D2's per-computer pairing record, ADR-110 D4's vouches and per-account computer pins.
**Counterpart:** PekoHub (keeps and serves each account's list, appends links in order). Peko desktop (one *Devices* page; joining a new computer). Peko mobile (joins like a computer; shows a code to add one).

---

## 1. Context

The 2026-11-18 review of the security model found that "which devices are this person's" has three answers, kept in four files, with no single record that decides:

| # | Device | How it is trusted today | Where it is kept |
|---|---|---|---|
| F1 | A phone | Scans a QR code from **one** computer, with a 32-byte secret that never crosses PekoHub (ADR-108 D2). Strong. | `runtime/devices.json`, on that computer only |
| F2 | Your other computer | Signs in to PekoHub, is held, and you click *Approve* on a card that names it by the last six characters of its `did:key` (ADR-109 D3). Each of your computers holds it and asks separately: with three computers, a fourth is approved three times. | `runtime/own_computers.json`, one per computer |
| F3 | Someone else's computer | Pinned the first time you write to them, then trusted when a pinned computer of theirs vouches (ADR-110 D4). | `runtime/sealing.json` and PekoHub's `runtime_vouches` |
| F4 | Routes in a conversation | Whatever computer a join, a DM route or a DM list names for a person, checked by whichever code wrote the route (ADR-110 §6.5). The 2026-11-18 DM-list bug (runtime #646) was one such place that forgot. | Remote-member rows of each channel |

Consequences:

- **F2 is weaker than F1.** A card with six characters gives the owner nothing to compare, so a computer registered with a stolen PekoHub sign-in is one click away from acting as them.
- **F2 and F3 disagree.** Another person's computer trusts your new laptop after one vouch from your old one; your own computers ignore that vouch and ask you again.
- **The approval cards are in the wrong place.** A computer, an owner change or a peko's new identity is filed on every peko's chat (ADR-109 D3), so five pekos show the same card five times, and a computer with no pekos can answer only through the CLI.
- **F4 has no single gate.** Sealing (ADR-110) and inbound authorship (`is_remote_member`) both take a route's word for which computer is a person's.

The usual answer is one signed list of an account's devices that every device and every contact checks: Signal's linked devices, Matrix cross-signing, Keybase's sigchain. Peko already has every key it needs: each computer's runtime key (`did:key`) and each phone's device key.

## 2. Decision

### D1 — An account's devices are one signed, append-only list

Each PekoHub account has one **device list**: a chain of links, each a compact JWS by a device already on the list.

```
{ v: "peko-device-list/1", account: "user:hub:<id>", seq, prev, op, device, iat }
```

- `seq` counts from 0; `prev` is the SHA-256 of the previous link (absent at 0).
- `op` is `add` or `remove`.
- `device` is `{ id, kind, name, agreement_key, hub_device_id? }` (`hub_device_id` names a phone's PekoHub stream). `id` is a `did:key`: a computer's runtime key, or a phone's device signing key. `kind` is `computer` or `phone`. `agreement_key` is its X25519 key (ADR-110 D2; ADR-108 D2 for phones).
- Link 0 (**genesis**) adds the first device and is signed by it.
- Every later link is signed by a device on the list as of `seq - 1`. An `add` also carries the new device's own JWS over the same payload, so nobody enrolls a key they do not hold.

A device is **on the list** when it was added and not removed since. Nothing else makes a device the account's: not a PekoHub sign-in, not the Hub's `runtimes` table, not an approval card.

### D2 — PekoHub keeps the list, and cannot rewrite it

PekoHub stores each account's chain and serves it (`device_list_request { account, from? }` → `device_list { account, from, links }`, to computers over the tunnel and to phones over their stream; there is no HTTP route). It pushes each new link to the account's computers and phones, and appends one only from a device of that account (`device_list_append` → `device_list_appended { ok, seq | error }`). It appends a link only when `seq` is the next one and `prev` matches, so concurrent appends are ordered and the loser retries on the new head. It checks signatures as a filter. The devices check everything again.

PekoHub can withhold links or serve an old head, but not forge one. Against that:

- A device never accepts a chain shorter than, or diverging from, the one it already holds.
- Every computer sends its account's head (`seq` and hash) in the sealed body of every envelope (`senderDevices`, beside `senderKey`). A receiver that sees a newer head for an account asks for the chain (at most every ten seconds per account) and acts on the envelope as it stands; the next envelope is checked against the longer list. A head that does not extend what it holds is a fork: it is refused and filed for the owner, like a key change.

### D3 — One way to join: a code from a device already on the list

A new device joins the same way whether it is a computer or a phone, and whichever kind of device lets it in:

1. On a device already on the list, the person chooses **Add a device** (desktop *Devices* page, the phone's settings, or `peko device add`). It shows a code of twelve Crockford base32 characters (`ABCD-EFGH-JKMN`), valid for ten minutes, and the same code as a QR (`peko://join?v=1&hub=…&code=…`). The first two characters are a public **nameplate** that names the attempt; the other ten (50 bits) are the password.
2. On the new device, they type the code or scan it. Case, spaces and hyphens do not matter, and O reads as 0, I and L as 1.
3. The two devices run **CPace** over ristretto255 (the balanced PAKE the CFRG recommends; `security/join.rs`) through PekoHub, keyed by the password and bound to the account and nameplate. Under the key it agrees, the joiner sends its device description and the holder answers with the exact payload of the link that adds it, each with a MAC; the joiner checks the payload adds exactly itself and signs it. The password never crosses PekoHub, and PekoHub gets one online guess per attempt, not an offline search. Three wrong attempts end a code. This replaces ADR-108 D2's HMAC, which is safe only with the 32-byte secret a QR code can carry: a code a person can type is short enough to search offline.
4. The device already on the list appends the `add` link, with the new device's own signature, and tells the joiner (`done { seq }`). Both screens say who joined.

The steps travel as `device_join { account, nameplate, to?, body }`. PekoHub relays them only within the account, by nameplate, sets `from` itself, and never sends a step back to its sender. A joiner takes only steps addressed to it.

A device that is not on the list says so on its own screen: *This computer is not added to your account yet. On a device you already use, choose Add a device and type the code here.* Nothing it sends as the owner is acted on meanwhile. Its posts stay in its own outbox (ADR-072) and go once it is added.

There are no approval cards for devices. Owner changes and peko key changes (ADR-109 D5, ADR-110 D4) remain approvals, but move with the rest to D6.

### D4 — Removing a device

Any device on the list removes any device (*Remove* on the *Devices* page, `peko device remove`, or "Sign out of this account" on itself). Every computer stops acting on it at the link it learns. A device that is removed and comes back must join again (D3).

If every device is lost, a new genesis starts a new chain. Contacts see that the person's devices were reset and are asked once, like a peko's key change (D5).

### D5 — Every check asks the list

One function answers everything that ADR-108, ADR-109, ADR-110 D4 and §6.5 now answer separately:

```
fn device_of(account, device_id) -> Option<DeviceKind>
```

- **Speaking as this computer's own account** (ADR-109 D2): applied only when `device_of(own, source)` is set. This replaces `own_computers.json` and `admits_own_account`'s held/approved/denied states.
- **A phone's request** (ADR-108 D3): accepted when it is signed by a `phone` device on the owner's list. Any computer on the list serves it, so a phone is no longer paired with one computer.
- **Sealing** (ADR-110 D3): every copy, whether addressed to an account or to a runtime a route names, is sealed only to a computer that `device_of(person, runtime)` admits. Routes stay for routing but no longer decide trust, which closes §6.5.
- **Inbound authorship**: a post, join or route speaking for `user:hub:<p>` is accepted only from a computer on `p`'s list.

Until this computer holds a person's list (the first message from or to them), routes are taken as they are and the list is asked for; from then on the list decides. This computer's own account is never taken on a route's word: until it holds its own list, nothing speaking as its account is acted on.

Another person's list is pinned by its genesis the first time this computer meets them: the same first-contact trust as today, but one pin per person instead of one per computer, and every later device comes with a signed link instead of a separate vouch. A chain with a different genesis is a reset (D4): it waits for the owner's decision. Lists live in `runtime/device_lists.json`; `sealing.json` keeps only keys. Vouches (`runtime_vouch`, `runtime_vouches`) are deleted.

This computer's own list is pinned only when this computer is on it and the owner's decision shows in it: this computer is the first device, or it signed its own entry (it joined by a code), or the list's first device or the device that added this one is one the owner approved before (D7). Otherwise it is shown, not trusted.

### D6 — The desktop has one place for this

A **Devices** panel in Settings lists the account's devices (name, kind, when added, by which device, last seen), with *Add a device* and *Remove*, or the join box on a computer that is not on the list. Owner changes, peko key changes and device resets move out of the peko chats into a **Security** list on the same panel, each shown once (IPC `security_decide`; `peko device approve|deny`). The phone shows the same list, and its *Add a device* shows the code as text.

### D7 — Moving to it

On upgrade, the first computer of an account to find no chain at PekoHub writes the genesis with itself. It then appends every computer its `own_computers.json` marks approved (keys from their PekoHub key statements, ADR-110 D7) and every phone in its `devices.json`, signing each as the adding device. For those links the new device's own signature is waived once (`via: "upgrade"`), since the old files already recorded the owner's decision. A computer that upgrades later and finds itself on the chain uses it. One that is not on the chain was not approved anywhere; it shows D3's message.

This changes the tunnel wire and on-disk formats, so it ships in a minor bump (ADR-102), together with PekoHub.

## 3. Consequences

- One record decides which devices are a person's, for their own computers, their phones, and everyone who writes to them. Four files and three mechanisms become one chain and one pin per contact.
- The ceremony that is strong for phones today becomes the only one, for computers too. A PekoHub sign-in alone never adds a device.
- Approving a computer happens once, on any device, instead of once per computer, and nothing about devices is filed on peko chats.
- PekoHub can still withhold or delay the list. Heads carried in messages make withholding visible to anyone the person writes to; a fork is refused.
- A stolen device that is still on the list can remove the others. That is the same as any linked-device scheme: the person removes it from another device, and the next link wins only if it extends the chain.
- First contact with another person still rests on PekoHub's word for their genesis, as ADR-110 D4 does for their computers. ADR-110 §6.1's comparison code would now compare one hash per person.

## 4. Alternatives

- **An account master key** (Matrix cross-signing): one more secret that signs the device list, which has to live on, or be synced to, every device that can add one, and be recovered when lost. A chain signed by the devices themselves needs no new secret, and losing one device loses nothing.
- **Keep per-computer approvals and let own computers accept vouches.** This fixes F2's repetition, but leaves three mechanisms and the six-character card. Routes would still need their own checks.
- **Keep ADR-108's HMAC for the code.** It is safe only with a long secret (QR). A typed code needs a PAKE, and one mechanism for both is simpler than two.
- **A short authentication string compared on both screens** (ZRTP, Bluetooth numeric comparison). It needs both screens in view and an extra step. Typing one code on one device is one step, and the PAKE gives the same protection.

## 5. Implementation

1. **Runtime: the chain** (`security/device_list.rs`): links, verification, the pin and head per account, `device_of`. Tests against a fixture chain.
2. **PekoHub**: a `device_links` table (account, seq, prev, link), ordered append, `device_list_request`/`device_list`/`device_list_append`, the join relay. Delete `runtime_vouches` and `device_pairings`.
3. **Runtime: joining** (`peko device add` / `join`, CPace through `device_join` relayed by account), the upgrade pass (D7), and heads in sealed bodies.
4. **Runtime: every check on `device_of`** (D5). Delete `own_computers.json`, the `computer` approval kind, `runtime_vouch`, and per-account computer pins.
5. **Desktop**: the *Devices* page, *Add a device*, the join screen on a computer that is not on its list, and the *Security* list (owner and key changes).
6. **Mobile**: join by code or QR, and *Add a device* showing a code; device requests served by any computer on the list.

## 6. Open

1. **The phone's QR**: the phone's *Add a device* shows the code as text; drawing it as a QR needs a QR library in the app.
2. **The comparison code** (ADR-110 §6.1) can now compare one genesis hash per person.

## 7. As built

- **CPace, not SPAKE2.** SPAKE2 needs fixed M and N points and has no maintained JavaScript implementation; CPace needs only hash-to-group, which `curve25519-dalek` (RFC 9496 `from_uniform_bytes`) and `@noble/curves` (`ristretto255_hasher.deriveToCurve`) both provide. The phone checks a vector the runtime wrote (`peko-mobile/tests/fixtures/join-vector.json`).
- **Heads in sealed bodies only**, not in `runtime_hello`: PekoHub reads `runtime_hello`, and the head is for contacts.
- **No HTTP route** for the list: phones hold a stream, and ask over it.
- **Who may remove whom**: any device on the list removes any device, except the last.
- **Limits**: 64 devices and 4096 links per account.
