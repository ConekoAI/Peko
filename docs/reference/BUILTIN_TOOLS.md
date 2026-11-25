# Built-in Tools Catalog

Peko exposes **31 built-in tool names**, all in PascalCase. Task, Plan, Cron, Async, Contacts, and Channel each require an explicit `action`; no former per-action tool names are aliases. Session also rejects fields irrelevant to the selected action. This reference
describes the compiled Tool implementations; the emitted descriptions and
JSON Schemas in source are the executable contract. MCP and workspace tools
have their own names and schemas and are outside this inventory.

Every registered tool appears in the native wire catalog (ADR-066), except the
ones the ADR-078 trust floor refuses the run (`Plan`, `Bash`, `Secret`, `Changes`, `Browser` and `Computer` below owner)
and, for an untrusted run, the ones the owner's `ACCESS.toml` does not list
(ADR-113). ToolCatalog
owns registration/lookup; ToolDispatcher validates arguments, writes each call
to its principal's tool ledger (ADR-079), and emits one attributed audit event. Every principal can use every registered tool. There is
no tool allowlist or capability filter. Principal bindings select workspace and
service dependencies; ownership, peer permissions, and channel membership
control access to resources.

The wire catalog states each field's schema once: in a multi-action tool's
`oneOf` variants, a field whose constraints match the top-level field is sent as
`{}` (`ToolMetadata::to_tool_definition`), so the model sees the same accepted
calls in far fewer tokens. The dispatcher validates against the full schema and
explains a failure for the action the caller chose — a missing field, a field the
action does not take (with the ones it does), an either/or group, or an unknown
action. A call that cannot do what was asked (unknown id, missing channel,
unknown skill, refused run) is a tool error, never a success carrying an
`error` field.

## Naming

Built-in wire names are exact PascalCase names: `Session`, `ModelList`,
`RoleCatalog`, etc. Old spellings are not aliases. MCP and workspace tool names
remain exactly as registered. Rust module names, IPC operation tags, action
values, and parameter names retain their existing spelling.

## Registration and lifetimes

[installation.rs](../../peko-rs/core/src/tools/installation.rs) owns the factories,
installation phases, and the 27-name inventory. The complete inventory derives from its manifest.

| Lifetime | Tools | Installation |
|---|---|---|
| Runtime defaults | Read/Write/Edit/Glob/Grep/Bash, Cron, ChannelRead, Attach, Suggest, Ask, Fetch, Secret, Browser, Computer | Runtime startup; fill missing defaults without replacing configured instances |
| Daemon services | ModelCall, Workflow, caller-aware Session and Agent | After PrincipalManager and daemon services exist |
| Principal workspace | Skill, RoleCatalog | Once per principal; RoleCatalog scans current role files on invocation |
| Principal services | Task, Plan, ChannelSend, Contacts, ModelList, Async, MCP, ToolLog | When their session storage, plan, caller identity/channel, or inbox bindings become available |
| Run bindings | Agent, Session | Private catalog overlay for caller execution dependencies |

Each run inherits live runtime/principal registrations through its overlay.
Installing a run executor never replaces another run's binding. Workflow/IPC
callbacks resolve the active overlay by the attributed principal and caller
session; when no run is live, daemon Agent/Session adapters resolve principal
services per call. All calls use the same dispatcher implementation, hooks,
audit sink, and timeout router. Run admission remains principal-wide. ChannelSend
keeps principal identity/reply locks and resolves the current tunnel context per
call, including connections made after installation.

Async executors and task registries belong to the principal, so receipts remain
resolvable after a run ends. Async action spawn stamps the caller session on each task;
completion events go to that session's inbox. Bash `run_in_background` takes
the same path as `Async action=spawn tool=Bash`: the dispatcher hands every call
from a principal a background spawner backed by that principal's Async runtime,
so both register one task in the principal's executor. A background task's body
runs inline under its own optional timeout and cancellation — it is not subject
to the foreground wait — and streams live output to `Async output`.

A foreground call waits at most 10 minutes, and less when someone writes: a
message from the conversation (a person or a peer peko) that arrives while the
call runs lets it go on for 2 more seconds, then detaches it, so the run
answers the message instead of making them wait on the tool. A detached call
returns a receipt (`_async_status: "queued"`, `task_id`, `detached_after_secs`,
`reason`: `timeout` | `message`, and a `note` on how to follow up), the work keeps running
with no time limit, and its completion is delivered to the calling session — it
starts a turn when the session is idle and reaches a running loop at its next
iteration. Background work has no time limit by default: Async spawn, background
Bash, cron tool jobs, Workflow, and Agent runs are bounded only by a limit the
caller sets explicitly.
Each principal has one task registry, shared by every executor doing its
work — Async spawn, background Bash, foreground calls detached on timeout,
subagent runs, and cron tool jobs — so the Async surface reads only the
caller's own registry, and the janitor purges finished tasks from each. A
foreground call is not a
background task: the dispatcher's routing entry for it appears in Async
list/status only once the call detaches on the foreground timeout.
Every background task has an owning principal: the principal of the call that
created it (Async spawn, background Bash, a call detached on timeout, subagent
runs, cron tool jobs). Async list/status/output/stop see only the caller's own
tasks. Work created without a principal is owned by the system principal and is
visible to no principal; the `ExecuteTool` IPC path refuses calls whose session
key resolves to no loaded principal (ADR-061 D2).
These lifetimes do not introduce a new authorization boundary; the principal
remains the trust boundary. See ADR-069.

## Parameter conventions

Required below means required for the documented action. Conditional requirements
are stated under each tool and encoded in its schema. Defaults may be applied
by runtime code rather than JSON Schema; schema defaults alone do not inject values.

## Files and shell

### Read

Read text with inline line numbers, binary content as base64, or images, audio, and video as media the model views directly.

