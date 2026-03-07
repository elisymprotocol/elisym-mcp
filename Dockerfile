FROM rust:1.83-bookworm AS builder

WORKDIR /build

# Copy workspace
COPY elisym-core/ elisym-core/
COPY elisym-mcp/ elisym-mcp/

# Build release binary
RUN cd elisym-mcp && cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/elisym-mcp/target/release/elisym-mcp /usr/local/bin/elisym-mcp

ENTRYPOINT ["elisym-mcp"]
