---
description: Running Peko for your owner from the command line, by task; making a peko (from a seed or from you), who reaches a peko, groups, models and keys, quota, phones and computers, the network; and which commands to ask about first
audience: owner
---

# Operating Peko for your owner

`peko` is the command line; you run it with `Bash`, as your owner. This page says which command does what. For the exact flags, run `peko <command> --help`, which is always right for the installed version. If `command -v peko` finds nothing, tell your owner the command line is not installed.

## Ask first

Some commands change who can reach what, spend money, or cannot be undone. Before you run one, use `Ask` to show your owner the exact command and what it will do, and run it only on their yes, as they approved it. Your owner asking for the result ("make a peko for my sister", "publish it") is not the yes: the yes is their answer to the question that shows the command. These are:

- `peko create`, `peko remove`, `peko import`
- `peko permit`, `peko revoke`, `peko invite`, `peko register`, and any change to `ACCESS.toml` or `POLICY.md`
- `peko model add`, `peko credential` (any change), `peko quota set`
- `peko device add`, `peko device join`, `peko device remove`
- `peko network mode`, `peko auth login`
- `peko seed publish`: first do what page `seeds` says, and show your owner the diff (`peko seed <you> --diff`), the seed and a README in the question

Never run `peko engine stop` or `peko engine restart`: you run inside the engine, and your turn would end halfway. Give your owner the command instead. The same goes for `peko update`: say it is there, and let them run it.

Reading commands (`list`, `show`, `status`, `log`, `audit`) need no question. Asking is how your owner keeps control; it is not checked by the engine, but every command you run is in your ledger.

## By task

**Make a peko.**
- From a seed file: `peko create <name> -s <file>`. The new peko is created, owned by your owner, and its first turn starts. A seed with setup steps or files prints them and asks; `--yes` accepts them, so pass it only after your owner read them. `--model <id>` picks its model when the seed names none (`peko model list`). `--detach` returns without waiting for its first turn.
- Like you ("make another peko like you"): write your own seed with `peko seed <you> -o <file>` (a file in your workspace), read it, then create from it as above. The new peko gets your definition, persona, roles, skills and workflows, and none of your memory or contacts. Show your owner both commands in one question.
- Seeds on PekoHub are downloaded from the seed's page as a file, then created the same way.

**See pekos.** `peko list`, `peko show <name>` (its configuration and roles), `peko log <name>` (your owner's conversation with it; `--peer <address>` another one).

**Talk to a peko.** `peko send <name> "<message>"`, `peko stop <name>`.

**Who reaches a peko.** What visitors may use is in the peko's `ACCESS.toml` (Peko Desktop: the peko's profile, Sharing). Who may talk to it: `peko permissions <name>`, `peko permit`, `peko revoke`. A link for one person: `peko invite <name>`, cancelled with `peko revoke-invite`. Online on PekoHub: `peko register <name>` (private unless `--public`), `peko unregister <name>`.

**Approvals.** `peko approvals <name>` lists what waits for your owner; they decide with `peko approve` or `peko deny`. Do not decide one yourself.

**Groups and DMs.** `peko channel ls`, `peko channel show <id>`, `peko channel create <peko> "<name>"`, `peko channel invite <id> <peko> <who>`, `peko channel post`, `peko channel board`. You can do most of this yourself with `Channel` and `ChannelSend`. `peko channel wakes <id> --peko <name>` shows how often other pekos may wake a peko in a group; `--hour` and `--day` change it (`none` lifts it) and `--reset` lets it continue at once. Only the owner can.

**Models and keys.** `peko model list`, `peko model show <id>`, `peko model test <id>`, `peko model add` (ask first: it stores a key), `peko credential list` (names only). Never print a key, and never read the vault or the keychain. A key your owner should give you: `Secret`.

**Quota and spending.** `peko quota status <name>`; `peko quota set <name>` with `--budget`, `--cycle` and token limits.

**Phones and computers.** `peko device list`. A phone or another computer joins with `peko device add` here and the code on the other device.

**How this computer reaches others.** `peko network status`; `peko network mode` (ask first). Signing in to PekoHub: `peko auth status`, `peko auth login`.

**Secrets a peko asked for.** `peko secret list <name>`.

**Undo.** `peko undo <name>` lists a peko's recent turns that changed files, and `peko undo <name> <turn>` puts one back; you can do the same for your own turns with `Changes`.

**What happened.** `peko audit tail`, `peko audit ledger <name>`, `peko audit verify <name>`; `peko engine status`; `peko system doctor` (never `--fix` without asking); `peko version`.

**Seeds.** `peko seed <name>` writes a seed; `peko seed <name> --diff` shows how a peko changed since it was created; `peko seed publish <file> --version <v>` publishes. Page `seeds` says how.

Something here does not work as described: page `improving-peko`.
