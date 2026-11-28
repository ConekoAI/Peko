---
description: When the fault or the gap is in Peko itself, not in your files; a bug goes to the triage skill, a missing capability becomes a proposal your owner reads and posts
audience: owner
---

# Improving Peko

Some problems no role, skill or seed can fix: Peko itself does something wrong, or cannot do something it should. Peko is open source (`https://github.com/ConekoAI/Peko`, the desktop app included), and a clear report helps every peko.

First make sure it is Peko and not you: a rule in your conventions, a role, a skill, or how your owner set you up. Page `changing-yourself` covers those.

## A fault: something is broken

Run the `triage` skill (`Skill` with name `triage`). It finds what happened from the logs and your ledger, reads the source of the version you run, checks whether the problem is known or already fixed, offers your owner a way past it, and drafts a bug report.

## A gap: Peko could do this better

You found, on your own, that something stands in the way of good work: a tool that cannot do what the task needs, a limit with no way around it, something your owner has to do by hand every time.

1. **Check it is real.** Read the source of the version you run, as `triage` does (it says how to fetch it once and reuse it). The capability may exist under another name; this manual and `peko <command> --help` may already cover it.
2. **Check it is not already asked for.** Search open and closed issues: `gh issue list --repo ConekoAI/Peko --state all --search "<words>"`. If it is, tell your owner and give them the link; adding a comment is theirs to decide.
3. **Draft a proposal**, short and concrete:
   - what you were trying to do, and for what kind of work;
   - what stood in the way, quoting the tool's answer or the code you read;
   - what would have helped;
   - the smallest change you can see, with file and line references in the source, and how it fits the way Peko works (its design docs are in the source under `docs/`).
4. **Remove everything private,** by `triage`'s rules: the repository is public, so no messages from conversations, names, addresses, ids, paths with a person's name in them, tokens or keys. Use placeholders such as `<peko>`, `<user>`, `/Users/<me>/`.
5. **Show your owner the whole title and text.** They post it, or say yes to your posting it. With `gh` signed in, write the text to a file in your workspace and run `gh issue create --repo ConekoAI/Peko --title "<title>" --body-file <file>` only after that yes; without it, give them `https://github.com/ConekoAI/Peko/issues/new` and the text to paste.

You never post on your own, and never more than your owner chose to send. End the text with a line saying you drafted it, and which model you are.

A security problem (anyone reaching what they should not, a way around your owner's approval, a secret leaking) never goes in a public issue: give your owner `https://github.com/ConekoAI/Peko/security/advisories/new` instead.

## While you wait

Do not patch Peko's installed files: the next update loses the patch, and nobody else gets it. If there is a way around the problem in your own files, use it, and say in your journal that it stands in for a fix.
