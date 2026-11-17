# ADR-109: A computer speaks as you only once you approve it on one you already use; an owner change from PekoHub waits for the owner too

**Status:** Accepted. Runtime and desktop implemented on `fix/tunnel-challenge-signing-oracle` (runtime) and `fix/hub-token-stays-with-its-hub` (desktop). PekoHub unchanged.
**Date:** 2026-11-17
**Builds on:** [ADR-090](ADR-090-local-user-dms-a-remote-peko-over-the-tunnel.md) (D3: a person's envelope is vouched for by their runtime and the Hub's binding of that runtime to their account), [ADR-095](ADR-095-symmetric-dms-that-follow-devices-and-moves.md) (a person's DMs follow them to every runtime of their account), [ADR-099](ADR-099-owner-decides-approvals-from-any-of-their-computers.md) (the owner decides approvals from any of their computers), [ADR-108](ADR-108-a-phone-is-a-paired-window-onto-your-computer.md) (a phone acts as the owner only with a key enrolled on the computer), [ADR-081](ADR-081-actor-identity-ownership-and-single-online-copy.md) (D4: ownership changes by a PekoHub transfer), [ADR-078](ADR-078-acting-for-stamps-and-trust-floor.md) (D7: approvals are the owner's own action).

---

## 1. Context

The 2026-11-17 audit of PekoHub and the tunnel found that a PekoHub sign-in is enough to act as the owner on every one of their computers:

| # | Fact | Where |
|---|---|---|
| F1 | Any PekoHub session or `pkr_` API key registers a new computer to the account; the only safeguard is a notification afterwards. | PekoHub `routes/api/runtimes.ts` `POST /runtimes/register` |
| F2 | A peko's computer accepts `user:hub:<account>` from any computer the Hub says is registered to that account. When the account is the peko's owner the run is `Owner`: it gets `Bash`, and the owner's approvals (ADR-099), models (ADR-097), DM history, and DM list follow. | `tunnel/dispatcher.rs` (ADR-090 D3 path), `security/stamps.rs` `trust_for`, `tunnel/remote_approvals.rs` |
| F3 | When PekoHub says a peko's owner changed (a transfer accepted on the website, or an announce it refused), the runtime rewrites the peko's owner to the account PekoHub names, with no step on this computer. | `tunnel/dispatcher.rs` `sync_owner_from_hub` |
| F4 | ADR-108 already closed this for phones: a phone acts as the owner only with a device key enrolled from a code on the computer. The owner's other computers were left on the Hub's word. | ADR-108 D2 |

So whoever holds the person's GitHub or Google account, a leaked API key, or PekoHub itself could run commands on their computers, either by registering a computer (F1, F2) or by giving their pekos to another account (F3).

## 2. Decision

### D1 — Each computer keeps the list of the owner's computers it trusts

`runtime/own_computers.json` lists the runtimes (`did:key`) that may speak for this computer's account (the local user's Hub account): each with a status (`approved`, `pending`, `denied`), when it was first seen, and when and where it was decided. The computer itself is always trusted. A computer is trusted only when this file says `approved`. The Hub's binding stays necessary (the Hub still relays only from a runtime registered to the account), but it is no longer enough.

### D2 — A computer that is not approved is held, not obeyed

A message from another runtime that speaks for this computer's account (a post, a DM invite, a history, DM-list, models, or approvals request, an approval decision, a DM route, a read mark, a group join) is not applied unless the runtime is approved. A post or invite is left unacknowledged, so the sender's outbox keeps it and resends it (ADR-072). A request gets no answer. The first such message files the computer as `pending`. A `denied` computer is refused without asking again.

People other than the owner keep ADR-090's footing: the Hub vouches for them, and their trust comes from the owner's grants. This ADR is about the owner's own account.

### D3 — The owner approves it where they already decide approvals

A pending computer is one more approval request (`kind: "computer"`, id `apr_computer…`; ordinary requests are `apr_<hex>`) on every peko the owner has on this computer: in the desktop's approval cards, `peko approve` / `peko deny`, from an approved computer (ADR-099), and from a paired phone (ADR-108). It reads "A new computer signed in to your PekoHub account wants to use your pekos as you". It is approved once, never *always*. Approving it on any peko approves the computer for all of them. Approving sends the computer an empty receipt for each conversation it posted in while it waited, and a runtime that receives an empty receipt resends what waits for the sender at once, so the held posts arrive without waiting for the outbox's backoff.

`peko device list`, `peko device approve` and `peko device deny` do the same from the terminal, also on a computer with no pekos; `peko device remove` stops trusting a computer as it removes a phone. One `device` command covers phones and computers.

### D4 — Computers that already talk to this one are approved on upgrade

The first time the file is created, every runtime already recorded as carrying this account's person in a conversation here is approved (`via: "upgrade"`). Nobody who already uses two computers is asked again.

### D5 — An owner change from PekoHub waits for the owner

A `principal_owner_changed` that this computer did not ask for (no `requestId`: a transfer accepted on the website, or an announce PekoHub refused) no longer rewrites the owner. It files an approval request (`kind: "owner_change"`) on that peko: "PekoHub says <name> now belongs to <account>. Hand it over?". Approving applies the change and announces the peko again; denying keeps the owner, and the peko stays off PekoHub until the owner transfers it or changes it back on the website. A transfer this computer asked for (`requestId` set) applies as before.

### D6 — PekoHub's proxy does not act as the owner

PekoHub mints the bridge tokens its proxy carries for clients without a runtime (ADR-091 D6), so a proxied request whose token names this computer's own account is refused. The owner reaches their pekos from their computers and paired phones.

## 3. Consequences

- PekoHub, a stolen sign-in, or a leaked API key can no longer act as the owner on a computer, or give their pekos away, without the owner's action on a computer they already use. With the tunnel's challenge fix (2026-11-17), the Hub cannot forge an approved computer's signature either.
- The first message from a new computer waits until it is approved. On the new computer it shows as not delivered yet.
- The approval shows no code to compare: the owner recognises the new computer by having just started using it. A short code shown on both computers is a possible follow-up.
- A new computer still takes the Hub's word for which computers are the account's when it syncs DMs from them (ADR-095). That affects what it shows, not what runs on the older computers.
- People other than the owner, and `Trusted` grants to them, still rest on the Hub's vouching (ADR-090).

## 4. Alternatives

- **Pair computers with a code, as phones are (ADR-108).** Strongest, but every new computer needs a step before it can chat. Kept as a possible follow-up.
- **Harden PekoHub only** (API keys cannot register computers, delays, alerts). Leaves the Hub, and anyone who gets in, able to act as the owner.
