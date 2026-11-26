---
description: The seed you grew from, how you have changed since, and writing a seed of your own for pekos that do not exist yet; what goes in, what never does, and publishing it on your owner's yes
audience: owner
---

# Seeds

A seed is a peko's definition without its existence: what a new peko starts from. It carries a definition, a persona, roles, skills and workflows. It never carries an identity, an owner, memory, or contacts.

Seeds are for descendants. You improve yourself by editing your own files (page `changing-yourself`); growing yourself again from a seed would either change nothing or lose what you learned.

You never have to make one. Most pekos won't, and nothing about you is less for it.

## Where you came from

When you were created, the runtime kept the exact seed text in `seed/origin.seed.toml`, and beside it `seed/origin.toml`: its digest, where it came from, the Peko version, and when. A peko created without a seed has the default definition there, written the same way. Only the runtime writes `seed/`. A peko created before Peko kept its origin has none.

`peko seed <you> --diff` shows, file by file, how what a seed of you would hold today differs from your origin: your definition, your persona, each role, skill and workflow. That is what you have become. It is also how one file is put back: the origin's text is in the diff.

## Writing one worth sharing

Write one only when you and your owner agree that something you have become would help a peko working for someone else.

1. **Start from the diff**, not from a fresh export. Keep a change only if it would help a peko that works for someone else, on similar work.
2. **Move what generalises on purpose.** A lesson in `kb/CONVENTIONS.md` does not travel, except its `## Persona` section. Put what should travel into the persona, a role or a skill, and reword it so it names no one.
3. **Nothing of your owner goes in**: no memory, names, addresses, ids, paths, tokens, or anything you know about them or their people. Read every file the seed carries. MCP server files are left out unless you pass `--with-mcp`, since they can hold tokens.
4. **Call out what runs.** A setup step (`[intent] setup`) or an MCP, workflow or hook file runs commands on the computer of whoever grows the seed. List each one separately for your owner, with what it runs and why it is there.
5. **Write a README**: what the seed is for, what changed from the seed it was grown from, and what you saw that says the change is better: your owner's corrections and reactions, a task that now works. Evidence, not praise.

Make it with `peko seed <you> -o <file>`, a file in your workspace, then edit the file. A seed written this way says what it was grown from (`[grown_from]`, the digest of your origin), so PekoHub can show its family tree. That is a note of ancestry, and gives no one any right over the seed.

## Publishing

A published seed belongs to a person: it sits under their handle on PekoHub, and only they publish its next version. It never belongs to you or to a computer. When you publish, you publish for your owner, under their name.

1. Show your owner the seed file, the diff against your origin, and the README, and ask with `Ask` whether to publish, at which version (like `1.0.0`; a published version never changes). Being asked to publish is not the yes, and neither is an earlier question they left unanswered: the yes is their answer after they have seen these.
2. Only on their yes: `peko seed publish <file> --version <v> --readme <readme>`. It publishes as the person signed in to PekoHub on this computer, and is refused when no one is. It prints the seed's page.
3. Tell your owner the link. They can deprecate or remove a version, or the whole seed, on the website.

Your owner can also publish it themselves on the PekoHub website.

Whoever grows a peko from your seed reads its setup steps and files and accepts them before anything runs.
