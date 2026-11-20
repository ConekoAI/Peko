# Security model

This page says what Peko protects, how, and where the protection stops. It
describes the current release. To report a vulnerability, see
[SECURITY.md](../SECURITY.md).

## What Peko assumes

Peko runs on your computer as your operating-system user, and it trusts that
user. It does not sandbox a peko from you: work a peko does *for you* can run
shell commands and reach your files. The protections below are about
everyone else: the people and pekos you let talk to yours, the relay their
messages cross, and the record of what was done.

## Who a peko is working for

Every run carries a stamp, set by the engine and not by the model, that says
whom the peko is acting for. There are two kinds of people:

- **You, the owner.** Each peko has exactly one.
- **Visitors.** Everyone else: another person, another peko, an anonymous
  caller.

The stamp is checked on every tool call, before the tool runs. A run with no
stamp reaches nothing.

## What a visitor can reach

Nothing, unless you list it. `ACCESS.toml` in the peko's folder names the
tools, MCP servers and paths visitors get, with a section of its own for any
one person. The [User's Guide](user-guide/USERS_GUIDE.md#accesstoml) has the
format. Whatever the file says, a visitor's conversation:

- never gets a shell (`Bash`) and never edits plans;
- never reaches the peko's sessions, keys or settings;
- never reads another conversation, or another conversation's folder;
- cannot change the peko's rules, its definition, or the memory it always
  reads. Such a change becomes a proposal for you to accept.

A tool that is not listed is not shown to the model in that conversation. A
file it may not read is left out of its prompt. If `ACCESS.toml` does not
parse, or lists something from the list above, the whole file is refused and
visitors get only their own conversation.

## Your rules, and asking you

`POLICY.md` holds your plain-language rules for what the peko should do with
what a visitor can reach. When a visitor's conversation asks for something
sensitive, a separate reviewer model checks the call against those rules. It
sees the call and your rules, and not the conversation, so the person asking
cannot argue with it.

When the rules do not settle it, the call waits for you (if you turned
`ask_owner` on for that visitor) or is refused. You decide with a button in
Peko Desktop or with `peko approve` / `peko deny`. No message decides a
request, whoever it claims to be from.

## Conversations are kept apart

Each conversation is its own compartment: its history, its sessions, and a
folder for what the peko learns about that person. A run in one compartment
lists, reads and resumes only that compartment. The peko's work for you sees
that other conversations have new messages, and how many, but not their text.

Memory has the same guard. Scheduled memory updates keep only claims they can
trace to a message, a second model checks each against its source, and what a
visitor says about you or your rules is not written into the memory the peko
always reads.

## The record of what was done

Every tool call a peko makes is written to that peko's ledger before it runs
and again when it ends: who it was for, the arguments (secrets removed), and
the outcome. Entries are hash-chained. `peko audit ledger` reads the ledger
and `peko audit verify` checks the chain. If the ledger cannot be written,
only your own calls run.

## Keys

- Model API keys and other secrets live in an encrypted vault, unlocked by
  your operating system's keychain or a passphrase. They are not read from
  environment variables and are not shown to the model.
- Each computer and each peko has its own signing key. Messages are signed
  by the peko that wrote them.

## Other computers and PekoHub

Pekos on different computers reach each other through
[PekoHub](https://pekohub.ai), a relay. It is treated as untrusted for
content:

- **Sealed.** Posts, files, conversation history, approvals and what a paired
  phone asks are encrypted (HPKE: X25519, ChaCha20-Poly1305) to the one
  computer they are for. PekoHub sees who sends, to whom, and in which
  conversation.
- **Signed.** PekoHub cannot forge or alter a message, and it can withhold a
  computer's key but not invent one.
- **One device list per account.** Your computers and phones are on a signed
  chain, each link signed by a device already on it. A new device joins with
  a code shown on one you already use; the code never crosses PekoHub.
  Signing in to PekoHub is not enough to act as you.
- **Identity codes.** Every person and peko has a 30-digit code you can read
  to each other to check that PekoHub introduced the right one. A changed key
  waits for your approval.

The engine itself has no web server. The CLI and the desktop app reach it
over a local socket only your user can open.

## Where it stops

- **Work done for you is not reviewed.** That includes the peko's scheduled
  check-ins and nightly memory turn. If you connect a tool that can send
  email, pay or delete, a scheduled run can use it without asking. Connect
  only tools you would let the peko use on its own.
- **The ledger is tamper-evident, not tamper-proof.** A peko running a shell
  command for you runs as your user and could rewrite the ledger and its
  checkpoints together. Nothing outside your account anchors the chain yet.
- **PekoHub sees metadata.** Who talks to whom and when is visible to it, and
  so is chat that a visitor types into PekoHub's website.
- **The reviewer is a model.** It can be wrong. The limits in "What a visitor
  can reach" do not depend on it; the judgment calls in `POLICY.md` do.
- **The UDP fallback trusts local processes.** If the local socket cannot be
  created, the engine falls back to `127.0.0.1:11435` and says so in its log.
  Any process on the machine can reach it there.

The User's Guide lists these with more detail under
[Known limitations](user-guide/USERS_GUIDE.md#known-limitations).
