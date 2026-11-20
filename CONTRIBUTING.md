# Contributing to Peko

Thanks for helping. Bug reports, fixes, docs and translations are all
welcome.

## Before you start

- **A bug or a small fix:** open an issue or go straight to a pull request.
- **A new feature or a change in behavior:** open an issue first, so we can
  agree on the shape before you write it.
- **A security problem:** do not open an issue. See [SECURITY.md](SECURITY.md).

## Build and test

Peko is written in Rust (stable).

```bash
git clone https://github.com/ConekoAI/Peko
cd Peko
cargo build --release -p peko-cli -p peko-daemon   # target/release/peko and peko-daemon
```

Keep the two binaries in the same directory: `peko` starts the engine from
the `peko-daemon` beside it.

The checks CI runs, none of which need Docker:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib
python3 scripts/check_workspace_deps.py
bash scripts/check_module_boundaries.sh
```

Tests that need a model use a mock server in Docker
(`make docker-up && make test-integration && make docker-down`).
[docs/development/CI.md](docs/development/CI.md) lists every tier.

## Finding your way around

[How Peko works](docs/HOW_IT_WORKS.md) maps the crates. Two scripts keep the
dependency graph honest: small shared crates depend on nothing else in the
workspace, and no crate depends back on `core`. If a check fails, the
message says which edge to remove.

## Pull requests

- One change per pull request, with a test that fails without it.
- Tests must not touch your real keychain or `~/.peko`: use the isolated
  fixtures the existing tests use.
- Add a line under `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md), written
  for someone who uses Peko.
- If you change a command, regenerate the CLI reference:
  `python3 scripts/gen_cli_reference.py target/debug/peko > docs/user-guide/CLI_REFERENCE.md`.
- If you change a file format or a wire type, update
  [docs/reference/DATA_MODEL.md](docs/reference/DATA_MODEL.md). A change that
  breaks one goes out in a minor version, not a patch.

## The desktop app

Peko Desktop lives in [ConekoAI/peko-desktop](https://github.com/ConekoAI/peko-desktop).
App bugs and translations go there.

## License

By contributing you agree that your work is released under the
[MIT License](LICENSE).
