# syntax=docker/dockerfile:1.7
# Static musl build of Quelle. The Debian runtime adds CA certificates,
# poppler's pdftotext for PDF text layers and Lightpanda for pages that
# only render with JavaScript (Lightpanda needs glibc).
FROM rust:1.95-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
# Build dependencies in their own layer so source changes reuse the cache.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --locked --release \
    && rm -rf src target/release/quelle target/release/deps/quelle-* target/release/.fingerprint/quelle-*
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
ARG LIGHTPANDA_VERSION=1.0.5
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates poppler-utils \
    && rm -rf /var/lib/apt/lists/*
# Lightpanda is AGPL-3.0 and shipped unmodified as a separate program; its
# licence is kept next to it and its source is at
# https://github.com/lightpanda-io/browser/tree/1.0.5
ADD --chmod=755 \
    --checksum=sha256:0ec21fa4867ab9e82b35285ff4ab0e7336c6c1d89f26fe991f27d73c8a678bd7 \
    https://github.com/lightpanda-io/browser/releases/download/${LIGHTPANDA_VERSION}/lightpanda-x86_64-linux \
    /usr/local/bin/lightpanda
ADD --checksum=sha256:8486a10c4393cee1c25392769ddd3b2d6c242d6ec7928e1414efff7dfb2f07ef \
    https://raw.githubusercontent.com/lightpanda-io/browser/${LIGHTPANDA_VERSION}/LICENSE \
    /usr/share/doc/lightpanda/LICENSE
COPY --from=build /build/target/release/quelle /usr/local/bin/quelle
USER 65532:65532
ENV QUELLE_LISTEN=0.0.0.0:3000 \
    LIGHTPANDA_DISABLE_TELEMETRY=true
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/quelle"]
