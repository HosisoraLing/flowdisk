FROM cloudflare/cloudflared@sha256:89ee50efb1e9cb2ae30281a8a404fed95eb8f02f0a972617526f8c5b417acae2 AS cloudflared
FROM rust:1.88-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY LICENSE THIRD_PARTY_LICENSES.txt ./
COPY src ./src
COPY web ./web
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked && cp target/release/flowdisk /build/flowdisk

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl aria2=1.36.0-1 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g 10001 flowdisk && useradd -r -u 10001 -g flowdisk flowdisk \
    && mkdir /data /downloads && chown flowdisk:flowdisk /data /downloads
COPY --from=builder /build/flowdisk /usr/local/bin/flowdisk
COPY --from=cloudflared /usr/local/bin/cloudflared /usr/local/bin/cloudflared
COPY LICENSE THIRD_PARTY_LICENSES.txt /usr/share/doc/flowdisk/
COPY docs/cloudflared-LICENSE.txt /usr/share/doc/flowdisk/cloudflared-LICENSE.txt
ENV DATA_DIR=/data DOWNLOAD_DIR=/downloads BIND=0.0.0.0:8080
USER 10001:10001
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["flowdisk"]
