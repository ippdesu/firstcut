FROM rust:1-trixie@sha256:223cc32ddca8f3ee90c436dbc94b6b5f052ea7f9653c5201e9612f08ad7d21b8 AS builder
WORKDIR /build

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked cargo-auditable@0.7.7

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY presets ./presets

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo auditable build --release --locked --bin pic_process && \
    cp target/release/pic_process /pic_process

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
LABEL org.opencontainers.image.source="https://github.com/ippdesu/firstcut" \
      org.opencontainers.image.description="Local-first photo scoring and review UI"

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates libstdc++6 libgomp1 zlib1g \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder --chown=10001:10001 /pic_process /usr/local/bin/pic_process
ENV HOME=/tmp
USER 10001:10001
EXPOSE 8787

ENTRYPOINT ["pic_process"]
CMD ["--help"]
