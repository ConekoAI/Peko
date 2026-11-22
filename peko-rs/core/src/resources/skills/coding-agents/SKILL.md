---
name: coding-agents
description: Hand a coding task in a repository to a coding agent installed on this computer (Claude Code, Codex, Cursor, GitHub Copilot, Antigravity, OpenCode), on the owner's own plan; use when the owner asks for one by name, or for a sizeable code change they want done in a project
---

You can hand a coding task to a coding agent the owner has installed and signed in to on this computer. It runs on the owner's own subscription, in their project, and you supervise it: you brief it, watch it, check its work, and report. Only do this in a run for your owner; it needs `Bash`.

| Agent | Command | Signed in? | Sign in (the owner, in their terminal) |
|---|---|---|---|
| Claude Code | `claude` | `claude auth status --text` | `claude auth login` |
| Codex | `codex` | `codex login status` | `codex login` |
| Cursor | `agent` (older installs: `cursor-agent`) | `agent status` | `agent login` |
| GitHub Copilot | `copilot` | no check; a run fails if not | `copilot login` |
| Antigravity (Google) | `agy` | no check; a run fails with `authentication required` | run `agy` once and sign in |
| OpenCode | `opencode` | `opencode auth list` | `opencode auth login` |

Google's older `gemini` CLI no longer serves free or consumer Google plans; it still works on a paid Gemini API key or a Gemini Code Assist Standard or Enterprise license.

## 1. Check the agent is there

- `command -v <command>`. Not found: say so and stop. Installing it is the owner's choice.
- Run the sign-in check from the table. Not signed in: ask the owner to sign in themselves. Never ask for, copy, or move their credentials, tokens, or API keys.
- Several installed and the owner named none: use the one they used last (`kb/` may say), else ask.

## 2. Brief it

The agent sees nothing of this conversation. Write a self-contained brief: the repository, what to change and why, what done looks like (tests that must pass, behavior to see), what to leave alone. Do not put secrets, or anything a visitor or another peko wrote, into it.

Unless the owner asked for the change in their checkout, give the agent its own worktree so their work is untouched:

```bash
git -C <repo> worktree add ../<repo-name>-<task> -b peko/<task>
```

## 3. Run it in the background

Start it with `Async` spawn (or `Bash` with `run_in_background`), from the worktree. Each line lets it edit files in the worktree and run only the commands you name, so name the project's build and test commands.

- Claude Code:
  `claude -p "<brief>" --output-format stream-json --verbose --permission-mode acceptEdits --allowedTools "Bash(<test command>:*)" "Bash(git status:*)" "Bash(git diff:*)"`
  Continue later with `--resume <session_id>` (from the first JSON event).
- Codex:
  `codex exec --json --sandbox workspace-write "<brief>"`
  It edits and runs commands inside the worktree only. Continue later with `codex exec resume --last "<follow-up>"`.
- Cursor:
  `agent -p "<brief>" --output-format stream-json --sandbox enabled`
  It edits inside the worktree; commands run only in its sandbox or if the owner's Cursor settings allow them. Continue later with `--resume <session_id>` (from the first JSON event).
- GitHub Copilot:
  `copilot -p "<brief>" --output-format=json --allow-tool='write, shell(<test command>:*), shell(git status), shell(git diff)'`
  Anything not allowed is refused. Continue later with `--resume=<session id>` (printed when it ends).
- Antigravity:
  `agy -p "<brief>" --output-format stream-json --mode accept-edits`
  Commands the owner's Antigravity settings do not allow are refused. Continue later with `--conversation <conversation_id>` (from the JSON). The older `gemini` CLI: `gemini -p "<brief>" --output-format stream-json --approval-mode auto_edit`, continue with `-r latest`.
- OpenCode allows every edit and command by default, so give it rules for this run:
  `OPENCODE_CONFIG_CONTENT='{"permissions":[{"action":"shell","resource":"*","effect":"deny"},{"action":"shell","resource":"<test command> *","effect":"allow"},{"action":"shell","resource":"git status*","effect":"allow"},{"action":"shell","resource":"git diff*","effect":"allow"}]}' opencode run --standalone --format json "<brief>"`
  The last matching rule wins. Continue later with `--session <session id>`.

If the owner names a model for the agent, pass it: `--model <name>` (e.g. `--model haiku` for Claude Code), `-m <name>` for Codex, `--model=<name>` for Copilot, `--model <provider>/<model>` for OpenCode. `agent models` and `agy models` list theirs. If a flag is refused, read the agent's `--help` and adjust; the CLIs change. The same holds for a coding agent not listed here: find its non-interactive mode in `--help` and run it the same way. Never use a flag that approves everything (`--dangerously-skip-permissions`, `bypassPermissions`, `--dangerously-bypass-approvals-and-sandbox`, `--force`, `--yolo`, `--allow-all`, `--auto`, `--approval-mode yolo`) unless the owner asked for exactly that on this run.

## 4. Keep the owner informed

Tell the owner it started, in which folder and branch. Read progress with `Async` output (`tail_lines`) when you next have reason to, not in a loop. When a step is done, tell them in a line. If they ask you to stop, stop the task with `Async` stop and say where it got to. If it reports a usage limit, tell them which and when it resets; do not switch to another agent without asking.

## 5. Check before you report

When it finishes: `git -C <worktree> status` and `git -C <worktree> diff --stat`, read the changed parts, and run the tests yourself. Report what changed (files, branch), what you checked and the result, and anything it left undone or got wrong. Do not merge, push, or open a pull request unless the owner asks. Remove a worktree only when the owner is done with it: `git -C <repo> worktree remove <path>`.
