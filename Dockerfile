FROM lukemathwalker/cargo-chef:0.1.73-rust-1.91.1-alpine3.22 AS chef
WORKDIR /app
RUN apk update && \
    apk add --no-cache clang lld llvm musl-dev make pkgconfig openssl-dev openssl-libs-static g++ libc-dev

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin noetl-control-plane

FROM alpine:3.22.2 AS runtime
WORKDIR /app
RUN apk add --no-cache libgcc libstdc++ ca-certificates openssl
COPY --from=builder /app/target/release/noetl-control-plane ./noetl-control-plane

ENV NOETL_HOST=0.0.0.0 \
    NOETL_PORT=8082 \
    RUST_LOG=info,noetl_server=debug

EXPOSE 8082

ENTRYPOINT ["./noetl-control-plane"]
