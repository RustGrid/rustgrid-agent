# syntax=docker/dockerfile:1
ARG RUST_BUILDER_IMAGE=rust:1.94-bookworm
ARG NODE_RUNTIME_IMAGE=node:24-bookworm-slim
FROM ${RUST_BUILDER_IMAGE} AS builder
WORKDIR /source
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY README.md LICENSE ./
RUN cargo build --locked --release

FROM ${NODE_RUNTIME_IMAGE} AS runtime
ARG CODEX_VERSION
RUN test -n "${CODEX_VERSION}" \
    && apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates git tini \
    && npm install --global "@openai/codex@${CODEX_VERSION}" \
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
