# Skills

---

## What a skill is

A **skill** is a reusable procedure a peko can load on demand: a Markdown
file with a small YAML header, living in the peko's workspace:

```
~/.peko/pekos/<name>/skills/<skill-name>/SKILL.md
```

That is the whole mechanism. Skills are *just files* — no registry, no
install step, no validation pipeline, no lifecycle CLI. The agent owns
discovery, creation, editing, and use of its skills; the runtime's only
job is to make the files visible and to hand back a skill's body when
asked for it by name.

The **directory name is the skill's name**. `skills/docker/SKILL.md` is
invoked as `Skill {name: "docker"}` regardless of what the frontmatter
`name:` field says — the frontmatter name is display metadata, the
directory is the lookup key (`extensions/skill/reader.rs`).

---

## The file

`SKILL.md` is YAML frontmatter between `---` fences, then a Markdown
body (source of truth: `tools/builtin/skill/frontmatter.rs`):

```markdown
---
name: docker
description: Use when building, running, or debugging Docker containers and compose stacks.
tags: [docker, containers]
author: ops-principal
arguments: [compose-file]
shell: bash
allowed-tools: ["docker *", "pwd"]
---

# Docker

Body text the model follows when the skill is invoked…
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Display name. The **directory** name is the real key. |
| `description` | yes | The one line the catalog shows — see *Authoring* below. |
| `tags` | no | Free-form labels. Not read by the runtime. |
| `author` | no | Free-form attribution. Not read by the runtime. |
| `arguments` | no | Positional argument names; order maps to the `args` array at invoke time (`$0` → first name, …). |
| `shell` | no | Only `"bash"` is accepted (the default). Any other value fail-closes the shell blocks below. |
| `allowed-tools` | no | Glob allowlist for dynamic-context shell commands. **Empty (or absent) means all commands are allowed.** Globs are case-sensitive and anchored to the full trimmed command (`git *` allows `git status`, never `true; rm -rf /`). |

A file whose frontmatter is missing or malformed is skipped at scan
time — the skill silently drops out of the catalog (a warning lands in
the daemon log), and invoking it by name fails with a
`skill_unreadable` error.

---

## Discovery — how the agent knows a skill exists

Skills are not listed in the system prompt. Once per turn, the runtime
scans `<workspace>/skills/` and renders a one-line catalog into the
tail `<runtime-context>` user message
(`extensions/skill/prompt.rs` → `engine/prompt/renderer.rs`):

```
## Skills (mandatory)
…
<available_skills>
- docker: Use when building, running, or debugging Docker containers… (skills/docker/SKILL.md)
- git-release: Use when cutting a release — changelog, tag, version bump (skills/git-release/SKILL.md)
</available_skills>
```

Properties of the catalog:

- **Description only.** The catalog shows the `description` line and
  the file path — never the body. This is deliberate progressive
  disclosure: the model decides from one line whether to load the full
  procedure.
- **Presence = visibility.** Every well-formed `SKILL.md` in the
  workspace appears. There is no capability or activation filter on
  listing or invocation (ADR-066).
- **Principal-scoped.** Invocation uses the owner’s workspace binding. The shared
  discovery cache includes the workspace path, so matching file metadata in
  another principal cannot reuse its catalog. A principal moved to a different
  workspace refreshes its tool binding within the same daemon.
- **Fresh next turn.** The scan is cached on the workspace path and a per-file `(mtime, len)`
  fingerprint, so creating or editing a `SKILL.md` is visible on the
  next loop iteration — no restart, no reload command.
- **Capped at 8 KB.** On overflow, whole lines are truncated from the
  end and a pointer is appended: `(more skills in <workspace>/skills/ —
  list the directory to see all)`. If you expect many skills, keep the
  catalog scannable rather than exhaustive.
- **The rendered name is the directory name**, so every line is
  directly invocable.

---

## Usage — what happens on invoke

The model calls the `Skill` tool:

```json
{ "name": "docker", "args": ["compose.prod.yaml"] }
```

The runtime resolves `skills/docker/SKILL.md`, reads the body, and
returns it as the tool result; the model then follows the body as
instructions for the rest of the turn. Two transformations run on the
body, in this order (`tools/builtin/skill/tool.rs`, `body.rs`):

**1. Dynamic context (shell injection).** Inline `` !`cmd` `` spans
(recognized only at line start or after ASCII whitespace) and fenced
blocks…

````
```!
git status --short
```
````

…are executed in source order and replaced by their stdout. Commands run via `bash -c`
with the working directory set to the **workspace root** — not the
skill directory — so reference sibling files as
`skills/<name>/scripts/foo.sh`, never `./scripts/foo.sh`. Limits: 5 s
timeout, 30 KB retained per stream (then `...(truncated)`). Both pipes drain
concurrently; excess output is discarded while reading. Cancellation kills the
command process tree. Bash must be installed on the destination host, including
Windows. On Windows, native Bash on PATH or in a Git for Windows installation
is selected; the WSL launcher is excluded because it cannot honor native
workspace paths. The runtime never interprets Bash commands as PowerShell. On non-zero
exit the stdout is inlined verbatim plus a `stderr: <stderr>` line; on
timeout, `stderr: command timed out after 5000 ms`; commands rejected
by `allowed-tools` leave the placeholder literal behind a
`[shell blocked: …]` marker. The pass runs once — command output is not
re-scanned for further placeholders. Use this for live-state-aware
bodies (branch name, running processes, env) instead of making the
model call `Bash` first.

**2. Argument substitution** (Claude-compatible):

- `$ARGUMENTS` — the full `args` array joined with spaces
- `$0`, `$1`, … — positional, 0-indexed; each numeric token is parsed once
- `$name` — names from the frontmatter `arguments:` list, mapped to
  positions in order
- `\$` — escape: renders a literal `$`

Placeholders with no matching argument are left literal, so the model
can see what it failed to supply and re-invoke. Names match whole tokens:
`$issue` never alters `$issue_id`. Argument values are inserted literally and
are never scanned as additional placeholders. Numeric values do not control
iteration count. Definitions and rendered bodies are limited to 1 MiB. Limits apply while
reading and appending, including repeated argument expansions; oversized
procedures fail instead of returning a truncated set of instructions.

Unknown skills (including invalid names or linked definitions) return a tool
error listing available directory names. `skill_unreadable` covers unreadable,
oversized, invalid UTF-8, or malformed definitions. Cancelled invocations return
an aborted tool error.

---

## Pages — long skills read in parts (ADR-127)

A skill may have pages: Markdown files under `skills/<name>/pages/`,
each read on its own with `Skill` `page`, so a run loads what it needs and
no more.

```
skills/house-manual/
├── SKILL.md            # the map: what each page covers, when to open it
└── pages/
    ├── kitchen.md
    └── accounts.md
