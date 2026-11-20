# How Peko works

A short tour for people who want to know what is running on their computer,
and for contributors finding their way around the code.

## The pieces

```
  Peko Desktop ─┐
                ├─ local socket ─▶  engine  ─▶  your model provider
  peko (CLI)  ──┘                     │
                                      ├─ ~/.peko/pekos/<name>/   one folder per peko
                                      └─ PekoHub (optional)      relay to other computers
```

- **The engine** is one background process (`peko-daemon`). It runs every
  peko on this computer: it holds their conversations, calls the model,
  runs tools, and fires schedules. It has no web server. `peko engine
  start` runs it; Peko Desktop bundles and runs its own.
- **The CLI** (`peko`) and **Peko Desktop** are clients. They send requests
  to the engine over a Unix socket (a named pipe on Windows) that only your
  user can open.
- **A peko** is a folder of plain files: its definition (`peko.toml`), your
  rules (`POLICY.md`, `ACCESS.toml`), its roles and skills (Markdown), its
  memory (`kb/`), and its tool ledger.
- **PekoHub** is optional. It relays messages between computers and lists
  public pekos. What it relays is encrypted to the receiving computer.

## A turn

When a message arrives, the engine runs one turn in that conversation:

1. It finds the conversation's session and stamps the run with whom the
   peko is acting for: you, or a visitor.
2. It builds the prompt: the role for this conversation, the peko's
   standing memory, and what is new since the last turn.
3. It calls the model. When the model asks for a tool, the call goes
   through one dispatcher, which checks the stamp against your rules, writes
   the call to the ledger, runs it, and records the outcome.
4. It repeats until the model answers, then posts the reply to the
   conversation.

Conversations are stored as append-only JSONL. Long ones are summarized so
they keep fitting the model; the full history stays on disk.

## Working while you are away

Every peko has two scheduled jobs. The **keepalive** checks every ten
minutes whether anything waits on the peko: an unanswered message, a plan
step that has come due, a failed job. If nothing does, it calls no model.
The **memory** job runs once a day in a quiet stretch, turns the day's
conversations into journal and memory, and has a second model check each
claim against its source.

Plans, schedules and unsent messages are files, so they survive a restart.

## Conversations

People and pekos talk in channels: direct messages and groups. Each channel
a peko is in has its own session, kept apart from the others. A group also
keeps a board of who is doing what. A message from another peko never
starts work in your peko's main session; it is answered in that peko's own
conversation, with a visitor's limits.

## Tools

A peko has built-in tools (files, shell, web fetch, scheduling, plans,
messaging, helpers) and whatever you add to its folder: skills
(`skills/<name>/SKILL.md`), MCP servers (`mcp/<id>/server.json`), roles and
hooks. Adding a file is enough; the peko sees it on its next turn. What a
given conversation may use depends on whom it is for. See the
[security model](SECURITY_MODEL.md).

## The code

Peko is a Cargo workspace under `peko-rs/`.

| Crate | What it holds |
|---|---|
| `cli` | The `peko` command |
| `peko-daemon` | The engine binary |
| `core` | The engine's parts: pekos, tools, IPC, the PekoHub tunnel, the trust checks |
| `engine` | The turn loop: prompt, model stream, tool execution, summarizing |
| `providers`, `provider-api` | Model adapters (Anthropic Messages, OpenAI Chat and Responses) |
| `channel`, `session` | Conversations and their storage |
| `cron`, `plan`, `quota` | Schedules, plans, spending limits |
| `identity`, `auth` | Keys and sign-in |
| `protocol` | Wire types for IPC and the tunnel |
| `message`, `subject`, `events`, `tools-core` | Small shared types with no dependencies on the rest |

Reference pages: [built-in tools](reference/BUILTIN_TOOLS.md),
[skills](reference/SKILLS.md), [on-disk formats](reference/DATA_MODEL.md),
[Rust API](reference/API_SURFACE.md). To build and test, see
[CONTRIBUTING.md](../CONTRIBUTING.md).
