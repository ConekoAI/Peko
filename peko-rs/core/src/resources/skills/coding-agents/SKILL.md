---
name: coding-agents
description: Hand a coding task in a repository to Claude Code or Codex installed on this computer, on the owner's own plan; use when the owner asks for either by name, or for a sizeable code change they want done in a project
---

You can hand a coding task to a coding agent the owner has installed and signed in to on this computer: Claude Code (`claude`) or Codex (`codex`). It runs on the owner's own subscription, in their project, and you supervise it: you brief it, watch it, check its work, and report. Only do this in a run for your owner; it needs `Bash`.

## 1. Check the agent is there

- `command -v claude` / `command -v codex`. Not found: say so and stop. Installing it is the owner's choice.
- `claude auth status --text` / `codex login status`. Not signed in: ask the owner to run `claude auth login` or `codex login` in their own terminal. Never ask for, copy, or move their credentials, tokens, or API keys.
- Both installed and the owner named neither: use the one they used last (`kb/` may say), else ask.

## 2. Brief it

The agent sees nothing of this conversation. Write a self-contained brief: the repository, what to change and why, what done looks like (tests that must pass, behavior to see), what to leave alone. Do not put secrets, or anything a visitor or another peko wrote, into it.

Unless the owner asked for the change in their checkout, give the agent its own worktree so their work is untouched:

```bash
git -C <repo> worktree add ../<repo-name>-<task> -b peko/<task>
```

## 3. Run it in the background

Start it with `Async` spawn (or `Bash` with `run_in_background`), from the worktree:

- Claude Code:
  `claude -p "<brief>" --output-format stream-json --verbose --permission-mode acceptEdits --allowedTools "Bash(<test command>:*)" "Bash(git status:*)" "Bash(git diff:*)"`
  `acceptEdits` lets it change files; commands it may run are only those in `--allowedTools`, so name the project's build and test commands there. Continue the same session later with `--resume <session_id>` (from the first JSON event).
- Codex:
  `codex exec --json --sandbox workspace-write "<brief>"`
  It edits and runs commands inside the worktree only. Continue later with `codex exec resume --last "<follow-up>"`.

If the owner names a model for the agent, pass it: `--model <name>` for Claude Code (e.g. `--model haiku`), `-m <name>` for Codex. If a flag is refused, read `claude --help` or `codex exec --help` and adjust; the CLIs change. Never add `--dangerously-skip-permissions`, `bypassPermissions`, or `--dangerously-bypass-approvals-and-sandbox` unless the owner asked for exactly that on this run.

## 4. Keep the owner informed

Tell the owner it started, in which folder and branch. Read progress with `Async` output (`tail_lines`) when you next have reason to, not in a loop. When a step is done, tell them in a line. If they ask you to stop, stop the task with `Async` stop and say where it got to. If it reports a usage limit, tell them which and when it resets; do not switch to the other agent without asking.

## 5. Check before you report

When it finishes: `git -C <worktree> status` and `git -C <worktree> diff --stat`, read the changed parts, and run the tests yourself. Report what changed (files, branch), what you checked and the result, and anything it left undone or got wrong. Do not merge, push, or open a pull request unless the owner asks. Remove a worktree only when the owner is done with it: `git -C <repo> worktree remove <path>`.