```

```markdown
---
description: What this page covers and when to open it (one line)
audience: owner        # or `all`, the default
---

# Accounts
…
```

- **The map.** `Skill { "name": "house-manual" }` returns the `SKILL.md`
  body followed by a `## Pages` list (each page's name and
  `description`), and `pages`, their names. The list holds only the pages
  for the run's standing: an `owner` page is listed only for runs acting
  for the owner.
- **A page.** `Skill { "name": "house-manual", "page": "accounts" }`
  returns `{name, page, audience, body}`: the page's text after its
  frontmatter. Pages are plain text: no dynamic context, no argument
  substitution. A page is at most 8 KB, frontmatter included; a longer one
  is an error.
- **Audience is not protection.** A run that is not the owner's and asks
  for an `owner` page gets three lines instead: the page is its owner's to
  act on, and how to raise it. Put nothing in a page that would matter if
  read: what a run may do is decided by the trust floor and
  `ACCESS.toml`, not by what it has read.
- **Names** are lowercase letters, digits, `-` and `_`. An unknown page
  is an error listing the pages for the run.

## Skills that ship with the runtime

Four skills are compiled into the runtime (`peko-rs/core/src/resources/skills/`,
`extensions/skill/bundled.rs`): they describe the running version, work
offline, and are listed in the catalog as `(ships with the runtime)`.
`peko seed` and snapshots never carry them.

| Skill | Listed for | What it is |
|---|---|---|
| `peko` | every run | The peko's own manual (ADR-127): how it works, its standing, changing itself, operating Peko, improving Peko, seeds. Six pages, each `all` or `owner` |
| `coding-agents` | owner runs | Handing coding work to a coding agent CLI |
| `computer-use` | owner runs | Using the owner's computer (`Computer`) |
| `triage` | owner runs | Diagnosing Peko itself and drafting an issue |

"Listed for" is a catalog filter only: `Skill` runs any of them for any
run that has the tool, and the trust floor refuses the tools a skill needs
(`Bash`, `Computer`) to runs that may not have them.

