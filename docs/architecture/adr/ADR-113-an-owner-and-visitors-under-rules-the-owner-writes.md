# ADR-113: A peko has an owner and visitors; one file the owner writes says what visitors reach

**Status:** Accepted. D2–D8 and D10 are implemented for untrusted runs; D1 (retiring `trusted`) and the `trusted` half of D9 are not yet (§5).
**Date:** 2026-11-19
**Amends:** [ADR-066](ADR-066-pure-workspace-tooling.md) D1, [ADR-078](ADR-078-acting-for-stamps-and-trust-floor.md) §2, D6 and D7, [ADR-075](ADR-075-principal-owned-mcp-runtime.md) (`untrusted_ok`), [ADR-080](ADR-080-daily-memory-maintenance.md) (who gets the hot files).
**Touches:** `peko-rs/core/src/security/access.rs` (new), `security/floor.rs`, `tools/dispatcher.rs`, `tools/catalog.rs`, `principal/acting_for_prompt.rs`, `peko-rs/engine/src/prompt/{memory,renderer}.rs`, `ipc/packet.rs` and `ipc/handlers/principal.rs` (`send_as`), `peko-rs/cli/src/commands/send.rs`, `registry/packaging/` (snapshots carry the file).

---

## 1. Context

A peko is meant to serve more than its owner: family, a team, and a small business's customers. Four things in the runtime work against that today.

- **Three trust levels, of which one no longer earns its place.** ADR-078 has owner, trusted and untrusted. Trusted meant "I would hand this actor my laptop", but `Bash` became owner-only on 2026-11-02. What trusted still adds is a fixed bundle (the `MCP` tool, unmarked MCP servers, files outside the workspace), which an owner cannot shape.
- **What an untrusted run reaches is fixed in code.** The only thing an owner can set is `untrusted_ok` on an MCP server. Everything else is prose in `POLICY.md`, judged by a model. Prose that a caller can argue with is not an access control.
- **An untrusted run reads the whole workspace.** ADR-078 D6 confines it to the workspace, not within it. Any notes the peko keeps about one contact are readable by a run acting for another. The hot `kb/MEMORY.md` is also in every agent's prompt, so a visitor can ask the peko to recite the owner's memory.
- **A local caller is always the owner.** `peko send` speaks as the local user. A script that relays a customer's WhatsApp message through the CLI hands that customer an owner run.

ADR-066 D1 says a peko has no per-tool grants. The trust floor already decides tools by whom a run acts for, so that sentence describes the runtime of September, not today's.

## 2. Decisions

### D1. Two kinds of actor: the owner, and visitors

Everyone who is not the owner is a visitor: people, other pekos, and anonymous `visitor:<id>` callers. `Trust::Trusted` and the `trusted` permission are retired. Admission (who may talk to the peko at all) is unchanged.

### D2. `ACCESS.toml` says what visitors reach

One file at the workspace root, protected like `POLICY.md`, seeded create-once.

```toml
[visitors]                           # every visitor without a section of their own
tools = ["Read", "Write", "Edit", "Glob", "Grep", "Fetch", "Task", "ChannelRead", "ChannelSend"]
mcp   = []                           # MCP server ids (the folder name under mcp/)
read  = ["public/", "kb/CONVENTIONS.md"]
write = []
own   = "contacts/{conversation}/"   # read and write, this conversation only
ask_owner = false                    # D10: nothing from visitors reaches the owner

[contact."user:hub:<UUID>"]          # replaces [visitors] for this actor
tools = ["Read", "Write", "Edit", "Glob", "Grep", "Fetch", "Task", "ChannelRead", "ChannelSend", "MCP"]
mcp   = ["orders", "calendar"]
read  = ["public/", "playbooks/"]
own   = "contacts/{conversation}/"
```

