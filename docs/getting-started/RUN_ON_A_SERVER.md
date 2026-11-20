# Run a peko on a server

This runs the Peko runtime in a Docker container, with no desktop app: a
peko that is always on, that you manage from the command line, and that
other people reach through a script you write (a chat service, email, a
web form).

The container is also the sandbox. When a peko works for you it can run
shell commands; in a container those commands run as the container's user
and reach only what the container reaches.

You need Docker and a model API key.

## 1. Get the image

Each release is published as `ghcr.io/conekoai/peko`, for amd64 and arm64:

```bash
docker pull ghcr.io/conekoai/peko:0.3
```

Tags: the exact version (`0.3.1`), the version line (`0.3`, which follows
its newest patch) and `latest`. `docker run --rm ghcr.io/conekoai/peko:0.3
version` prints the version.

To build it yourself instead, from a checkout of this repository:

```bash
docker build -t ghcr.io/conekoai/peko:0.3 .
```

The first build compiles the runtime and takes a while (about 15 minutes on
a laptop).

## 2. Start the runtime

```bash
docker run -d --name peko \
  -v peko-data:/data \
  -e PEKO_MASTER_PASSPHRASE='choose-a-long-passphrase' \
  --restart unless-stopped \
  ghcr.io/conekoai/peko:0.3
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
docker exec peko peko engine status
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

The reply is printed.

The peko answers that person as a visitor, in their own conversation, with
only what `ACCESS.toml` gives visitors. It keeps what it learns about them
in that conversation's own folder.

### Messages the peko sends later

Sometimes the peko writes again after its reply: after you approve a
request, or when a check-in comes due. Nothing is waiting for those. A
bridge follows them with:

```bash
docker exec peko peko log desk --sent --json
```

It prints one line for each post the peko makes, in any one-to-one conversation:

```json
{"to":"visitor:local:+15550100","channel":"dm_…","message":{"id":"msg_…","text":"…", …}}
```

`to` is whom to deliver it to: `visitor:local:<id>` for the id you sent
with `--as visitor:<id>`, and `user:local` for your own conversation. The replies
`peko send` prints show up here too, so a bridge that follows `--sent`
can deliver everything from it and skip the replies `peko send` prints;
`message.id` stays the same for the same post. After a restart, `--since 10m`
first repeats the posts of the last ten minutes. Only the peko's owner can
follow everything it posts.

### Run the bridge in its own container

The bridge can run in a container of its own, sharing the runtime's
volume. It needs nothing else: no passphrase, and no network between
the two.

```bash
docker run --rm --volumes-from peko --entrypoint peko \
  ghcr.io/conekoai/peko:0.3 send desk --as 'visitor:+15550100' "Hello"
```

Build your bridge on this image (or copy `/opt/peko/bin/peko` into yours)
and mount the same `/data`, running as the same user (uid 10001). The
command reaches the runtime through the socket in `/data/run`, and gets its
answer back there. Run only `peko send`, `peko log` and other commands from it; the engine itself
runs in one container per volume.

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

Pull the new image, then replace the container. The data stays in the
volume.

```bash
docker pull ghcr.io/conekoai/peko:0.3
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