A workspace skill of the same name replaces a bundled one entirely, its
pages included. A page file alone, `skills/<name>/pages/<page>.md` with no
`SKILL.md` beside it, adds a page to the bundled skill of that name or
replaces the one of its name, so an owner or a seed can change one page of
the manual by file. Whoever does that takes on keeping it current.

The manual's pages are kept true by tests (ADR-127 D9): every `peko …`
command a page names is in the parser (`peko-rs/cli`) and in
[`CLI_REFERENCE.md`](../user-guide/CLI_REFERENCE.md), every tool it names
is a built-in, each page declares its audience and fits in 8 KB, and the
map fits in 2 KB.

---

## Authoring guidance

**Write the `description` for your future self.** It is the *only*
signal in the catalog. A future turn — possibly months later, mid-task
— will scan a list of one-liners and decide in a second whether this
skill applies. So make it discriminating and trigger-oriented, not a
summary of the contents:

- Good: `Use when cutting a release — changelog, version bump, tag.`
- Good: `Use when the user asks about deployment topology or which
  service owns a table.`
- Bad: `A skill for Docker operations.` (says nothing about *when*;
  collides with every other ops skill)

Lead with the trigger (`Use when…`), then the differentiator. If two
skills' descriptions could both match the same situation, tighten them
until they can't.

**Keep `SKILL.md` focused.** The body is injected into the context in
full on every invocation. Put the procedure there; push long reference
material (schemas, examples, lookup tables) into sibling files —
`skills/<name>/references/api.md` — and point at them from the body
("read `skills/<name>/references/api.md` when you need field
definitions"). The model loads those only when the procedure actually
needs them.

**Extract, don't hoard.** A skill earns its place when a procedure is
reusable across sessions: you caught yourself re-deriving the same
steps, or a correction should stick permanently. One-off task notes
belong in the knowledge base or the session, not in `skills/`.

**Name directories lowercase-hyphenated** (`git-release`, not
`GitRelease` / `git_release`). The directory name is the invoke key and
appears verbatim in the catalog.

**Test by invoking.** Create the file, then in the next turn ask the
peko to do something the skill covers — or invoke it directly. Check
that the catalog line appears, that `$0`/dynamic-context blocks render
as intended, and that the body is actually followable by a model that
has never seen it before.

---

## Trust model

- **Skills are plain files.** No validation, no install step, no
  lifecycle commands — the retired `peko ext *` CLI is not coming back
  (ADR-050). Creating, editing, and deleting files *is* the management
  surface.
- **Malformed = invisible, not fatal.** A broken `SKILL.md` drops out
  of the catalog with a log warning; nothing else is affected.
- **Presence = executability.** There is no capability grant check. Skills
  belong to the principal whose workspace contains them; all its sessions
  share the same skill binding.
- **Names can't escape `skills/`.** A skill name must be a single plain
  path component — anything containing `/` or `\`, or equal to `.` /
  `..`, is refused. Linked `skills/` roots, skill directories, and `SKILL.md`
  definitions are excluded by both discovery and invocation.
- **Dynamic context is shell access.** `` !`cmd` `` blocks run real
  commands as the daemon user with the workspace as cwd. Treat
  `allowed-tools` as the skill author's self-imposed leash, and
  remember the default (absent/empty) is *allow all*.

## Snapshot portability

A `.peko` export includes the entire `skills/<name>/` tree: definitions, scripts,
references, and assets. A renamed import restores it under the destination
principal, and the next turn discovers it without reinstallation. Unix
executable files are recorded in the optional manifest `executable_files` list
and restored as `0755`; other workspace files restore as `0644`. Special mode
bits and original ownership are not transported. Older v1 snapshots remain
readable, but cannot recover executable bits they never recorded.

Snapshots reject symbolic links and special files rather than dereferencing
them or silently dropping assets. Force import also rejects linked destination
paths. Bundle ordinary files under the skill directory; external programs,
credentials, absolute host paths, and operating-system behavior remain host
dependencies. On Windows, use an explicit interpreter for bundled scripts.

---

## See also

- PRINCIPAL_WORKSPACE.md — the full workspace
  layout (`tools/`, `mcp/`, `hooks/`, `plugins/` follow the same
  files-are-the-truth rule).
- [Built-in tools](BUILTIN_TOOLS.md) — where the `Skill` tool sits in
  the built-in catalog.
- adr/ — ADR-047 (workspace trust boundary), ADR-050
  (capabilities as files), ADR-052 (tiered system prompt).
