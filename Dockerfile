# syntax=docker/dockerfile:1
FROM rust:1-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app

# Cache dependencies first
COPY Cargo.toml ./
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Build the actual binary
COPY src ./src
RUN cargo build --release

FROM scratch

COPY --from=builder /app/target/release/canned /canned

ENV CANNED_CONFIG=/etc/canned/canned.yaml
EXPOSE 7890
ENTRYPOINT ["/canned"]
