# Multi-stage pure-Rust build (no native libvips/OpenSSL runtime dependencies).
FROM rust:1.85-slim-bookworm AS builder

WORKDIR /src

# Build dependencies first to maximise Docker layer caching.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY murklet.example.toml ./murklet.example.toml
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home murklet

WORKDIR /app

COPY --from=builder /src/target/release/murklet /usr/local/bin/murklet
COPY --from=builder /src/murklet.example.toml /app/murklet.toml

RUN mkdir -p /app/assets /app/cache && chown -R murklet:murklet /app

USER murklet
EXPOSE 9876

ENTRYPOINT ["murklet"]
CMD ["/app/murklet.toml"]
