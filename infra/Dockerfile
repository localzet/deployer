FROM rust:1.88-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.toml
COPY apps/orchestrator/Cargo.toml apps/orchestrator/Cargo.toml
RUN mkdir -p apps/orchestrator/src && echo "fn main() {}" > apps/orchestrator/src/main.rs
RUN cargo build --release -p cicd-orchestrator || true

COPY apps/orchestrator/src apps/orchestrator/src
COPY web web
RUN cargo build --release -p cicd-orchestrator

FROM docker:cli AS docker-cli

FROM debian:bookworm-slim
ENV DEBIAN_FRONTEND=noninteractive
RUN printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\n' > /etc/apt/apt.conf.d/80-retries \
  && apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates git curl \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=builder /app/target/release/cicd-orchestrator /usr/local/bin/cicd-orchestrator
COPY web /app/web

ENV CICD_CONFIG=/app/config/app.toml
EXPOSE 8080

CMD ["cicd-orchestrator"]
