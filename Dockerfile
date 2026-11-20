FROM rust:1.98-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
ENV SQLX_OFFLINE=true
RUN cargo build --release --locked -p tgm

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 tgm \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin tgm

COPY --from=builder --chown=10001:10001 /src/target/release/tgm /usr/local/bin/tgm

USER 10001:10001
ENTRYPOINT ["/usr/local/bin/tgm"]
