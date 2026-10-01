# ---- build stage -----------------------------------------------------------
FROM rust:1.95-bookworm AS builder
WORKDIR /app

# Build dependencies first so they are cached between source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/docsgpt_slack* target/release/deps/libdocsgpt_slack* \
              target/release/docsgpt-slack* target/release/.fingerprint/docsgpt-slack-*

COPY src ./src
RUN touch src/lib.rs src/main.rs && cargo build --release --locked && mkdir -p /app/data

# ---- runtime stage ---------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /app/target/release/docsgpt-slack /usr/local/bin/docsgpt-slack
COPY --from=builder --chown=nonroot:nonroot /app/data /app/data
ENV SQLITE_PATH=/app/data/docsgpt-slack.db \
    RUST_LOG=info
VOLUME ["/app/data"]
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/docsgpt-slack"]
