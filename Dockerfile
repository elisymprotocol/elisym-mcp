FROM rust:1.86-bookworm AS builder

WORKDIR /build

# Copy source
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

# Build release binary with HTTP transport
RUN cargo build --release --features transport-http

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/elisym-mcp /usr/local/bin/elisym-mcp

# Default: stdio transport (MCP standard)
# For HTTP: docker run -p 8080:8080 elisymprotocol/elisym-mcp --http --host 0.0.0.0
EXPOSE 8080
ENTRYPOINT ["elisym-mcp"]
