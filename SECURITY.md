# Security

## Reporting a vulnerability

Please report security problems privately, not in a public issue.

Use **Report a vulnerability** on the repository's
[Security tab](https://github.com/ConekoAI/Peko/security/advisories/new).
Say what you found, how to reproduce it, and which version you ran
(`peko version`).

We will confirm we received it, tell you what we found, and credit you in
the release notes unless you would rather we did not.

## Supported versions

Fixes go into the latest release. `peko update` moves you to it.

## What counts

The [security model](docs/SECURITY_MODEL.md) says what Peko protects and
where that stops. Reports we especially want:

- a visitor reaching a tool, file or conversation the owner did not list;
- a message deciding an approval, or passing as the owner;
- PekoHub, or anyone on the network, reading or altering sealed messages;
- a tool call that runs without a ledger entry;
- secrets reaching a model, a log or another conversation.

The model lists limits we already know about, for example that work done
for the owner runs as the owner's operating-system user. Those are not
vulnerabilities, though ideas for narrowing them are welcome as issues.
