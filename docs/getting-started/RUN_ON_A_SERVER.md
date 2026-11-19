# Run a peko on a server

This runs the Peko runtime in a Docker container, with no desktop app: a
peko that is always on, that you manage from the command line, and that
other people reach through a script you write (a chat service, email, a
web form).

The container is also the sandbox. When a peko works for you it can run
shell commands; in a container those commands run as the container's user
and reach only what the container reaches.

You need Docker and a model API key.

## 1. Build the image

From a checkout of this repository:

```bash
docker build -t peko .
```

The first build compiles the runtime and takes a while (about 15 minutes on
a laptop). `docker run --rm peko version` prints the version it built.

## 2. Start the runtime

```bash
docker run -d --name peko \
  -v peko-data:/data \
  -e PEKO_MASTER_PASSPHRASE='choose-a-long-passphrase' \
  --restart unless-stopped \
  peko
```

- **`/data`** holds everything: pekos, conversations, keys. Keep it on a
  volume and back it up.
- **The passphrase** encrypts the keys the runtime stores (model API keys
  and its own identity). A container has no OS keychain, so you supply it.
  Use the same one every time. To keep it out of the command line, mount a
  secret and set `PEKO_MASTER_PASSPHRASE_FILE=/run/secrets/<name>` instead.
- Nothing listens on the network. You talk to the runtime with
  `docker exec`.
- Run one container per `/data` volume. A second runtime on the same
  volume would take the first one's place.

`docker stop peko` shuts the runtime down cleanly; `docker start peko`
brings it back with everything it had.

Check it:

```bash
docker exec peko peko daemon status
```

## 3. Add a model

```bash
docker exec peko peko model add --id main \
  --api-format anthropic_messages --base-url https://api.anthropic.com \
  --model <model name> --context-window 200000 --key "$MODEL_API_KEY"
```

## 4. Create a peko from a seed

A seed is one text file with the peko's definition and the files it starts
with. For a peko that serves other people, the seed should carry its rules
for visitors (`ACCESS.toml`, `POLICY.md`) and what it may tell them (for
example `public/faq.md`). See the
[user guide](../user-guide/USERS_GUIDE.md#accesstoml).

[`examples/seeds/customer-desk.seed.toml`](../../examples/seeds/customer-desk.seed.toml)
is one to start from.

```bash
docker cp desk.seed.toml peko:/seeds/desk.seed.toml
docker exec peko peko create desk --model main --seed /seeds/desk.seed.toml --yes
```

`--yes` accepts what the seed brings. Read the seed first: run the same
command without `--yes` to see its setup steps and files.

Talk to it as its owner:

```bash
docker exec peko peko send desk "What can visitors ask you about?"
```

## 5. Let other people reach it

The runtime has no built-in connection to any chat service. A script of
yours relays each message in with `--as`, naming the sender by whatever you
know them by (a phone number, an email address, a ticket number):

```bash
docker exec peko peko send desk --as 'visitor:+15550100' --name 'Ada' \
  "When are you open?"
```

The reply is printed. The peko answers that person as a visitor, in their
own conversation, with only what `ACCESS.toml` gives visitors. It keeps
what it learns about them in that conversation's own folder.

Anything the peko posts later in that conversation (for example after you
approve a request) can be followed with:

```bash
docker exec peko peko log desk --peer 'visitor:local:+15550100' --watch
```

Your script calls `docker exec` from the host, or runs inside the same
container. The `peko` command in a second container cannot reach the
runtime yet, even with the volume shared.

Only the peko's owner can send as a visitor, which here means anyone who
can run `docker exec` on the container. The runtime records that you
vouched for the sender; it does not check who they are.

## 6. Requests that need you

If a visitor's section in `ACCESS.toml` sets `ask_owner = true`, things it
may not do come to you:

```bash
docker exec peko peko approvals desk
docker exec peko peko approve desk <id>      # add --always to allow it from now on
docker exec peko peko deny desk <id>
```

With `ask_owner = false` (the default), they are refused and nothing waits.

## Updating

Build the new image, then replace the container. The data stays in the
volume.

```bash
docker build -t peko .
docker stop peko && docker rm peko
# run the same `docker run` as in step 2
```

## What the container does and does not protect

- A peko's shell commands run inside the container, as an unprivileged
  user (uid 10001). They cannot reach the host beyond what you mount.
- The container reaches the network. A peko working for you can fetch and
  send what the container can.
- An MCP server you connect holds its own credentials. `ACCESS.toml`
  decides which servers a visitor's conversation may use; it does not limit
  what a server does with its key.
- Run one container per business or per person. Pekos in the same runtime
  share its user and its `/data`.