- It is default-deny: a tool, server or path that is not listed is not reachable.
- A contact section replaces the default whole; sections do not merge.
- The dispatcher checks it for every call, before the reviewer, with no model involved. A tool that is not listed is also absent from the run's wire catalog.
- Contact keys are actor addresses and resolve to immutable keys, as every other authorization site does (ADR-081).
- Paths are workspace-relative and name a file, or a folder and everything under it. A path that leaves the workspace, names the whole workspace, or contains an `own` folder (a recursive search from it would read every conversation's folder) is an error.
- A file that does not parse, has an unknown key, or has any such error is refused whole. Visitors then get the built-in minimum (`Read`/`Write`/`Edit`/`Glob`/`Grep`/`Task`/`ChannelRead`/`ChannelSend`, their own folder, nothing shared) until it is fixed, and every owner turn's `<acting-for>` block says why.
- With no file, the seeded default applies. The seeded `tools` list is every built-in an untrusted run had before this ADR (all but `Bash`, `Plan` and `MCP`), so the file starts as a statement of today's tools for the owner to prune.

### D3. A floor the file cannot lift

Whatever the file says, a visitor run never gets `Bash` or `Plan`, never reads or writes the runtime's own state or another peko's workspace, never writes the protected set (its writes still become proposals), and never reaches another conversation's folder. Listing a floor item is a load error under D2.

### D4. A visitor's file tools reach only what the file names

A visitor run's `Read`/`Write`/`Edit`/`Glob`/`Grep`, and the paths it hands `Attach`, reach its own folder (read and write), the `read` paths (read) and the `write` paths (read and write). Every other path is refused, and the refusal names what the run can reach. This replaces "confined to the workspace".

A write to a protected file is still filed as a proposal (ADR-078 D6) whether or not the path is listed: it is never applied, and it is how a visitor's run suggests a change to the owner.

### D5. Each conversation has a folder of its own

`{conversation}` is the run's compartment (ADR-078 D3): one DM or one group. The folder is created on first write.

- A DM's folder is what the peko keeps about that one contact. A group's folder is shared by its members.
- The folder is named by the conversation's channel id, kept to letters, digits, `_` and `-`.
- The folder's `notes.md` is put into every turn of that conversation, up to 4 KiB, inside the `<acting-for>` block with runtime tags defused. The block also tells the run what it can reach.
- The folder is capped (`own_max_bytes`, default 10 MB); a `Write` or `Edit` that would pass the cap is refused and says why.
- The nightly memory pass may read these folders but promotes nothing from them to the hot files: ADR-080 D4 already requires an owner-only source.
- Deleting a contact's data is deleting the folder.

The key is the conversation, not the person, because the runtime cannot verify that two conversations are the same person. Joining two folders is the owner's act.

### D6. A visitor's prompt carries only what the visitor may read

A hot kb file reaches a visitor turn only if D4 lets that visitor read it. With the seeded default, a visitor turn gets `kb/CONVENTIONS.md` and its own `notes.md`, and not `kb/MEMORY.md` or `kb/index.md`. The role, `[identity]`/`[intent]` from `principal.toml`, the conversation's binding note (`kb/groups/<channel>.md`) and the role note reach every turn as before.

The engine cannot read `ACCESS.toml` (it sits below `core`), so the scope reaches it as a built-in prompt section, `kb_scope`, that the renderer reads and never shows the model: empty for runs the file does not bind, otherwise a marker line and the paths the run may read.

### D7. A local caller with the owner's authority may post as a named visitor

`peko send <peko> --as visitor:<id> [--name <display name>]`, carried as `send_as: {actor, name?}` on IPC `principal_send` and `principal_send_stream`. The visitor is `visitor:local:<id>`; the id is what the bridge knows the person by (1–128 letters, digits, or `. _ + @ -`). A Hub visitor cannot be named this way: PekoHub mints those.

The daemon accepts it only from an operator caller that holds the owner's `ManageSettings`, takes the same path as every other ingress (`ensure_child_ingress`), and stamps the run as that visitor, untrusted. Each use writes a `principal.visitor_vouched` Security audit event naming the caller and the visitor. The reply comes back on the send's own stream; `peko log <peko> --peer visitor:local:<id> --watch` follows what the peko posts later.

This is the whole of the bridge surface: an adapter for a chat service, email or a web form is a script outside the runtime that calls the CLI.

### D8. `POLICY.md` and the reviewer keep the judgment calls

`ACCESS.toml` decides what a visitor can reach. `POLICY.md` still decides what the peko should do with it, and the reviewer and owner approvals work as in ADR-078 D7.

### D10. Each section says whether its visitors' requests reach the owner

A personal owner wants a friend's request for one more file to reach them. A business wants nothing from a customer to queue for a person. `ask_owner` (default `false`) is one switch per section for the three ways a visitor's run can put something in front of the owner:

| | `ask_owner = true` | `ask_owner = false` |
|---|---|---|
| A call the section does not list (tool, MCP server, or path) | An approval request (ADR-078 D7) showing the exact call | Refused |
| A listed call the reviewer finds unclear | An approval request, as before this ADR | Refused |
| A write to a protected file | A proposal in `kb/inbox/`, as before | Refused |

- **Approve** replays exactly the call the owner saw, as the requester. The replay is checked as if that one tool or path were listed, so everything else still applies (a protected write is still a proposal).
- **Always** adds what the call needed to that actor's own `[contact]` section in `ACCESS.toml`, keeping the file's comments: the tool, the MCP server, or the path. An actor without a section gets one that starts as a copy of `[visitors]`, because a section replaces the default whole. The file is written only if the result is valid. (*Always* for a reviewer's unclear call still appends a rule to `POLICY.md`.)
- **Never askable:** `Bash`, `Plan`, the runtime's own state, a path outside the workspace, another conversation's folder, and a folder that contains those folders. These are refused under either setting, and an approved replay does not reach them.
- **At most 5 requests per conversation wait at once.** Further ones are refused until some are decided or expire (24 h). The owner's own tasks from a shared room (ADR-100) are not counted and are not affected by `ask_owner`.
- The run is told which case it is in, so a peko that cannot ask says it cannot do the thing instead of promising to check with the owner.

The seeded `[visitors]` has `ask_owner = false`: an anonymous caller should not be able to put requests on the owner's screen. The owner turns it on for the contacts they want to hear from. With it off everywhere, nothing a visitor does waits on the owner, which is also what makes one owner workable for a business.

### D9. Migration

`ask_owner = false` is also a change for existing pekos: a visitor's unclear call and protected write used to reach the owner and are now refused until a section turns requests on.

At creation, and at boot once per peko with no `ACCESS.toml`: the file is seeded, and every MCP server whose definition is marked `untrusted_ok: true` is added to `[visitors].mcp`. `untrusted_ok` is not read after that. Existing workspace files are not moved, so a visitor loses read access to everything outside `public/` and `kb/CONVENTIONS.md` until the owner lists it. That is a breaking change and goes out in a minor release.

When `trusted` is retired (D1), every `trusted` grant becomes a contact section with the `MCP` tool and all the peko's MCP servers.

Snapshots carry `ACCESS.toml`; its `[contact."<actor>"]` headers are rewritten across runtimes the way `POLICY.md` headings are.

## 3. Consequences

- ADR-066 D1 now reads: a peko has no grants of its own; what a run reaches follows whom it acts for and the owner's `ACCESS.toml`.
- A peko can remember each contact separately without one contact's run reading another's notes.
- Staff are contacts with a wider section. There is no staff level and no second approver. Where `ask_owner` is on, requests wait for the one owner; where it is off, nothing waits.
- A visitor id asserted through D7 is only as good as the script that asserts it. The runtime records who vouched; it does not verify the customer.
- `ACCESS.toml` bounds the runtime's tools. It does not bound what an MCP server's own credentials can do. A server that declares the reserved `acting_for` parameter (ADR-075) is told which actor each call is for and can scope its answers; for a visitor sent through D7 that actor is only what the bridge asserted.

## 4. Open questions

0. **Seeds cannot carry `ACCESS.toml`.** A seed's files stay inside `roles/`, `skills/`, `workflows/` and `mcp/` (ADR-107). A business seed needs to ship its visitor rules, which means the creator accepts them like the seed's other files.
0. **Files a visitor sends are not reachable from their conversation.** Inbound attachments are copied to `attachments/<date>/`, which is shared by every conversation and so is not listed for visitors. Images still reach the model as media; a visitor's document cannot be opened with `Read`. They should land in the conversation's own folder.

1. **Folder names.** The compartment's channel id is stable but unreadable to an owner browsing the workspace. A slug is readable but can change.
2. **The word "visitor".** It is also an actor kind (`visitor:<id>`). "Guest" for the trust word would avoid the clash.
3. **Reviewer cost.** Every reviewed call of a visitor run is a model call. At customer-service volume the owner may want listed tools to skip review. `ask_owner = false` removes the waiting, not the review.
4. **Skills, roles and workflows.** The catalogs in a visitor turn could follow `read`, or have a list of their own.
5. **Scoping MCP answers to the customer.** The reserved `acting_for` parameter already tells a server whom a call is for. What is missing is a documented pattern (and a reference server) for using it to return only that customer's records.
6. **Group creation.** `Channel` `create`/`invite`/`role` went to owner approval for trusted runs. With trusted gone, a listed `Channel` tool could keep that behaviour for any visitor.

## 5. Implementation status

Built (runtime, branch `feat/adr-113-visitor-access`): D2–D8, D10, the `untrusted_ok` half of D9, and snapshots carrying the file. `ACCESS.toml` binds **untrusted** runs.

Not built:

- **D1.** `Trust::Trusted` and `peko permit … trusted` still exist, and a trusted run is not bound by `ACCESS.toml`: it keeps what ADR-078 gives it. Retiring it changes the permission model in the CLI, the desktop and the tunnel, and is its own change.
- **D9, `trusted` grants.** Follows D1.
- Open questions 0–6.
