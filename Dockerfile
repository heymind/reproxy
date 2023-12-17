# Start from https://github.com/kpcyrd/mini-docker-rust/blob/main/Dockerfile
FROM rust:alpine
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY ./ /app
RUN cargo build --release
RUN strip target/release/reproxy

FROM alpine
COPY --from=0 /app/target/release/reproxy .
# set the binary as entrypoint
ENTRYPOINT ["/reproxy"]