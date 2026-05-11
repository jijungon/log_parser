# Stage 1: Build (musl — GLIBC 의존성 없는 정적 바이너리)
FROM rust:1-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    musl-tools \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /build

# 의존성 레이어 캐싱
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release --target x86_64-unknown-linux-musl && \
    rm -f target/x86_64-unknown-linux-musl/release/deps/log_parser*

# 실제 소스 빌드
COPY src ./src
RUN cargo build --release --target x86_64-unknown-linux-musl

# Stage 2: Runtime (정적 바이너리라 minimal image 사용 가능)
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    systemd \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/log_parser /usr/local/bin/log_parser

RUN mkdir -p /run/log_parser /var/lib/log_parser/spool/pending /etc/log_parser

ENTRYPOINT ["/usr/local/bin/log_parser"]
CMD ["/etc/log_parser/agent.yaml"]
