# Peko runtime: the daemon and the CLI, for running pekos on a server.
#
#   docker build -t peko .
#   docker run -d --name peko -v peko-data:/data \
#     -e PEKO_MASTER_PASSPHRASE=... peko
#   docker exec peko peko create my-peko --model <id> --seed /seeds/my.seed.toml --yes
#
# See docs/getting-started/RUN_ON_A_SERVER.md.
#
# The container is the sandbox: a peko's owner turns run shell commands as
# the container's user and reach only what the container reaches.

FROM rust:1.96-slim-bookworm AS build
WORKDIR /src
# Only what the build reads, so editing docs or the container scripts
# does not rebuild the binaries.
COPY Cargo.toml Cargo.lock ./
COPY peko-rs peko-rs
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p peko-cli -p peko-daemon \
    && mkdir -p /out \
    && cp target/release/peko target/release/peko-daemon /out/ \
    && strip /out/peko /out/peko-daemon

FROM debian:bookworm-slim
# What a peko's own work usually needs: a shell, git, curl, and TLS roots.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash git curl tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home --shell /bin/bash peko \
    && mkdir -p /data /seeds /opt/peko/bin \
    && chown peko:peko /data /seeds

# The binaries sit together (the CLI starts the daemon it finds beside
# it); `peko` on the PATH is a wrapper that reads the passphrase.
COPY --from=build /out/peko /out/peko-daemon /opt/peko/bin/
COPY docker/peko /usr/local/bin/peko
COPY docker/entrypoint.sh /usr/local/bin/peko-entrypoint
RUN chmod 0755 /usr/local/bin/peko /usr/local/bin/peko-entrypoint

USER peko
WORKDIR /home/peko
# All runtime state (pekos, conversations, keys, the vault) is under
# /data. There is no OS keychain in a container, so the vault is locked
# with a passphrase: PEKO_MASTER_PASSPHRASE, or a file named by
# PEKO_MASTER_PASSPHRASE_FILE (a Docker or Kubernetes secret).
ENV PEKO_HOME=/data \
    PEKO_UNLOCK_METHOD=passphrase \
    RUST_LOG=info
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
    CMD peko daemon status >/dev/null 2>&1 || exit 1

ENTRYPOINT ["tini", "--", "peko-entrypoint"]
