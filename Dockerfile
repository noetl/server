FROM lukemathwalker/cargo-chef:0.1.73-rust-1.91.1-alpine3.22 AS chef
WORKDIR /app
RUN apk update && \
    apk add --no-cache clang lld llvm musl-dev make pkgconfig openssl-dev openssl-libs-static g++ libc-dev

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build deps for the recipe + the orchestrate-shadow feature (wasmtime) so the
# cook layer caches them (noetl/ai-meta#108 slice 4).
RUN cargo chef cook --release --features orchestrate-shadow --recipe-path recipe.json
COPY . .
# Built with the orchestrate-shadow feature so the image can run the in-server
# plug-in shadow; the behaviour is still gated at runtime by
# NOETL_ORCHESTRATE_PLUGIN_SHADOW (default off).
RUN cargo build --release --features orchestrate-shadow --bin noetl-control-plane

# Build the built-in system plug-ins to wasm32 (noetl/ai-meta#108 slice 3).
# The `plugins/orchestrate` crate is excluded from the server workspace, so it is
# built explicitly; the artifact is baked into the runtime image + seeded into
# the plug-in registry on boot.
FROM chef AS wasmbuilder
RUN rustup target add wasm32-unknown-unknown
COPY . .
RUN cargo build --release --target wasm32-unknown-unknown \
        --manifest-path plugins/orchestrate/Cargo.toml

FROM alpine:3.22.2 AS runtime
WORKDIR /app
RUN apk add --no-cache libgcc libstdc++ ca-certificates openssl
COPY --from=builder /app/target/release/noetl-control-plane ./noetl-control-plane
# Built-in system plug-ins, seeded into noetl.plugin_module on boot. The file
# stem becomes the catalog path: orchestrate.wasm -> system/orchestrate@1.
COPY --from=wasmbuilder \
    /app/plugins/orchestrate/target/wasm32-unknown-unknown/release/noetl_orchestrate_plugin.wasm \
    /opt/noetl/plugins/orchestrate.wasm

ENV NOETL_HOST=0.0.0.0 \
    NOETL_PORT=8082 \
    NOETL_SYSTEM_PLUGIN_DIR=/opt/noetl/plugins \
    RUST_LOG=info,noetl_server=debug

EXPOSE 8082

ENTRYPOINT ["./noetl-control-plane"]
