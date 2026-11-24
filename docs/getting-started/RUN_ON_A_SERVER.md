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
  `docker exec`, and your service talks to it over HTTP once you turn
  that on (step 5).
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

The runtime has no built-in connection to any chat service. A service of
yours (a help desk, a web app, a script reading email) relays each message
in, naming the sender by whatever you know them by: a phone number, an
email address, a ticket number. The peko answers that person as a visitor,
in their own conversation, with only what `ACCESS.toml` gives visitors. It
keeps what it learns about them in that conversation's own folder.

The service reaches the runtime over HTTP with a key. The command line
still works too (see [the end of this step](#or-use-the-command-line)).

### Turn on HTTP

The runtime answers over HTTP only when you say so, and only to a caller
with an API key. Turn on keys and give the address to listen on:

```bash
docker exec peko peko config set enable_api_key true --file /data/runtime/auth_config.toml
docker exec peko peko config set network.http 0.0.0.0:11436 --file /data/runtime.toml
docker exec peko peko config set network.http_hosts '["peko:11436"]' --file /data/runtime.toml
docker restart peko
```

- **`http`** is the address and port. `0.0.0.0` lets other containers
  reach it; `127.0.0.1` keeps it to programs in the same container.
- **`http_hosts`** are the names your service calls the runtime by
  (`http://peko:11436` here). For any address but `127.0.0.1` the runtime
  will not start without it, and it refuses requests sent to any other
  name.
- The runtime will not start with `http` set and API keys off.

Make a key for your service:

```bash
docker exec peko peko auth api-key create --name helpdesk --scopes owner
```

It prints the key once. An `owner` key acts as the peko's owner: whoever
holds it can send as any visitor, approve requests, read every
conversation and change every setting. Keep it like a root password: in
your service's secret store, never in an image or a repository. List keys
with `peko auth api-key list` and revoke one with
`peko auth api-key revoke <key id>`; a revoked key stops working at once.
Keys are made and revoked only from the command line, never over HTTP.

Put the two containers on one Docker network, and do not publish the port
(no `-p 11436:11436`). The connection is not encrypted; it is meant for
the private network between your service and the runtime, not the
internet.

```bash
docker network create peko-net
docker network connect peko-net peko
docker run -d --name helpdesk --network peko-net -e PEKO_KEY=... your-service
```

### Send a visitor's message

Every request is one JSON object, `POST`ed to `/v1/call`, with the key in
the `Authorization` header (never in the URL):

```bash
curl -s http://peko:11436/v1/call \
  -H "Authorization: Bearer $PEKO_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"type": "principal_send", "name": "desk",
       "message": "When are you open?",
       "send_as": {"actor": "visitor:+15550100", "name": "Ada"}}'
```

The answer is the runtime's own reply packet (here `principal_sent`, with
the peko's answer). A request that answers with several packets (a
streamed reply, a watch) comes back as `text/event-stream`, one
`data: <packet>` line per packet; send `Accept: text/event-stream` to
always get that, which you should for anything that runs a long time.
`principal_send_stream` streams the reply as it is written.

The requests are the ones Peko Desktop and the command line send: the
`RequestPacket` types in
[`packet.rs`](../../peko-rs/core/src/ipc/packet.rs). A packet is at most
60,000 bytes. The runtime's own errors come back as an `error` packet with
status 200; status 4xx means the request did not get in (no key or a bad
one, too large, not JSON, too many requests: 100 a minute per key by
default, set in `auth_config.toml` under `[rate_limit]`).

Everything a key does is recorded in the audit log under the key's id
(`peko audit`).

### Files

Files go beside the requests, as bytes. Send one in, then name it in a
message:

```bash
curl -s "http://peko:11436/v1/files?name=receipt.pdf" \
  -H "Authorization: Bearer $PEKO_KEY" \
  -H 'Content-Type: application/octet-stream' --data-binary @receipt.pdf
# {"file":"upload:3f9c…","name":"receipt.pdf","size":48213}

curl -s http://peko:11436/v1/call \
  -H "Authorization: Bearer $PEKO_KEY" -H 'Content-Type: application/json' \
  -d '{"type": "principal_send", "name": "desk", "message": "My receipt",
       "attachments": ["upload:3f9c…"],
       "send_as": {"actor": "visitor:+15550100"}}'
```

A file is at most 20 MB, and a message carries at most 8. The message
copies the file into the peko's workspace and the upload is gone; one no
message used is deleted after an hour, and when the runtime restarts.

A post the peko makes with a file names it in `attachments`, with a
`path`. Fetch it with that path:

```bash
curl -s -G http://peko:11436/v1/files -H "Authorization: Bearer $PEKO_KEY" \
  --data-urlencode "path=/data/pekos/…/attachments/2026-11-24/ab12cd34-chart.png" -o chart.png
```

Only files in a peko's `attachments` folders come back this way. A file
that came from someone on another computer has a `sha256` instead; fetch
it with `?channel=<the conversation's id>&sha256=<digest>`. Both need an
`owner` key.

### Messages the peko sends later

Sometimes the peko writes again after its reply: after you approve a
request, or when a check-in comes due. Nothing is waiting for those. A
service follows them with one long request:

```bash
curl -sN http://peko:11436/v1/call \
  -H "Authorization: Bearer $PEKO_KEY" \
  -H 'Content-Type: application/json' -H 'Accept: text/event-stream' \
  -d '{"type": "principal_sent_watch", "name": "desk", "since": "2026-11-24T09:00:00Z"}'
```

It sends one `principal_sent_appended` packet for each post the peko makes,
in any one-to-one conversation, with `heartbeat` packets between:

```json
{"type":"principal_sent_appended","to":"visitor:local:+15550100","channel":"dm_…","message":{"id":"msg_…","text":"…", …}}
```

`to` is whom to deliver it to: `visitor:local:<id>` for the id you sent
with `visitor:<id>`, and `user:local` for your own conversation. The
replies `principal_send` returns show up here too, so a service that
follows this can deliver everything from it; `message.id` stays the same
for the same post. After a restart, `since` first repeats the posts newer
than it.

### Or use the command line

Without HTTP, a script relays a message with `peko send --as`:

```bash
docker exec peko peko send desk --as 'visitor:+15550100' --name 'Ada' \
  "When are you open?"
```

The reply is printed. `docker exec peko peko log desk --sent --json`
prints one line for each post the peko makes, as the watch above does.
`docker exec` needs access to the Docker socket, which is root on the host.

The script can also run in a container of its own that shares the
runtime's volume, with no passphrase and no network between the two:

```bash
docker run --rm --volumes-from peko --entrypoint peko \
  ghcr.io/conekoai/peko:0.3 send desk --as 'visitor:+15550100' "Hello"
```

Build it on this image (or copy `/opt/peko/bin/peko` into yours), mount
the same `/data` and run as the same user (uid 10001). The command
reaches the runtime through the socket in `/data/run`. That container sees
everything in `/data`: the vault, every conversation, the keys. HTTP with
a key gives a service the same reach without the volume.

Only the peko's owner can send as a visitor: anyone holding an `owner`
key, or who can run `docker exec` on the container. The runtime records
that you vouched for the sender; it does not check who they are.

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
