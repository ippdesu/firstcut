# syntax=docker/dockerfile:1

FROM rust:1-trixie AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY presets ./presets

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin pic_process && \
    cp target/release/pic_process /pic_process

FROM debian:trixie-slim
LABEL org.opencontainers.image.source="https://github.com/ippdesu/firstcut" \
      org.opencontainers.image.description="Local-first photo scoring and review UI"

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates libstdc++6 libgomp1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder --chown=10001:10001 /pic_process /usr/local/bin/pic_process
ENV HOME=/tmp
USER 10001:10001
EXPOSE 8787

ENTRYPOINT ["pic_process"]
CMD ["--help"]
