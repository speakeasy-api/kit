# syntax=docker/dockerfile:1

# Published flavors are built from the named final stages: bookworm, slim,
# and alpine. The unqualified image uses the final `default` stage, which
# intentionally aliases slim.
ARG RUST_VERSION=1.94.0
ARG DEBIAN_SUITE=bookworm
ARG ALPINE_VERSION=3.23
ARG VERSION=source
ARG REVISION=unknown

FROM rust:${RUST_VERSION}-slim-${DEBIAN_SUITE} AS builder-gnu

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
ARG TARGETARCH

COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY src ./src
COPY docs/user ./docs/user

RUN --mount=type=cache,id=kit-cargo-registry,sharing=locked,target=/usr/local/cargo/registry \
    --mount=type=cache,id=kit-gnu-target-${TARGETARCH},sharing=locked,target=/src/target \
    cargo build --locked --release --bin kit \
    && strip --strip-unneeded target/release/kit \
    && cp target/release/kit /kit

# Alpine is a separate musl build. Do not copy the glibc artifact above into
# this stage or add a libc compatibility shim to the runtime image.
FROM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS builder-musl

RUN apk add --no-cache \
        build-base \
        cmake \
        linux-headers \
        perl \
        pkgconf

WORKDIR /src
ARG TARGETARCH

COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY src ./src
COPY docs/user ./docs/user

RUN --mount=type=cache,id=kit-cargo-registry,sharing=locked,target=/usr/local/cargo/registry \
    --mount=type=cache,id=kit-musl-target-${TARGETARCH},sharing=locked,target=/src/target \
    cargo build --locked --release --bin kit \
    && strip --strip-unneeded target/release/kit \
    && cp target/release/kit /kit

FROM debian:${DEBIAN_SUITE} AS bookworm

ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.title="Kit" \
      org.opencontainers.image.description="Coding agent runtime and terminal client" \
      org.opencontainers.image.source="https://github.com/speakeasy-api/kit" \
      org.opencontainers.image.url="https://github.com/speakeasy-api/kit" \
      org.opencontainers.image.documentation="https://github.com/speakeasy-api/kit#readme" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 kit \
    && useradd --uid 1000 --gid 1000 --create-home --no-log-init --shell /bin/sh kit \
    && mkdir -p /workspace \
    && chown kit:kit /workspace

COPY --from=builder-gnu /kit /usr/local/bin/kit

ENV HOME=/home/kit
WORKDIR /workspace
USER kit

EXPOSE 8081
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/kit"]
CMD ["--help"]

FROM debian:${DEBIAN_SUITE}-slim AS slim

ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.title="Kit" \
      org.opencontainers.image.description="Coding agent runtime and terminal client" \
      org.opencontainers.image.source="https://github.com/speakeasy-api/kit" \
      org.opencontainers.image.url="https://github.com/speakeasy-api/kit" \
      org.opencontainers.image.documentation="https://github.com/speakeasy-api/kit#readme" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 kit \
    && useradd --uid 1000 --gid 1000 --create-home --no-log-init --shell /bin/sh kit \
    && mkdir -p /workspace \
    && chown kit:kit /workspace

COPY --from=builder-gnu /kit /usr/local/bin/kit

ENV HOME=/home/kit
WORKDIR /workspace
USER kit

# Remote ACP examples conventionally bind this port. Kit does not open a
# listener unless the selected command asks it to.
EXPOSE 8081
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/kit"]
CMD ["--help"]

FROM alpine:${ALPINE_VERSION} AS alpine

ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.title="Kit" \
      org.opencontainers.image.description="Coding agent runtime and terminal client" \
      org.opencontainers.image.source="https://github.com/speakeasy-api/kit" \
      org.opencontainers.image.url="https://github.com/speakeasy-api/kit" \
      org.opencontainers.image.documentation="https://github.com/speakeasy-api/kit#readme" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

RUN apk add --no-cache ca-certificates git \
    && addgroup -S -g 1000 kit \
    && adduser -S -D -u 1000 -G kit -h /home/kit -s /bin/sh kit \
    && mkdir -p /workspace \
    && chown kit:kit /workspace

COPY --from=builder-musl /kit /usr/local/bin/kit

ENV HOME=/home/kit
WORKDIR /workspace
USER kit

EXPOSE 8081
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/kit"]
CMD ["--help"]

# Keep the unqualified image on the conservative glibc/slim flavor.
FROM slim AS default
