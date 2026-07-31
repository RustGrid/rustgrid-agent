# syntax=docker/dockerfile:1
ARG RUST_BUILDER_IMAGE=rust:1.94-bookworm
ARG NODE_RUNTIME_IMAGE=node:24-trixie-slim
FROM ${RUST_BUILDER_IMAGE} AS builder
WORKDIR /source
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY contracts/impact-map-v2.schema.json ./contracts/impact-map-v2.schema.json
COPY src ./src
COPY README.md LICENSE ./
RUN cargo build --locked --release

FROM ${NODE_RUNTIME_IMAGE} AS runtime
ARG CODEX_VERSION=0.144.4
ARG NPM_VERSION=12.0.1
ARG NPM_BRACE_EXPANSION_VERSION=5.0.8
ARG NPM_TAR_VERSION=7.5.22
RUN test -n "${CODEX_VERSION}" \
    && test -n "${NPM_VERSION}" \
    && test -n "${NPM_BRACE_EXPANSION_VERSION}" \
    && test -n "${NPM_TAR_VERSION}" \
    && apt-get update \
    && apt-get upgrade --no-install-recommends -y \
    && apt-get install --no-install-recommends -y ca-certificates git tini \
    && npm install --global "npm@${NPM_VERSION}" \
    && npm install --global "@openai/codex@${CODEX_VERSION}" \
    && npm install --prefix /tmp/npm-security-patches --ignore-scripts --no-audit --no-fund \
      "brace-expansion@${NPM_BRACE_EXPANSION_VERSION}" \
      "tar@${NPM_TAR_VERSION}" \
    && rm -rf \
      /usr/local/lib/node_modules/npm/node_modules/brace-expansion \
      /usr/local/lib/node_modules/npm/node_modules/tar \
    && cp -a \
      /tmp/npm-security-patches/node_modules/brace-expansion \
      /usr/local/lib/node_modules/npm/node_modules/brace-expansion \
    && cp -a \
      /tmp/npm-security-patches/node_modules/tar \
      /usr/local/lib/node_modules/npm/node_modules/tar \
    && test "$(node -p "require('/usr/local/lib/node_modules/npm/node_modules/brace-expansion/package.json').version")" = "${NPM_BRACE_EXPANSION_VERSION}" \
    && test "$(node -p "require('/usr/local/lib/node_modules/npm/node_modules/tar/package.json').version")" = "${NPM_TAR_VERSION}" \
    && rm -rf /tmp/npm-security-patches \
    && npm cache clean --force \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 65532 rustgrid-agent \
    && useradd --uid 65532 --gid 65532 --home-dir /var/lib/rustgrid-agent --create-home --shell /usr/sbin/nologin rustgrid-agent \
    && install -d -o rustgrid-agent -g rustgrid-agent /var/lib/rustgrid-agent/workspaces /etc/rustgrid-agent

COPY --from=builder /source/target/release/rustgrid-agent /usr/local/bin/rustgrid-agent

USER 65532:65532
WORKDIR /var/lib/rustgrid-agent
ENV RUSTGRID_AGENT_LOG=json
VOLUME ["/var/lib/rustgrid-agent/workspaces"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/rustgrid-agent"]
CMD ["--config", "/etc/rustgrid-agent/agent.json", "watch", "--once"]
HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
  CMD ["/usr/local/bin/rustgrid-agent", "--config", "/etc/rustgrid-agent/agent.json", "status", "--json"]
