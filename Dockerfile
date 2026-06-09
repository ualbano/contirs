FROM rust:alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app

# Dependency layer — cached as long as Cargo.toml and Cargo.lock are unchanged.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch main.rs so cargo sees a source change and rebuilds the real binary.
RUN touch src/main.rs && cargo build --release


FROM alpine:3

RUN apk add --no-cache tzdata

COPY --from=builder /app/target/release/conti /usr/local/bin/conti
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
