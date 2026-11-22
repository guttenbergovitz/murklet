# Multi-stage pure-Rust build (no native libvips/OpenSSL runtime dependencies).
FROM rust:1.88-slim-bookworm AS builder

WORKDIR /src

# Build dependencies first to maximise Docker layer caching. Cargo validates that every
# [[bench]] target declared in Cargo.toml exists, so empty stubs are created and kept for
# the final build as well (benchmarks are never compiled by `cargo build --release`).
COPY Cargo.toml Cargo.lock ./
RUN mkdir src benches \
    && printf 'fn main() {}\n' > src/main.rs \
    && touch benches/engine_resize.rs benches/cache_evict.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY murklet.example.toml ./murklet.example.toml
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="Murklet" \
      org.opencontainers.image.description="Image proxy and on-demand image transformation service written in Rust." \
      org.opencontainers.image.url="https://github.com/guttenbergovitz/murklet" \
      org.opencontainers.image.source="https://github.com/guttenbergovitz/murklet" \
      org.opencontainers.image.documentation="https://github.com/guttenbergovitz/murklet/blob/main/docs/operations.md" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home murklet

WORKDIR /app

COPY --from=builder /src/target/release/murklet /usr/local/bin/murklet
COPY --from=builder /src/murklet.example.toml /app/murklet.toml
COPY LICENSE-MIT LICENSE-APACHE /usr/share/doc/murklet/

# The example binds to loopback, which is unreachable through a published container port.
RUN sed -i 's|127\.0\.0\.1:9876|0.0.0.0:9876|' /app/murklet.toml \
    && grep -q '0\.0\.0\.0:9876' /app/murklet.toml

RUN mkdir -p /app/assets /app/cache && chown -R murklet:murklet /app

USER murklet
EXPOSE 9876

ENTRYPOINT ["murklet"]
CMD ["/app/murklet.toml"]
