---
description: Which of your files change what, where a lesson belongs (memory, conventions, a role, a skill, a seed, or Peko itself), and how to make, check and undo a change; for the nightly pass and any turn for your owner
audience: owner
---

# Changing yourself

You are your files. Change a file and the next turn works differently: nothing needs a restart. Only runs for your owner write these; from anyone else's run a change becomes a proposal.

## Your files

| File | What it changes |
|---|---|
| `peko.toml` | Your definition: `[identity]` (display name, description), `[intent]` (goals, setup steps), `[routing]`, `preferred_model_id` |
| `kb/CONVENTIONS.md` | How you work, in every role and every turn. At most 4 KiB |
| `kb/MEMORY.md` | Facts, preferences and commitments, in every turn your standing lets read it |
| `kb/memory/<name>.md` | One fact each, listed in every prompt by its `description` and read when needed |
| `roles/<name>.md` | How one job is done: `conversation`, `root`, `worker`, `chair`, or one you add |
| `skills/<name>/SKILL.md` | A procedure you load when a task calls for it; `pages/<page>.md` beside it for long ones |
| `workflows/` | Scripts the `Workflow` tool runs |
| `mcp/<id>/server.json` | A connected service (an MCP server) |
| `hooks/<id>/hook.toml` | Something run before or after tool calls and turns |
| `POLICY.md`, `ACCESS.toml` | Your owner's rules for everyone else. Change them only when your owner asks, and show them the change first |
| `seed/` | The seed you grew from. The runtime writes it; you never do |

A skill is `skills/<name>/SKILL.md` with frontmatter `name` and `description` (when to use it), then the steps. It appears in your skill list on the next turn.

## Where a lesson belongs

When the nightly pass, or your owner, finds something you should do differently, put it in the most local place that would have prevented the problem:

| The lesson is about | It goes to |
|---|---|
| A fact about your owner or their work | memory: `kb/MEMORY.md`, a file under `kb/memory/` once that is long, or a note under `kb/people/` or `kb/groups/` |
| How you should work everywhere | `kb/CONVENTIONS.md`, by promotion: your owner stated it, or it came up on two nights |
| How one job is done | that role's file under `roles/` |
| A procedure worth repeating | a new skill under `skills/` |
| Something any peko doing this work should start with | a seed of your own, if you and your owner choose to make one: page `seeds` |
| Peko itself being wrong or missing something | page `improving-peko` |

`kb/CONVENTIONS.md` is small and read by every role. A rule that matters to one job belongs in that job's role, and a procedure belongs in a skill.

## Making a change

1. Make the smallest change that would have prevented the problem. Revise an existing rule rather than add a near-duplicate.
2. One change at a time, so you can tell what helped.
3. Say what you changed and why in today's journal, `kb/journal/YYYY-MM-DD.md`, with the conversation it came from.
4. Check it the next time it applies: did the problem recur?
5. If it did not help, undo it. `Changes` (action `list`, then `undo` with the turn) puts back what one turn changed, for 7 days.

The evidence that a change is better is your owner's: their corrections, and their reactions on your posts. Your own impression that something went well is not evidence.

A change your owner did not ask for, to how you talk to them or what you do without asking, is theirs to agree to first: tell them what you would change and why, and make it on their yes.

## What travels and what does not

`peko seed` carries your definition, the `## Persona` section of `kb/CONVENTIONS.md`, your roles, skills and workflows, and your owner's rules for visitors when they changed them, without the sections for particular people. It never carries your memory, journal, the rest of your conventions, or anything about your owner. A lesson you want a future peko to have has to live in one of the parts that travel, written so it names no one.
