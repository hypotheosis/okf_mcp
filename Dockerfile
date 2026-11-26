# syntax=docker/dockerfile:1
# The okf-mcp server image (M7). Built by .github/workflows/image.yml and
# published to ghcr.io/hypotheosis/okf-mcp; see docs/operations.md.

# Build stages: the same Rust as the workstation's toolchain (1.99.1). Bump
# both together, and Dockerfile.maintenance with them.
# Base images go by tag, not digest, on purpose: rebuilds get Debian's security
# fixes. A pushed release keeps its digest; never rebuild an old tag.
#
# cargo-chef splits the compile in two layers: `cook` builds only the
# dependencies, keyed on recipe.json (the manifests and Cargo.lock), and the
# final `cargo build` compiles this crate. CI runners start cold, but
# image.yml keeps layers in the GitHub Actions cache, so a commit that leaves
# the manifests alone reuses the cooked layer and compiles only okf-mcp. No
# cache mounts here: they are not saved between runners, and a target/ mount
# would keep the cooked dependencies out of the layer that is cached.
FROM rust:1.99.1-slim-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.78
WORKDIR /build

# recipe.json changes only when a manifest or Cargo.lock does, so the cook
# layer below stays cached across source-only commits.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --bin okf-mcp --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --bin okf-mcp \
    && cp target/release/okf-mcp /usr/local/bin/okf-mcp

# Runtime stage: no git, and the binary links no OpenSSL (sqlx uses rustls).
# CA certificates for completeness; that package pulls in Debian's openssl
# tools, which the server never runs. A fixed, non-root uid that manifests
# can name.
FROM debian:bookworm-slim
# The official MCP Registry checks this label against server.json's name.
LABEL io.modelcontextprotocol.server.name="io.github.hypotheosis/okf-mcp"
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin okf
COPY --from=build /usr/local/bin/okf-mcp /usr/local/bin/okf-mcp
USER 10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/okf-mcp"]
CMD ["serve"]