[Implementation](../../peko-rs/core/src/tools/builtin/fs/read.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `file_path` | string | yes | — |
| `offset` | integer | no | ≥ 1 |
| `limit` | integer | no | ≥ 1 |
| `encoding` | utf8 \| base64 | no | — |

A folder is refused with a hint to list it with `Glob`. Offset is 1-based. Limit defaults to 2000 lines, and returned text is capped at 256 KiB (cut at a line boundary; a single longer line is cut and marked). A bounded read returns `truncated: true` and `next_offset`. Encoding defaults to utf8; binary content is detected automatically, and binary files over 256 KiB are refused. There is no PDF pages selector.

Media (ADR-088 D6): an image (PNG, JPEG, GIF, WebP), audio (WAV, MP3, FLAC, M4A, OGG), or video (MP4, MOV, AVI, WMV, WebM) file returns `content` (a line describing it), `mime`, `size_bytes`, and `media: {kind, mime, path}`. The engine attaches the file to the tool result as an image / audio / video block when the run's model spec takes that kind and the file is within the block limit (5 MB images, 20 MB audio/video); otherwise the model reads a note that it cannot take it. Only `Read` may point the engine at a file this way. `encoding: "base64"` returns a media file's bytes as base64 text instead.

### Write

Create, overwrite, or append files; creates parent directories.

[Implementation](../../peko-rs/core/src/tools/builtin/fs/write.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `file_path` | string | yes | — |
| `content` | string | yes | — |
| `mode` | overwrite \| create_new \| append | no | default "overwrite" |
| `encoding` | utf8 \| base64 | no | default "utf8" |

Mode defaults to overwrite; use create_new to refuse an existing file. Parent directories are created automatically.

### Edit

Replace exact text; requires a unique match unless replace_all is true.

[Implementation](../../peko-rs/core/src/tools/builtin/fs/edit.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `file_path` | string | yes | — |
| `old_string` | string | yes | — |
| `new_string` | string | yes | — |
| `replace_all` | boolean | no | default false |

old_string must match exactly and occur once unless replace_all is true.

### Glob

Find files and optionally directories by glob pattern.

[Implementation](../../peko-rs/core/src/tools/builtin/fs/glob.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `pattern` | string | yes | — |
| `path` | string | no | — |
| `include_hidden` | boolean | no | default false |
| `include_ignored` | boolean | no | default false |
| `include_dirs` | boolean | no | default false |
| `limit` | integer | no | default 1000, ≥ 1, ≤ 10000 |

path defaults to the calling workspace. An explicit path overrides workspace injection. A pattern with `**` searches from the directory before its first `**` and matches the remainder against each path below it (`**/tests/*.py`); without `**`, the last segment matches names in one directory. Results are in path order with paths relative to the base directory. Like ripgrep, hidden names and files excluded by `.gitignore` / `.ignore` (and git's exclude/global lists) are skipped — in or out of a git repository — unless include_hidden / include_ignored.

A visitor's search (ADR-113) from a folder above what it may reach, such as the workspace itself, is not refused: the dispatcher limits the walk to the reachable paths under it (the conversation's own folder and the `read` / `write` paths of its `ACCESS.toml` section) and the folders leading to them. Nothing else is listed or opened, other conversations' folders included. A search from a folder with nothing reachable in it is refused as before. The limit travels as `__visitor_scope`, which only the dispatcher sets; a model's own value is dropped. Grep searches the same way.

### Grep

Regex search with context, filename-only, or count output.

[Implementation](../../peko-rs/core/src/tools/builtin/fs/grep.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `pattern` | string | yes | — |
| `path` | string | no | — |
| `include` | string | no | — |
| `limit` | integer | no | default 100, ≥ 1, ≤ 1000 |
| `include_content` | boolean | no | default true |
| `context_before` | integer | no | default 0, ≥ 0, ≤ 10 |
| `context_after` | integer | no | default 0, ≥ 0, ≤ 10 |
| `context` | integer | no | ≥ 0, ≤ 10 |
| `case_insensitive` | boolean | no | default false |
| `include_hidden` | boolean | no | default false |
| `include_ignored` | boolean | no | default false |
| `output_mode` | content \| files_with_matches \| count | no | default "content" |

context overrides context_before/context_after. Content/context controls apply only to content mode. Output is a plain-text string with accompanying JSON metadata; paths inside the workspace are relative to it. Files are walked in path order; hidden names and files excluded by `.gitignore` / `.ignore` are skipped unless include_hidden / include_ignored.

### Bash

Execute shell commands, synchronously or in the background.

**Owner runs only (ADR-078 D6, amended 2026-11-02).** The shell runs as the OS
user, so no path rule can hold it: a run acting for anyone else would read every
conversation the runtime keeps. The floor refuses it to trusted and untrusted
peers alike and leaves it out of their wire catalog.

[Implementation](../../peko-rs/core/src/tools/builtin/bash.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `command` | string | yes | — |
| `description` | string | no | — |
| `cwd` | string | no | — |
| `run_in_background` | boolean | no | default false |
| `timeout` | integer | no | ≥ 1 |
| `max_output_bytes` | integer | no | ≥ 1 |
| `secrets` | string[] | no | names of the peko's secrets |

`secrets` (ADR-116 D5) sets each named peko secret as that environment variable for this command only; a name with no secret fails the call and points to `Secret`. Every value of 8 or more characters is replaced by `[secret NAME]` in `stdout`, `stderr` and a background task's live output. A background spawn reads them in its task body.

timeout is milliseconds. max_output_bytes defaults to 100000 per stream and is ignored in background mode, where each stream keeps its last 100000 bytes (a dropped start is marked `(truncated)...`). Background execution returns an async receipt; it runs through the caller principal's Async runtime exactly like `Async action=spawn tool=Bash`, and `timeout` bounds the task.

## Agents, roles, and skills

### Agent

Run work in a new, resumed, compacted, or branched session.

[Implementation](../../peko-rs/core/src/tools/builtin/messaging/agent.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `action` | new \| resume \| compact \| branch | no | default "new" |
| `prompt` | string | yes | nonempty |
| `role` | string | no | default "worker" |
| `path` | string | yes | nonempty |
| `model` | string | no | — |
| `source` | string | no | — |
| `overwrite` | boolean | no | — |
| `page_limit` | integer | no | ≥ 0, ≤ 10000 |
| `title` | string | no | new only; 1–80 characters |
| `as_owner` | boolean | no | new only (ADR-100); from the owner's turn in a shared conversation, or from a turn another peko started in a group (ADR-120 D5) |
| `item` | string | no | with `as_owner`, in a group: the board item the task serves (ADR-100 D6) |

A run that called `Ask` (ADR-114 D4) returns `needs_input: true` and its `questions` beside its `output`, on the foreground result and on the background completion; continue it with `new` on the same `path` and the answers as `prompt`. All four actions require nonempty prompt and path. role names the template the run follows (a RoleCatalog id); omitted, it is `worker`, the seeded role for a delegated task. new is create-or-resume; new/branch accept a single relative slug or an absolute session address, while resume/compact use absolute addresses such as sess:/worker. source and overwrite are branch-only; source defaults to the calling session and overwrite defaults false. page_limit is new/branch-only, 0–10000 (0 clears the cap), omission preserves an existing cap (new sessions are unlimited); exceeding it permanently deletes the oldest closed pages. model is new/resume/branch-only and refused for compact. title is new-only and names the session the call creates, in a few words, for Session `list` and `find`; it is stored as one line of at most 80 characters, and a session that already exists keeps the title it has. A session created without one is named after the first line of its prompt (at most 60 characters); a branch with no title of its own is named the same way. All live runs share the principal’s concurrency pool (default 20); delegation depth is unrestricted.

`as_owner: true` (ADR-100) hands the task to the owner. Only a run the owner's own post started in a conversation others share may use it (its stamp carries `owner_request`), and only with action new, a bare-slug `path`, and a `prompt` of at most 4,000 characters: the whole brief, since the run sees nothing else. The dispatcher refuses it to every other run. It never goes to the policy reviewer: it waits as an `owner_task` approval, which the owner approves once (never *always*). On approval the brief runs as the owner (owner trust, internal compartment, origin `approval`) at `sess:/<slug>` at the top of the owner's tree, outside the conversation's reach, and its report wakes the session that asked. In a group it first checks the board item it serves (`item`, else the one item the peko is working on): when someone else has done or is doing that work, or a twin waiting on its own approval was added first, it does not run, and the owner's reply, the peko's inbox and the item's note say why; otherwise the item turns `doing` (ADR-100 D6).

### RoleCatalog

Discover role templates available in the principal.

[Implementation](../../peko-rs/core/src/tools/builtin/role_catalog.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| — | — | — | No parameters |

Returns {total, roles}. Each role exposes id, name, and description; use its id as Agent.role.

### Skill

Load a SKILL.md body, resolve dynamic shell context, and substitute arguments.

[Implementation](../../peko-rs/core/src/tools/builtin/skill/tool.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `name` | string | yes | — |
| `args` | string[] | no | — |

args is string[]. The body supports $ARGUMENTS, positional $0/$1/etc., named frontmatter arguments, and dynamic shell context. Presence in the principal workspace determines availability; there is no principal capability allowlist.

Skills that ship with the runtime (`peko-rs/core/src/resources/skills/`) sit beneath the workspace's: a workspace skill of the same name wins. A bundled skill returns `dir: null`, since it has no files beside it. The catalog lists one as `(ships with the runtime)`, and an owner-only one (`coding-agents`, `triage`) only for runs acting for the owner.

## Session storage

### Session

Inspect and manage persisted sessions and compaction pages.

[Implementation](../../peko-rs/core/src/tools/builtin/session/tool.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `action` | status \| list \| history \| find \| copy \| move \| remove \| list_pages \| read_page \| search_pages | yes | — |
| `path` | string | no | — |
| `target` | string | no | — |
| `query` | string | no | — |
| `page` | integer | no | ≥ 1 |
| `offset` | integer | no | default 0, ≥ 0 |
| `max_results` | integer | no | default 20, ≥ 0 |
| `title` | string | no | — |
| `page_limit` | integer | no | ≥ 0, ≤ 10000 |
| `recursive` | boolean | no | default false |
| `peer` | string | no | — |
| `agent_name` | string | no | — |
| `limit` | integer | no | ≥ 0; per-action defaults |
| `active_minutes` | integer | no | ≥ 0; ≤ floor(u64::MAX / 60000) |
| `include_tools` | boolean | no | default true |
| `timezone` | string | no | — |

action is required. Both Agent and Session reject unknown fields, explicit nulls, and fields outside the chosen action on dispatcher and direct calls. Session result limits, offsets, and page numbers must fit usize; active_minutes is bounded so conversion to milliseconds cannot overflow. See the action table below for conditional requirements and actual defaults. Absolute addresses use sess:/a/b; sess:/ identifies the trunk for reads. Omitted read paths select the calling session. Mutation ownership/run guards remain in the session runtime.

| Action | Purpose | Required fields besides action | Optional fields / runtime defaults |
|---|---|---|---|
| status | Metadata and usage; `this_run` says whom the calling run acts for (`acting_for`, `name`, `trust`, `conversation`) and what it reaches (`everything` for the owner; for a visitor, its files, own folder, tools, MCP servers and whether requests go to the owner) | — | path, timezone |
| list | List/filter sessions | — | path, peer, agent_name, active_minutes, limit=50 |
| history | Messages | — | path, limit=100, include_tools=true |
| find | Search session names, then transcripts | query | path, peer, limit=50 |
| copy | Copy to a new address | path, target | title |
| move | Reparent/rename/update retention | path and at least one of target/title/page_limit | page_limit=0 means unlimited; maximum 10000 |
| remove | Delete a session | path | recursive=false |
| list_pages | Compaction page catalog | — | path |
| read_page | Render one page | page (1-based) | path, offset=0, limit=200 |
| search_pages | Search all pages | query | path, max_results=20 |

find is a case-insensitive substring search. Sessions whose title or own slug contains the query come first, most recently active first, as hits with `matched: "name"`, `role: "session"`, and the title (or slug) as `snippet`; messages that contain it follow with `matched: "text"`. Every hit carries the session's `path` and, when it has one, its `title`; `limit` counts both kinds together. An ancestor's slug does not name the sessions under it.

A move with target plus title/page_limit applies subsequent updates at the destination and returns that effective path. Invalid arguments reject before any storage mutation.

Session manages storage; Agent runs work. A move/remove refuses the trunk,
the current session, and actively running targets. Destructive operations remain
subject to runtime ownership guards. Page retention prunes old pages permanently.

## Channels

### Contacts

`list` returns the caller's own actor, current participants of its channels, and
principals explicitly learned by that caller. Contacts are per-principal; neither
the runtime actor registry nor all hosted principals are enumerated. Each actor
includes its local ID, optional Hub ID, kind, name, registration state, canonical local address, and
mention token; a person signed in to PekoHub also has a `handle` (ADR-124), unique
where names repeat. `resolve` accepts a scoped local or Hub reference (`peko:` and
`principal:` name the same actor), a name (`ada` / `@ada`) or a person's handle
(`@maya`) of an actor the run sees — one match is the answer, several are an error listing the candidates, an
owner run also finds the principals hosted here — or a peko by its owner's PekoHub
handle (`@frank/ada`): a peko the run sees whose directory `ownerHandle` matches,
else (owner runs) PekoHub by handle, which answers for public pekos, the caller's
owner's own, and pekos of people in a registered group with the caller's owner
(ADR-098 D1, PekoHub ADR-011). Registered remote
principals are learned through the Hub directory, anonymously for local-only callers
or authenticated as registered callers. An unfamiliar locally hosted principal can
be learned by its exact local ID. Learned contacts live in the principal's own
contact book (`local/contacts.json`, ADR-081 D5): they persist across restarts and
travel in `.peko` snapshots. A learned contact this runtime cannot route to the same
immutable actor (a local actor of another runtime, or a Hub contact not yet
re-resolved here) is listed with `reachable: false` and cannot be addressed until
resolved again. Another caller must perform its own lookup, even if the runtime
already knows the actor.
User and visitor resolution is limited to current participants of the caller's channels.
Malformed directory identities are rejected before storage writes. Unknown arguments,
missing references, and references supplied to `list` are rejected.

Reach follows the run's acting-for stamp (ADR-078 D4). An owner run sees every
channel and every learned contact. Any other run sees its own actor and the
participants of its own conversation only; `resolve` searches the same scope, and
learning a new contact (local or Hub) is refused. An unstamped call sees no
conversation. Each listed channel carries its `kind` (`dm` / `group`), the
caller's session `binding`, and its `wake` policy (ADR-082).

### ChannelSend

Send with `to` (the old `channel` input is a compatibility alias; do not supply both).

- `principal:local:<id> / principal:hub:<UUID>`, `principal:<local-name>`, `principal:<owner>/<name>` resolves
  an actor and posts on its pair DM. Local messaging needs no Hub setup.
- `user:local:<id>` / `user:hub:<UUID>` or `visitor:local:<id>` / `visitor:hub:<UUID>` delivers a labeled note to an existing conversation.
- `dm_<hash>`, `chan_<id>`, or `group:<slug>` posts directly to a conversation;
  `parent` threads a reply using an event's line `id` or its stable `msg_id`
  (resolved to the line). Actor targets do not accept
  `parent`; use the returned conversation ID. `label` applies only to human notes.
- `options` (2–4 strings, one line and at most 60 characters each, no
  duplicates) adds suggested replies to the post (ADR-093): readers can send one
  with a tap, as their own message. Refused on notes to users; broken options
  refuse the whole send.

Principal sends return canonical `to`, real `channel` (also `dm_channel` for old
clients), `id`, and `delivery` (`delivered` or durably `queued`). Same-runtime pair
DMs store each message once and bind independently to both principals' sessions.
Outside groups, principal posts never start runs (ADR-072): read/watch the returned
channel for an answer. In a group (ADR-120), a principal's post wakes another peko
when it mentions it, adds a board item `for` it, or replies to one of its posts,
while the group holds at most 24 peko posts since a person last posted and within
60 such wakes per peko per group an hour; a post that reaches a turn already
running is steered into it at the same standing, else noted to it and given its
own turn after. Human group posts wake only the mentioned principal: `@principal:local:<id> / principal:hub:<UUID>`
is canonical; `@name` works only with a complete, unambiguous principal roster.

### ChannelRead

Accepts real conversation IDs including `dm_<hash>`. Each result includes current
`participants` (canonical address, known name, mention token), the channel's
`kind`, and the caller's session `binding` and `wake` policy (ADR-082). New and legacy histories remain readable; raw legacy local
authors are normalized when their principal identity is known. Watches and unread
summaries use the same human/group mention rules as turn admission.


Read or search channel events with backward/forward pagination, or wait for the next post.

[Implementation](../../peko-rs/core/src/tools/builtin/channel/channel_read.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `channel` | string | yes | — |
| `limit` | integer | no | ≥ 1 |
| `query` | string | no | — |
| `author` | string | no | — |
| `before` | string | no | — |
| `since` | string | no | — |
| `watch` | boolean | no | false |
| `reply_to` | string | no | — |

Accepts chan_<id> or group:<slug>; caller membership is required. limit defaults to 50, capped at 1000 for reads and 200 for searches. Nonempty query or author activates search mode. since overrides before for reads and is ignored in search mode. Returns events oldest-to-newest with has_more and next_cursor.

`watch: true` waits until someone else posts after `since` (default: the newest event at call time) and returns those posts in the same shape; `author`, `query`, and `reply_to` (an event id the post must be threaded onto) narrow which posts count. The caller's own posts never end a watch, nor do posts that start turns by themselves (a user's root post on a DM channel; in a group, a user's root post the caller's wake policy wakes on — an `@<name>` mention by default, every one under `human`, none under `off` — and, unless the policy is `off`, another peko's mention of the caller or reply to one of its posts, ADR-120) — they already reach a session as a turn. There is no time limit: past the 10-minute foreground cap the call moves to the background and its result arrives as a completion. Reads and watches advance the calling session's read mark, which the session context's unread summary uses, and clear the returned posts from the session's waiting list (other principals' posts the summary keeps listing until answered — ADR-072 D4a).

### Channel

A principal manages its groups (ADR-082, ADR-092) and the role it runs in each conversation (ADR-087). Requires `action`.

[Implementation](../../peko-rs/core/src/tools/builtin/channel/channel_tool.rs)

| Action | Parameters | Effect |
|---|---|---|
| `create` | `name`; optional `slug`, `members` | Creates a group (`group:<slug>`, else `chan_*`) with the caller as creator and first member, provisions its group session and binding (`wake: mention`), adds the owner when the run acts for them (a group made for the owner has them in it), and invites each member as `invite` does. Returns `channel` and the `invited` addresses. |
| `invite` | `channel`, `actor` | Adds one actor to a group the caller belongs to: the caller's owner, or a principal in its contact book (`Contacts resolve` first). Any other human is refused — the owner adds them with `peko channel invite`. Only the group's owner or an admin adds members, and only at the group's home runtime (ADR-092 D1). Remote members receive the signed roster snapshot. |
| `leave` | `channel` | Removes the caller from a group and deletes its binding; the session stays as history. |
| `wake` | `channel`, `policy` | Sets the caller's policy on a group: `human` (every human root post starts a turn), `mention` (only `@<name>`, the default), or `off`. A person's reply never starts a turn; another peko's post does under `human` and `mention` when it mentions the caller, adds an item for it, or replies to one of its posts (ADR-120). |
| `role` | `channel`; optional `role` | Sets the role (a `RoleCatalog` id) the caller's turns run in a DM or group; omitting `role` returns it to the default conversation role (`routing.peer_agent`, else `conversation`). Refuses a role the principal does not have. |
| `admin` | `channel`, `actor`, `admin` | The group's owner makes a member an admin (who may then add members), or stops it. At the home runtime only (ADR-092 D1). |
| `board` | `channel`, `op`; per op `title` / `for` (add), `item` (claim, update, release, drop), `state` / `note` (update), `check_at` (claim, update) | Changes the group's board (ADR-101) by posting the change in the group; the runtime writes the post's readable line. `add` makes an item (`for` asks a member, by address or name, and wakes it like a mention); `claim` takes one for the caller (refused while someone else holds it); `update` moves one the caller holds or added (the group's owner or an admin may too) to `doing` / `blocked` / `done` with a `note`; `release` lets go; `drop` removes one the caller added. `check_at` (10 minutes to 30 days ahead) brings an item the caller holds back to it once; `blocked` without one gets 24 hours. Returns the item with its short `id` and a `note`; an `add` or `claim` also returns `similar`, the open items whose titles look like the same work (they share at least half the longer title's words). Groups only; open to any run that may post there. |
| `register` | `channel`; optional `visibility` | The group's owner or an admin gives it an ID on PekoHub, or changes who may find it: `invite_only` (the default: people join only with an invite link) or `public` (anyone with the ID may). At the home runtime only; the owner must be a peko registered on PekoHub. Returns `id`, `page`, `visibility` (ADR-092 D5). |
| `link` | `channel`; optional `days`, `revoke` | The group's owner or an admin makes an invite link to a registered group, good for `days` (1–30, default 7): `link` and `expires`. With `revoke: true`, stops every link instead and returns how many were live. At the home runtime only (ADR-092 D5). |
| `join` | `link` | The caller itself (not its owner) joins a group by its invite link, the link's token, or a public group's ID: PekoHub says where it lives, the caller's runtime keeps a request and asks the home, and the home's invitation completes it — taken even by a private peko without a `Chat` grant. Returns `channel`, `name`, and `status` (`joined` or `requested`). The peko must be registered on PekoHub (ADR-098 D2). |
| `info` | `channel` | The channel's `kind`, `name`, `creator`, `participants`, and `you` (`member`, `wake`, `role`, and — for owner runs — `session`). For a group also `home` (its home runtime), `owner`, `admins`, `board` (items not done, and those done in the last day), once registered `hubId` and `visibility`, and `you.owner` / `you.admin`. |

DMs are opened by `ChannelSend` and keep their members and wake policy, and have
no board: only `info` and `role` accept one. Authority follows the run's stamp (ADR-082 D6,
ADR-087 D6): an owner run may use every action on its channels; any other run
reaches only its own conversation's channel. The floor refuses `create`,
`invite`, `admin`, `register`, `link`, `join`, and `role` to untrusted runs, and a trusted run's call to one of
them skips the policy reviewer and waits for the owner's approval (`peko approve`
or the desktop's Approve).
Calls run alone (not parallel).

### Attach

Send files to the person the run is talking to (ADR-088 D7) — the counterpart of a user's attachments.

[Implementation](../../peko-rs/core/src/tools/builtin/channel/attach.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `paths` | string[] | yes | 1–8 items |
| `text` | string | no | — |

Each path (absolute, `~/…`, or relative to the workspace) passes the same floor as `Read` (ADR-113: an untrusted run reaches only its conversation's own folder and the paths `ACCESS.toml` lists), then the file is copied to `<workspace>/attachments/<YYYY-MM-DD>/<8 hex>-<name>` under the inbound limits (at most 8 files, 20 MB each, regular files only; nothing is copied unless every file passes). In a conversation with anyone on the Hub or another runtime (ADR-094) the limits are 10 MB a file and 25 MB a call, and each file also goes to the channel's file store under its SHA-256, which the post carries; the other runtimes pull the bytes through PekoHub. The copies are posted on the run's conversation channel — the stamp's compartment — as one message from the principal with `text` as its caption, so `principal_log` and the desktop show them beside the reply. Returns `{success, channel, id, attachments: [{name, path, mime, size}]}` with absolute paths.

A non-owner run's call goes to the policy reviewer first, as `Read` does. Refused outside a conversation (cron, internal work). Calls run alone (not parallel).

An `.html` attachment is a **page** (ADR-118): Peko Desktop shows it inline, served from its own `pekopage:` protocol with a policy of its own (`sandbox allow-scripts`, no network, nothing of the app), at most 2 MB; larger pages and other clients show a file. The description tells the peko to make pages self-contained.

### Suggest

Offer suggested replies with the run's reply (ADR-093 D3) — a few short answers the person can send with one tap.

[Implementation](../../peko-rs/core/src/tools/builtin/channel/suggest.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `replies` | string[] | yes | at most 4; `[]` clears |

A conversation's reply is the run's final text, posted after the run, so `Suggest` posts nothing: it stages 2–4 replies (one line, at most 60 characters each, no duplicates, trimmed) for the run's conversation ([reply_options.rs](../../peko-rs/core/src/principal/reply_options.rs)), and the reply post carries them as `options`. A later call replaces the earlier one. The staged replies are dropped when the reply is empty or withheld by the policy reviewer, and when anyone else posted in the conversation after the call, so a run that never replied cannot hand them to a later reply. Returns `{success, channel, replies}`.

Refused outside a conversation (cron, internal work) and for a run that is not the conversation's bound session (an `Agent` worker shares the compartment but reports back). Exempt from the policy reviewer: the replies are text the person may choose to send, and a tap carries no authority — no message decides an approval (ADR-078).

### Ask

Ask questions that wait for an answer (ADR-114 D2, D4) — in a conversation, a card the person answers together; in a delegated task, questions back to the run that delegated it.

[Implementation](../../peko-rs/core/src/tools/builtin/channel/ask.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `questions` | {text, options?, multi?}[] | yes | 1–4 items |
| `for` | string | no | groups: a `user:` or `visitor:` member |

Each question is at most 300 characters, with no options (free text) or 2–4 options checked like `Suggest` replies; `multi` allows several choices and needs options. Like `Suggest`, `Ask` posts nothing. The run's caller decides where the questions go:

- **The conversation's bound session:** they are staged for the run's reply ([reply_options.rs](../../peko-rs/core/src/principal/reply_options.rs)) and the reply post carries them as `ask`, replacing any staged suggested replies. They are dropped with the reply the same way `Suggest` replies are. `for` names who should answer; it must be a person on the conversation's roster and is stored as their canonical address. The person's answer is a root post carrying `answers` (ADR-114 D1, D5); in a group, a plain root post by whom the open card is for (anyone, without `for`) also wakes the peko under `mention` and closes the card.
- **A delegated run** (an `Agent` spawn that is not a conversation turn): they are staged for its result ([delegated_asks.rs](../../peko-rs/core/src/agents/delegated_asks.rs)). When the run ends, the `Agent` result — foreground, or the background completion after the 10-minute receipt — carries `needs_input: true` and `questions`. `for` is refused.
- **Anything else** (the trunk, cron work): refused.

A later call replaces the earlier one. A conversation turn that asked and then ends quietly (`NO_REPLY` or nothing) replies with the text it wrote beside the call; with none, the card is posted with text listing the questions. Returns `{success, to: "conversation" | "delegator", questions, note}`, plus `channel` in a conversation.

Exempt from the policy reviewer: the questions ride a reply that is already reviewed, and an answer carries no authority. Visitors reach it when `ACCESS.toml` lists it (the seeded file does). Calls run alone (not parallel).

### Secret

Ask the owner for a secret (ADR-116) — a token, password or API key — without it passing through the conversation.

**Owner runs only.** From anyone else's run it would be a way to phish the owner; the floor refuses it and leaves it out of their wire catalog.

[Implementation](../../peko-rs/core/src/tools/builtin/secret.rs) · [requests](../../peko-rs/core/src/principal/secret_requests.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `action` | `request` \| `list` | yes | — |
| `name` | string | request | an environment variable name; with `server`, an MCP credential slot |
| `why` | string | request | 1–300 characters, shown to the owner |
| `server` | string | no | an MCP server in the workspace's `mcp/` |

`request` stores a `sec_…` record in `<local>/secret_requests/`, announces it in the owner's conversation (`🔑 Secret requested`), and returns `{success, id, secret, status: "waiting", expires_at}`. Asking again for a target already waiting returns that request; at most 5 wait at once, and each lapses after 24 hours. The owner answers on the desktop card or with `peko secret set` / `decline` (IPC `secret_provide` / `secret_decline`, owner only, local callers only). The value goes to the vault — namespace `secret:principal:<id>` named by the variable, or the server's `mcp:principal:<id>:<server>` slot — and the session that asked gets a completion saying it was saved or declined, never the value. `list` returns `{success, secrets: [name], waiting: [{id, secret, why, expires_at}]}`. Calls run alone (not parallel).

## Web

### Fetch

Read one public web page or text file by URL (ADR-103). A plain GET, not a browser.

[Implementation](../../peko-rs/core/src/tools/builtin/fetch.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `url` | string | yes | `http` or `https`, no user or password |
| `raw` | boolean | no | false; true returns an HTML page's source |
| `max_chars` | integer | no | 40000; 1000–200000 |

Returns `{url, status, content_type, text, total_chars, truncated}`, plus `final_url` after a redirect, `title` for an HTML page, and a `note` when the text was cut or the page looks drawn by JavaScript. HTML is rendered to plain text with its links kept; plain text, JSON, XML and similar types come back unchanged; a charset declared in the header or the page is decoded. At most 2 MB is read, the call takes at most 30 seconds, and up to 5 redirects are followed.

**Limits the model is told.** The description says that `Fetch` reads only part of the internet: not pages that need JavaScript or a sign-in, not sites that block automated requests, not PDFs, images or other files, and it does not search. Every failure names its reason (bot check, refusal, rate limit, not text, timeout) and tells the model not to retry the URL. It identifies itself as `Peko/<version>` and does not imitate a browser.

**Addresses.** Every hop, redirects included, must resolve to public addresses only, and the request is pinned to the address that was checked. Loopback, private, link-local, carrier-NAT and reserved ranges are refused for every run, owner runs included (an owner run has `Bash` for local services).

**Trust.** Every run gets it. A non-owner run's call goes to the policy reviewer first, as `Read` does. Returned text has runtime tags defused and is to be treated as data. Calls may run in parallel.

### Browser

The peko's own browser (ADR-119): a headless Chromium with a profile of the peko's own, for pages `Fetch` cannot read. Its owner can step into the same browser from any of their devices.

**Owner runs only.** A signed-in profile is the owner's authority, as `Bash` is; the floor refuses it to every other run and leaves it out of their wire catalog. `ACCESS.toml` cannot list it.

[Implementation](../../peko-rs/core/src/tools/builtin/browser.rs) · [browser](../../peko-rs/core/src/browser/mod.rs) · [engine](../../peko-rs/core/src/browser/engine.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `action` | `open` \| `read` \| `click` \| `type` \| `press` \| `screenshot` \| `tabs` \| `close` \| `handoff` | yes | — |
| `url` | string | open | `http` or `https` |
| `tab` | string | all but `open`, `tabs` | a tab id (`t1`) from `open` or `tabs`; on `open`, reuses that tab |
| `ref` | integer | click, type | a control's number from the last `read` of that page |
| `text` | string | type (or `secret`) | replaces what the field holds; for a list, the option to choose |
| `secret` | string | type (or `text`) | the name of a peko secret (ADR-116) to put in the field |
| `submit` | boolean | no | false; true presses Enter after typing |
| `key` | string | press | Enter, Tab, Escape, Backspace, Delete, Space, the arrows, PageUp, PageDown, Home, End |
| `offset` | integer | no | 0; `read` continues a long page from this character |
| `max_chars` | integer | no | 30000; 1000–100000 |
| `why` | string | handoff | 1–300 characters, shown to the owner |

`open` and `read` return `{success, tab, url, title, text}` and `more` when the page continues. The text is the page as it is drawn (scripts have run), with each visible control on a line of its own, `[n] kind "label"`, where `n` is the reference `click` and `type` take; a password field says only whether it is filled. References live until the page navigates. `click`, `type` and `press` return `{success, tab, url, title}` after the action, and `dialog` when the page showed an alert (it is accepted). `screenshot` returns the page as an image for models that take images. `tabs` lists `{tab, url, title}`; at most 12 tabs are open.

**The browser.** One per peko, started on first use from an installed Chrome, Edge, Brave or Chromium (or `PEKO_BROWSER_PATH`), else from the Chrome for Testing build `peko browser install` downloads into `<runtime>/browser/`. Its profile is `<local>/browser/profile`, outside snapshots. It is driven over a pipe it inherits (a loopback port on Windows), and stops after 15 minutes without a call or a viewer, unless a hand-off waits. No downloads or uploads.

**Secrets.** `type` with `secret` reads the value from the vault; it is in no parameter, and every later result of the tool has that value replaced by `[secret:NAME]`.

**Hand-off (ADR-119 D3, D4).** `handoff` records what the peko needs, tells the owner in their conversation (`🌐 Browser needs you`), and returns `{success, status: "waiting"}`. The owner takes control from a viewer: Peko Desktop on this computer (IPC `browser`), a paired phone (device verb `browser`), or Peko Desktop on another of their computers (IPC `channel_peko_browser`, tunnel `tunnel_peko_browser_request`). While a viewer holds control every action of this tool fails with "your owner is using the browser" and changes nothing; nothing the owner sees or types is recorded or reaches the model. When they release it (or their device goes away for 90 seconds) the session that asked gets a completion and reads the page again. A hand-off lapses after 24 hours.

**Trust.** Page text and titles have runtime tags defused and are to be treated as data. Calls run alone (not parallel).

## The owner's computer

### Computer

The owner's own computer (ADR-123): see the screen and click and type in its apps, the same way on macOS and Windows, for work no other tool can do (a desktop app with no web page, API or script interface; testing an app the owner builds).

**Owner runs only.** It acts in the owner's signed-in apps, with the owner's reach, as `Bash` does; the floor refuses it to every other run and leaves it out of their wire catalog. `ACCESS.toml` cannot list it. Cron, keepalive and conversation runs that act for the owner may use it, so a peko can test an app while its owner is away. `[computer] enabled = false` in `runtime.toml` takes it out of every peko's catalog (read on each run).

[Implementation](../../peko-rs/core/src/tools/builtin/computer.rs) · [desk](../../peko-rs/core/src/computer/mod.rs) · [macOS](../../peko-rs/core/src/computer/macos.rs) · [Windows](../../peko-rs/core/src/computer/windows.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `action` | `screens` \| `windows` \| `focus` \| `screenshot` \| `click` \| `drag` \| `move` \| `scroll` \| `type` \| `key` | yes | — |
| `app` | string | no | windows: only windows of this app name (substring) or process id |
| `screen` | integer | no | screenshot: a screen id from `screens`; default the main screen |
| `window` | integer | focus, click, drag, scroll, type, key; screenshot (or `screen`) | a window id from `windows` |
| `region` | [x, y, width, height] | no | screenshot: part of an earlier `frame`, in its pixels |
| `after_ms` | integer | no | screenshot: wait first; at most 10000 |
| `frame` | integer | click, drag, move, scroll; screenshot with `region` | the screenshot the coordinates are in |
| `x`, `y` | number | click, move, scroll | pixels in the frame's picture |
| `from`, `to` | [x, y] | drag | pixels in the frame's picture |
| `button` | `left` \| `right` \| `middle` | no | left |
| `count` | integer | no | 1; 2 double-clicks (at most 3) |
| `dx`, `dy` | integer | scroll (one of them) | lines; `dy` > 0 down, `dx` > 0 right; ±100 |
| `text` | string | type (or `secret`) | at most 5000 characters; newlines press Enter, tabs Tab |
| `secret` | string | type (or `text`) | the name of a peko secret (ADR-116) to type |
| `keys` | string | key | a key or chord: `enter`, `tab`, `escape`, `backspace`, `delete`, `space`, the arrows, `home`, `end`, `pageup`, `pagedown`, `insert`, `f1`…`f20`, a character; modifiers `cmd`, `ctrl`, `alt`, `shift`, `win` (Windows) joined by `+` |

`screens` returns `{screens: [{id, bounds: {x, y, width, height}, scale, main}]}`. `windows` returns `{windows: [{window, app, pid, title, bounds, minimized}]}`, front to back, minimized (macOS: off screen) last; Peko Desktop's indicator is never listed. `screenshot` returns `{frame, width, height, of, content, media}` with the picture as `media` (an image, for models that take them); every input action returns `{success, done, window: {window, app, pid, title}}`, the window read again just after the input (its title as the input left it; no `window` when the input closed it).

**Frames (D1).** Every screenshot is a numbered frame of a screen or a window; input names the frame and gives `x, y` in that picture's pixels, and the runtime maps them to the screen. A window frame is relative to the window and the window's bounds are read again before acting, so a window that moved still works; one that changed size, closed, or was minimized fails with `status: "stale"` ("take a new screenshot"). A frame of one window cannot steer input to another. Frames are kept for the run's last 8 screenshots. Pictures are JPEG, at most 1568 px on the long edge and 1.2 megapixels; a `region` shows part of a frame at up to full resolution.

**The focus guard (D3).** Before input, the window's app must be frontmost and the window its front window. If not, the runtime brings it forward once (macOS: Accessibility `AXFrontmost`/`AXRaise`, System Events as fallback; Windows: a synthetic Alt, then `SetForegroundWindow`), waits up to 500 ms, and checks again; still not in front, the action fails with `status: "not_in_front"` naming what is in front, and nothing is sent. A click whose point another app's window covers is refused the same way, and so is input at a point off every screen (the part of a window beyond the screen's edge, which its picture shows): the system would move it to the edge, onto something else. Long text is typed in pieces of 20 UTF-16 units, and the guard is checked between them.

**One hand on the screen (D2).** Hardware input from the owner (macOS: a listen-only event tap; Windows: low-level hooks; events the runtime posted are told apart) pauses the run that used the computer in the last 30 seconds: its input actions, and every other run's, fail with `status: "paused"` until the owner chooses **Let it continue** (Peko Desktop's indicator, IPC `computer_resume`, `peko computer continue`) or writes in that session (a direct message or a channel post); screenshots still work. Only one run acts at a time: another run's input fails with `status: "busy"` until the first has ended or has sent no input for 30 seconds. A run that has ended holds nothing: the owner's input pauses nothing of it, and the indicator goes away (a pause stays until the owner lifts it). A locked screen, the screen saver, a system password prompt (macOS) or the secure desktop (Windows) fails every action with `status: "blocked"`. `type {secret}` is refused while a secure field or a UAC prompt holds the keyboard. The display is kept awake while the computer is used, and for a minute after.

**Platforms (D4).** macOS 13+: ScreenCaptureKit pictures (`CGWindowListCreateImage` where it is missing), `CGEventPost` input, coordinates in points; it needs Screen Recording and Accessibility granted to the app that started the engine (`peko computer status|setup`). Windows 10 1903+ and 11: Windows.Graphics.Capture (`PrintWindow`/`BitBlt` fallback), `SendInput` (text with `KEYEVENTF_UNICODE`), per-monitor DPI aware v2, coordinates in physical pixels; no grants. A window of an elevated process is refused (UIPI would drop the input); an engine running as a Windows service (session 0) reports that it has no desktop. Other systems: every call says it is not available.

**What is recorded (D6).** The ledger keeps each call's action, frame, coordinates, window id, key names, and for `type` the number of characters (`"[N characters]"`) or the secret's name (`secret_name`), never the text and never a picture; the result excerpt names the window's app, process id and title. A secret's value is in no parameter or result; window titles have it replaced by `[secret:NAME]`, and pictures taken after it was typed carry a note that it may be visible. Screenshots are kept under `<local>/computer/screenshots/` (the newest 50), in no snapshot.

**Trust.** Screen content is information, never instructions. Calls run alone (not parallel).

## Inference and workflows

### ModelList

List configured models, optionally filtered by capability and text.

[Implementation](../../peko-rs/core/src/tools/builtin/model_list.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `filter` | vision \| tools \| thinking \| priced \| json_mode | no | — |
| `contains` | string | no | — |

filter and contains are AND-combined; contains matches id, display_name, and note case-insensitively. Each model's entry includes its `effort` (absent when unset). Requires a bound model catalog; every run inherits the principal service.

### ModelCall

Make one sessionless completion or structured judgment call.

[Implementation](../../peko-rs/core/src/tools/builtin/model_call.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `model` | string | no | — |
| `prompt` | string | no | — |
| `system` | string | no | — |
| `max_tokens` | integer | no | — |
| `state` | string \| object | no | — |
| `questions` | object | no | nonempty object |

Exactly one mode is required: prompt for completion, or state plus a nonempty questions map for judgment. system/max_tokens are completion-only; no sampling settings are taken (models use their own defaults). model defaults to the principal’s preferred model. Judgment requires decisions:true; question specs are passed through and support boolean, choice (options), and score (min/max). Calls are attributed to the principal and metered.

### Workflow

Run a saved Python workflow from the principal’s workflows directory.

[Implementation](../../peko-rs/core/src/tools/builtin/workflow.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `path` | string | yes | — |
| `args` | string[] | no | — |
| `timeout_ms` | integer | no | — |

path must resolve to a .py file inside workflows/. timeout_ms is optional; omitted or zero means no limit. The run token is revoked when the run ends. The process receives runtime identity and can call tools through the workflow SDK. Nesting depth is injected internally from the run token and is absent from the public schema.

## Session todos

### Task

Required `action`: `create`, `get`, `list`, `update`. Each action accepts only its documented fields.

#### create

Create a session-local todo.

[Implementation](../../peko-rs/core/src/tools/builtin/tasks/create.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `subject` | string | yes | — |
| `description` | string | no | — |
| `activeForm` | string | no | — |

Todos are stored in the calling session’s todos.jsonl sidecar.

#### get

Fetch one todo.

[Implementation](../../peko-rs/core/src/tools/builtin/tasks/get.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `taskId` | string | yes | — |

#### list

List session-local todos, optionally filtered by status.

[Implementation](../../peko-rs/core/src/tools/builtin/tasks/list.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `status_filter` | pending \| in_progress \| completed | no | — |

#### update

Change a todo’s status and/or owner.

[Implementation](../../peko-rs/core/src/tools/builtin/tasks/update.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `taskId` | string | yes | — |
| `status` | pending \| in_progress \| completed | no | — |
| `owner` | string | no | — |

At least one of status or owner is required, in addition to taskId.

## Durable plans

### Plan

Required `action`: `create`, `list`, `get`, `add_step`, `mark_step`, `record_evidence`, `close`. Each action accepts only its documented fields.

Open plans also reach every owner run without a call: the `## Open plans` tail section lists up to five unfinished plans (most recently active first) with their due check-ins first, then in-progress, ready, and blocked or failed steps, and re-injects when a plan changes. A step's `checkAt` (ADR-084) says when to come back to it: until then a ready or in-progress step is snoozed and shows `check at <time>`; after it the check-in is due. An attention-gated heartbeat (ADR-083) runs for unsnoozed ready or in-progress steps, and once for each check-in that came due since its last turn.

**Owner runs only (ADR-078).** Plans are the principal's internal work and
carry no compartment: every action reaches every plan the principal holds. The
trust floor (`security::floor::OWNER_ONLY_TOOLS`) therefore refuses `Plan` to
any run not acting for the owner — trusted and untrusted peers alike, including
their standing children, group `@mention` turns, cron jobs they created, and
subagents spawned from those runs — and leaves it out of their wire catalog. The
refusal is a `floor_denied` ledger outcome, decided before policy review. An
owner run in a mixed group is capped below owner and loses `Plan` too. Non-owner
runs track their conversation's work with the session-scoped `Task` tool.

#### create

Create a principal-owned durable plan with dependency nodes.

[Implementation](../../peko-rs/core/src/tools/builtin/plan/create.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `title` | string | yes | — |
| `nodes` | object[] | yes | at least 1 item |

nodes contains at least one object: {step, nodeId?, dependsOn?: string[], status?, checkAt?}. nodeId is auto-assigned when omitted; status defaults to pending. checkAt is an RFC 3339 time or a duration from now (90m, 2d). Plans belong to the principal and persist across sessions.

#### list

List all plans owned by the current principal.

[Implementation](../../peko-rs/core/src/tools/builtin/plan/list.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `action` | string | yes | See action heading |

#### get

Fetch a plan record.

[Implementation](../../peko-rs/core/src/tools/builtin/plan/get.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `planId` | string | yes | — |

#### add_step

Append a node to an open plan.

[Implementation](../../peko-rs/core/src/tools/builtin/plan/add_step.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `planId` | string | yes | — |
| `step` | string | yes | — |
| `nodeId` | string | no | — |
| `dependsOn` | string[] | no | — |
| `status` | pending \| in_progress \| completed \| blocked \| failed | no | — |
| `checkAt` | string | no | — |

nodeId is auto-assigned when omitted; status defaults to pending. checkAt is an RFC 3339 time or a duration from now (90m, 2d). Closed plans refuse new nodes.

#### mark_step

Change a plan node’s status.

[Implementation](../../peko-rs/core/src/tools/builtin/plan/mark_step.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `planId` | string | yes | — |
| `nodeId` | string | yes | — |
| `status` | pending \| in_progress \| completed \| blocked \| failed | yes | — |
| `reason` | string | no | — |
| `checkAt` | string | no | — |

reason applies to blocked/failed states. checkAt sets the step's check-in (RFC 3339 or a duration from now); an empty string clears it, and completing the step clears it. Statuses: pending, in_progress, completed, blocked, failed.

#### record_evidence

Attach an outcome summary and artifact references to a node.

[Implementation](../../peko-rs/core/src/tools/builtin/plan/record_evidence.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `planId` | string | yes | — |
| `nodeId` | string | yes | — |
| `output` | string | yes | — |
| `artifacts` | string[] | no | — |
| `decidedBy` | string | no | — |

artifacts is string[] of paths/references; decidedBy is optional attribution.

#### close

Close a plan with a reason.

[Implementation](../../peko-rs/core/src/tools/builtin/plan/close.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `planId` | string | yes | — |
| `reason` | string | yes | — |

Repeated closure returns AlreadyClosed.

## Background execution

### Async

Required `action`: `spawn`, `output`, `status`, `list`, `stop`. Each action accepts only its documented fields.

#### spawn

Start a tool in the background and return an async receipt.

[Implementation](../../peko-rs/core/src/tools/builtin/async_control/spawn.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `tool` | string | yes | — |
| `params` | object | yes | — |
| `label` | string | no | — |
| `wake_on_completion` | boolean | no | — |
| `timeout_secs` | integer \| null | no | ≥ 1 |

params is forwarded verbatim. wake_on_completion defaults true; completion enters the spawning session’s inbox and may start an idle-session follow-up. timeout_secs is an optional lifetime limit; null/omit means no limit.

#### output

Fetch output, optionally waiting for completion.

[Implementation](../../peko-rs/core/src/tools/builtin/async_control/output.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `task_id` | string | yes | — |
| `block` | boolean | no | default false |
| `timeout_ms` | integer | no | ≥ 0 |
| `tail_lines` | integer | no | default 0, ≥ 0 |

block defaults false. With block=true, timeout_ms (the wait, not a limit on the task) defaults to 300000. tail_lines=0 returns full output; positive values select the last N lines.

#### status

Inspect a background task’s state and metadata.

[Implementation](../../peko-rs/core/src/tools/builtin/async_control/status.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `task_id` | string | yes | — |

#### list

List background tasks in the bound async runtime.

[Implementation](../../peko-rs/core/src/tools/builtin/async_control/list.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `status_filter` | pending \| running \| completed \| failed \| cancelled \| timed_out | no | — |
| `tool_filter` | string | no | — |

Uses the principal-owned async runtime shared across runs. Statuses: pending, running, completed, failed, cancelled, timed_out.

#### stop

Cancel a background task.

[Implementation](../../peko-rs/core/src/tools/builtin/async_control/stop.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `task_id` | string | yes | — |

## Scheduling

### Cron

Required `action`: `create`, `list`, `delete`, `update`, `trigger`, `history`. Each action accepts only its documented fields.

#### create

Schedule an instruction-driven agent turn or a fixed tool invocation.

[Implementation](../../peko-rs/cron/src/tools/create.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `message` | string | no | nonempty |
| `model` | string | no | nonempty |
| `when` | string | no | enum: always, attention, quiet |
| `tool` | string | no | nonempty |
| `params` | object | no | — |
| `wake_on_completion` | boolean | no | — |
| `timeout_secs` | integer | no | — |
| `one_shot` | boolean | no | default false |
| `label` | string | no | — |
| `cron` | string | no | — |
| `at` | string | no | — |
| `delay` | string | no | — |
| `interval_ms` | integer | no | — |
| `timezone` | string | no | — |
| `idle_ms` | integer | no | — |

Requires exactly one nonempty message or tool, plus a schedule. message starts an agent turn in the creating session (trunk fallback); tool invokes fixed parameters and may itself use an LLM. model (message jobs only) names the configured model each fire's turn runs on instead of the principal's pinned one; the runtime refuses a model it cannot resolve. when (message jobs on a recurring schedule) is always (default), attention, or quiet: an attention job runs a due fire's turn only when something waits on the principal (channel posts its digest would show, ready or in-progress plan steps, another job failed since its last turn) or its last turn is an hour old; other fires are quiet ticks that call no model and leave last_status quiet (ADR-083). A quiet job (interval or cron schedule, not idle_ms) holds a due fire until nobody has talked to the principal for 4 hours, at most 12 hours past due; a held fire moves next_run_at and leaves last_status quiet (ADR-080 D9). params defaults to {}; wake_on_completion defaults false and timeout_secs is an optional per-run limit (omitted means no limit). delay is a positive relative duration (90s, 5m, 1h, 1d, or bare milliseconds) and cannot be combined with another schedule. Explicit fields resolve by precedence at > interval_ms > cron > idle_ms. timezone applies to cron and defaults to the machine's IANA zone (recorded on the job; UTC if it cannot be determined). idle_ms rounds down to whole minutes, with a one-minute minimum; the job fires once per quiet stretch — when nobody has talked to the principal for that long (cron turns, completion wakes, and other principals' posts are not activity) — and again only after new activity. Activity persists across daemon restarts. one_shot=true deletes after the first fire; at/delay jobs are always one-shot. Same-job runs do not overlap, and overdue interval slots are skipped. message jobs on interval_ms require at least 60000 ms.

#### list

List the calling principal’s scheduled jobs. Each entry reports its schedule in create's fields (`at`, `interval_ms`, `cron` + `timezone`, or `idle_ms`) and its `message` (with `model` and `when` when set) or `tool` + `params`.

[Implementation](../../peko-rs/cron/src/tools/list.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `action` | string | yes | See action heading |

#### delete

Delete a scheduled job.

[Implementation](../../peko-rs/cron/src/tools/delete.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `id` | string | no | — |
| `label` | string | no | — |

Supply exactly one of id or label.

#### update

Pause/resume a job, change completion wake behavior, change a message job's model, `when` or message, or change a recurring job's schedule, keeping its id.

[Implementation](../../peko-rs/cron/src/tools/update.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `id` | string | no | — |
| `label` | string | no | — |
| `enabled` | boolean | no | — |
| `wake_on_completion` | boolean | no | — |
| `model` | string | no | — |
| `when` | string | no | enum: always, attention, quiet |
| `message` | string | no | nonempty |
| `interval_ms` | integer | no | — |
| `cron` | string | no | — |
| `timezone` | string | no | — |
| `idle_ms` | integer | no | — |

Requires id or label and at least one of enabled/wake_on_completion/model/when/message/interval_ms/cron/timezone/idle_ms. model, when and message apply to message jobs; an empty model returns the job to the pinned model, when attention needs a recurring schedule, and when quiet an interval or cron schedule. The `memory` job's marker message cannot be replaced (ADR-080 D5); replacing the message of a job whose `when` came from the runtime's tick text pins that `when`. At most one of interval_ms, cron and idle_ms replaces the schedule, validated as in create (message jobs on interval_ms need at least 60000 ms); the next fire is computed from the new schedule from now. A new cron keeps the job's timezone unless timezone is given; timezone alone moves a cron job to that zone. The job keeps its id, creator, counters and history, so the runtime's seeded `keepalive` and `memory` jobs keep the behavior tied to their ids. A nonempty id takes precedence if both selectors are supplied. Re-enabling resets consecutive failures; completion subscription applies to tool jobs and uses the creating session (trunk fallback).

#### trigger

Fire a job now, including disabled jobs; coalesce with an in-flight run.

[Implementation](../../peko-rs/cron/src/tools/trigger.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `id` | string | no | — |
| `label` | string | no | — |

Supply exactly one of id or label. Disabled jobs may be triggered; an in-flight job coalesces and returns its actual run_id.

#### history

Fetch a job’s recent run history.

[Implementation](../../peko-rs/cron/src/tools/history.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `id` | string | no | — |
| `label` | string | no | — |
| `limit` | integer | no | — |

Supply exactly one of id or label. limit defaults to 10, caps at 50, and returns newest runs first. Fired one-shot jobs delete themselves but keep their runs: read them by the job_id returned at creation (labels resolve live jobs only).

## MCP integrations

### MCP

Principal-owned connections and credentials are shared by all of that principal's
sessions. Configuration is `mcp/<id>/server.json` in its workspace. `Write`/`Edit`
and normal filesystem operations manage those definitions; the runtime reconciles
them before each model request. Cold servers discover tools automatically.
Discovered methods appear as `mcp__<server>__<method>` with their server schemas.
Calls preserve the advertised catalog binding and return an error if an unsent
call's definition changes. Started calls retain their original connection.

| action | Additional fields | Behavior |
|---|---|---|
| `status` | none | Own servers, source paths, connection/catalog state, tool counts, sanitized errors; does not connect |
| `refresh` | optional `server` | Scan files and fetch live catalogs for one or all configured servers |
| `reconnect` | required `server` | Replace the connection, reset retry backoff, and discover again |
| `authenticate` | required `server` | Return a browser authorization URL for OAuth; owner-bound callback completes without starting a run |
| `resources` | required `server` | List all resource pages |
| `resource_templates` | required `server` | List all template pages |
| `read_resource` | required `server`, `uri` | Read structured resource contents |
| `prompts` | required `server` | List all prompt pages |
| `get_prompt` | required `server`, `name`; optional string-map `arguments` | Retrieve prompt messages as tool data |

`MCP` is not parallelizable. Fields irrelevant to an action fail validation.
Stdio and Streamable HTTP use the MCP SDK; SSE remains a compatibility transport.
Sampling is opt-in and uses the principal's actual model/meter. See
ADR-075 and the
[quick start](../mcp/QUICK_START.md).

## Audit

### ToolLog

Read this principal's tool ledger (ADR-079): every tool call it made, whom the
run acted for, the session, and how the call ended.

[Implementation](../../peko-rs/core/src/tools/builtin/tool_log.rs)

| action | Fields | Returns |
|---|---|---|
| `search` | optional `tool`, `tool_action`, `session` (slug path or own session id) + `subtree` (default false), `actor`, `outcome`, `since` / `until` (RFC 3339 or an age like `30m`, `6h`, `2d`; default since 7d), `limit` (default 50, 1–200), `cursor` | `{ calls, next_cursor }`: compact rows newest first — `call_id`, `ts`, `tool`, `action`, `session` path, `actor`, `trust`, `outcome`, `duration_ms`, `exit_code`, `parent` |
| `get` | required `call_id` | The call's `start` and `ends` records (redacted params, result excerpt), its session path, its `parent` row, and its `children` rows |
| `turns` | optional `session` + `subtree`, `since` / `until` (default since 1d), `limit` (default 50, 1–200; the newest turns are kept) | `{ turns, omitted_older }`: one row per run, oldest first — `run_id`, `session` path, `actor`, `trust`, `started`, `ended`, `calls`, `tools` (name → count), `failed`, and the files it `wrote` / `read` and the channels it read (`channels_read`) and posted to (`channels_posted`) (ADR-085). Turns without tool calls are not listed |

Outcomes: `ok`, `error` (including a tool-reported failure such as a non-zero
Bash exit), `cancelled`, `floor_denied`, `policy_denied`, `awaiting_owner`,
`proposed`, `detached` (still running in the background; a late end follows),
and `in_flight` (no end yet). `tool_action` filters on the logged call's
declared action. Session paths resolve when read, so a moved session shows its
current path. Owner runs see the whole ledger; any other run sees only calls
made in its own compartment (ADR-078 D4), and the rest reads as missing.
Excerpts are tool output, returned with runtime tags defused. `ToolLog` is
parallelizable, read-only, and exempt from policy review.

### Changes

The files recent turns changed, and undoing one (ADR-117).

**Owner runs only.** Undo rewrites the owner's files; the floor refuses it to every other run.

[Implementation](../../peko-rs/core/src/tools/builtin/changes.rs) · [change log](../../peko-rs/core/src/tools/changes.rs)

| Parameter | Type | Required | Schema default / bounds |
|---|---|---|---|
| `action` | `list` \| `undo` | yes | — |
| `turn` | string | undo | a turn (run id) from `list` |
| `all` | boolean | no | list: every conversation, not only this one |
| `limit` | integer | no | list: 1–50, default 5 |

The dispatcher records what each run changes, in the principal's local tier (`<local>/changes/`): `Write` and `Edit` name the exact file they write (a `FileChangeSink` on the call's context); a `Bash` call's changes inside the peko's workspace are the difference of two scans around it (`.git`, `node_modules`, `target`, `.triage`, `.venv` skipped). Shell changes outside the workspace are not recorded; the turn is marked `shell`. A foreground `Agent` call folds the worker's run into the turn. `list` returns `{success, turns: [{turn, current, updated_at, files: [{path, change}], shell, undone_at?, …}]}`, newest first, in the caller's conversation unless `all`. `undo` puts each file back to what it was before the turn (and the runs it delegated), only where the file is still as the turn left it, and returns `{success, restored: [path], skipped: [{path, why}]}`; a turn is undone once. Files over 5 MB are named but not kept. Turns are kept 7 days. Calls run alone (not parallel).

## Testing

Three tiers cover the built-in tools; put a test in the lowest tier that can
observe the behavior.

| Tier | Where | Runs in | Use for |
|---|---|---|---|
| Unit | `#[cfg(test)]` beside each implementation | `cargo test --lib` | Argument handling and domain rules against the tool's own fake or backend |
| Harness | [`ToolHarness`](../../peko-rs/core/src/tools/builtin/test_harness.rs) | `cargo test --lib` | Dispatch semantics (validation, workspace injection, audit), multi-tool flows, caller/principal scoping |
| Daemon | [`cli_tools.rs`](../../peko-rs/core/tests/cli_tools.rs) | `make test-cli-tools` (mock LLM) | Daemon wiring only: workspace resolution, process spawning, result persistence |

`ToolHarness::new()` registers every built-in behind the production dispatcher,
with the harness principal's tool ledger bound,
backed by tempdir storage where it is cheap (workspace files, ChannelStore,
CronScheduler, ModelCatalog, AsyncExecutor) and fakes where the real backend
is a daemon (Task, Plan, Agent, Session). ModelCall and Workflow stay unbound
and fail closed; Fetch reaches no local address, so its pages are served in its
own unit tests. Its smoke test must cover the installation manifest, so a new
built-in fails until it has a backend and a smoke case. Cron actions take an
explicit runtime through `CronTool::with_runtime`; `peko_cron::testing` provides
`FileCronRuntime` behind the `test-support` feature.

Mock LLM replies are scripted, so a daemon test must assert on effects the
mock cannot produce (files, persisted tool results), never on a reply
sentinel alone. `make coverage` reports unit-tier line coverage via
cargo-llvm-cov.

## Related contracts

- [API_SURFACE.md](API_SURFACE.md)
- [DATA_MODEL.md](DATA_MODEL.md)
- ADR-066
- [ToolCatalog](../../peko-rs/core/src/tools/catalog.rs)
- [ToolDispatcher](../../peko-rs/core/src/tools/dispatcher.rs)
