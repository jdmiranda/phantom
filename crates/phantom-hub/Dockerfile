# syntax=docker/dockerfile:1
#
# Multi-stage Dockerfile for phantom-hub.
#
# Stage 1: dependency caching via cargo-chef
#   Caches the full workspace dep compile so that subsequent builds only
#   recompile changed crates.
#
# Stage 2: application build
#   Inherits the dep cache from stage 1 and builds phantom-hub --release.
#
# Stage 3: minimal runtime image
#   Copies only the compiled binary into a distroless/debian-slim image.
#
# Railway auto-detects this Dockerfile. The START_COMMAND in railway.toml
# overrides CMD if needed.

# ── Stage 1: chef planner ────────────────────────────────────────────────────
FROM rust:1.87-slim AS planner
WORKDIR /app

RUN cargo install cargo-chef --locked

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 2: dependency cook ─────────────────────────────────────────────────
FROM rust:1.87-slim AS cook
WORKDIR /app

RUN cargo install cargo-chef --locked

COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# ── Stage 3: builder ─────────────────────────────────────────────────────────
FROM rust:1.87-slim AS builder
WORKDIR /app

# Copy pre-built deps from cook stage.
COPY --from=cook /app/target target
COPY --from=cook /usr/local/cargo /usr/local/cargo

# Copy full source tree.
COPY . .

# Build only phantom-hub (avoids GPU crates that need X11/Vulkan headers).
RUN cargo build --release -p phantom-hub

# ── Stage 4: runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Non-root user for principle of least privilege.
RUN addgroup --system phantom && adduser --system --ingroup phantom phantom
USER phantom

WORKDIR /app

COPY --from=builder /app/target/release/phantom-hub /app/phantom-hub

# Railway injects PORT; default to 8080 for local testing.
ENV PORT=8080
EXPOSE 8080

CMD ["/app/phantom-hub"]
