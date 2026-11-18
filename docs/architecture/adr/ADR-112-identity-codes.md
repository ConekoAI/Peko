# ADR-112: Every person and peko has a code, which two owners read to each other to check PekoHub's word

**Status:** Accepted (2026-11-18)
**Date:** 2026-11-18
**Builds on:** [ADR-110](ADR-110-what-crosses-pekohub-is-sealed-to-the-computer-it-is-for.md) (§6.1 left a comparison code open; D4: a peko's key is pinned on first sight, a new one waits as `key_change`), [ADR-111](ADR-111-one-signed-device-list-per-account.md) (a person's devices are one list, pinned by its first link; a new first link waits as `devices_reset`).

---

## 1. Context

Two moments rest on PekoHub's word alone:

- **The first meeting.** The first time this computer meets a peko, it pins the signing key PekoHub names. The first time it meets a person, it pins the device list PekoHub serves.
- **A change.** PekoHub names a peko with a new key, or serves a person's list with a new first link. The conversation waits, and the owner is asked to approve.

Before this ADR, the owner had nothing to check either one against. They would approve anyway, or not at all. Signal's safety numbers, WhatsApp's security codes and Matrix's emoji verification answer this the same way: the two people compare something derived from the keys, over a channel the server does not control.

## 2. Decision

### D1 — A code per identity

Each identity has one code: thirty digits in six groups of five (`62659 39535 75152 38949 74100 79799`).

- **A person's code** is computed from the hash of their device list's first link. It stays the same as devices are added or removed, and changes only when the list starts over.
- **A peko's code** is computed from its signing key (`did:key`).

`code = six 5-byte chunks of SHA-512ⁿ(lp("peko-code/1") ‖ lp(kind) ‖ id), each mod 100000`. Each round hashes the previous hash with `id` again, for 5200 rounds; `kind` is `person` or `peko`. This is how Signal makes each half of a safety number. Finding another identity with the same code takes about 2^100 hashes.

The code is per identity, not per pair. An owner reads their own code from their own screen. Anyone else compares it with the code their computer holds. One number checks one identity, and it is the same for everyone who checks it.

### D2 — Where it shows

- **Your account's code**: Settings → Devices on the desktop, the phone's Devices list, and `peko code`.
- **Your peko's code**: its profile on the desktop, and `peko code <peko>`.
- **Someone else's peko**: its profile on the desktop, and `peko code peko:hub:<id>`. This is the key pinned here.
- **A person**: `peko code user:hub:<id>`, from the list held here.
- **A request to trust something new**: `key_change` and `devices_reset` requests carry the code of what they would trust (`code`). A key change also carries the code of the key trusted until now (`pinned_code`). The desktop's Security list and `peko device list` show them, with: *approve only if its owner reads you the same code.*

IPC: `identity_code { of }` → `identity_code_result { of, name?, code? }`. `of` is `account`, `user:hub:<id>`, `peko:hub:<id>`, or one of your pekos by name or id. `device_list` gains `code`, and its `security` items gain `code` and `pinned_code`.

### D3 — Comparing decides nothing by itself

A code is a check the owner makes, not a state the runtime keeps: there is no "verified" mark. A change still waits for the owner's decision (ADR-110 D4, ADR-111 D4). The code is what lets them make that decision knowingly.

## 3. Consequences

- A change can be approved for a reason: the owner reads the code to the peko's owner on a call, or in person, and approves only if it matches.
- A first meeting can be checked at any time after it happens, from the profile.
- An owner who never compares is exactly as protected as before. Nothing gets worse.

## 4. Alternatives

- **One number per pair** (Signal's 60-digit safety number). It covers both sides at once, but twice as many digits must be read, and it does not fit a peko whose owner is not part of the conversation.
- **Emoji or words** (Matrix SAS). Easier to read aloud, but that needs an interactive exchange between both devices. A fixed code can be read off a profile at any time.
- **A "verified" mark that warns when a verified identity changes.** Useful, but it is state that must be kept, synced across the owner's computers and explained. It is left open.

## 5. Open

1. A verified mark, kept on the account's devices, that makes a later change to a verified identity louder.
2. A QR code of the code, for two people with phones in the same room.
