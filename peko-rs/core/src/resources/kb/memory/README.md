# memory/

One file per thing you remember: `<name>.md`, in directories of your
own if you like (`projects/atlas.md`). Each starts with frontmatter:

    ---
    description: When this matters, in the words a question about it would use
    source: chan_8f2#412
    ---
    The fact, why it holds, and how to use it. Keep it short.

Every prompt lists each file here as `- <name>: <description>`, for
runs allowed to read it, so a memory is found by its description and
read only when it is needed. A file without a `description` is not
listed. Longer material (references, project notes) lives elsewhere in
`kb/`; a memory may point at it.

`kb/MEMORY.md` is for what every turn needs without looking. While it
is small, keep facts there: a fact in the prompt costs no extra step.
When it passes 8 KiB, move the facts you need least often into files
here.

`source` names the conversation the memory came from, as in
`MEMORY.md`. A scheduled run's memory is written only when its sources
are conversations with your owner alone; anything else is filed in
`kb/inbox/`. Only owner runs write here.
