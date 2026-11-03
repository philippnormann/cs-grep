# syntax=docker/dockerfile:1
# Stage 1: Build all Rust binaries
FROM docker.io/library/rust:1-slim-bookworm AS rust-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN cargo build --locked --release -p cs-grep -p cs-grep-web

# Stage 2: Runtime image with Rust binaries only
FROM docker.io/library/debian:bookworm-slim AS runner
RUN apt-get update && apt-get install -y --no-install-recommends \
    sqlite3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=rust-builder /src/target/release/cs-grep /usr/local/bin/cs-grep
COPY --from=rust-builder /src/target/release/cs-grep-web /usr/local/bin/cs-grep-web
VOLUME ["/data"]
EXPOSE 5002
CMD ["cs-grep-web", "--host", "0.0.0.0", "--port", "5002", "--db", "/data/papers.db", "--config", "/data/config.yaml"]
