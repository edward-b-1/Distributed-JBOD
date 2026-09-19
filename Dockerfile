# Distributed-JBOD: one image with every binary. The node is configured
# entirely through DJBOD_* environment variables (SPEC 20.6), so no file
# needs to be baked in or mounted; TLS material, when used, is mounted
# and named by DJBOD_TLS_CERT, DJBOD_TLS_KEY, DJBOD_TLS_CA.
#
#   docker build -t djbod .
#   docker run -d --network host -e DJBOD_NODE_ID=... -e DJBOD_ADVERTISE=10.0.0.1:5263 \
#     -v /var/lib/djbod:/var/lib/djbod -v /mnt/disk0:/data/d0 -v /mnt/disk1:/data/d1 djbod
#
# See docs/deployment.md.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --workspace

FROM debian:bookworm-slim
RUN groupadd --system --gid 5263 djbod \
    && useradd --system --uid 5263 --gid 5263 --home-dir /var/lib/djbod --shell /usr/sbin/nologin djbod \
    && mkdir -p /var/lib/djbod /data/d0 /data/d1 /etc/djbod \
    && chown -R djbod:djbod /var/lib/djbod /data
COPY --from=build /src/target/release/djbod-node /src/target/release/djbod \
    /src/target/release/djbod-recover /src/target/release/djbod-ui /usr/local/bin/
COPY deploy/docker/entrypoint.sh /usr/local/bin/djbod-entrypoint
USER djbod
ENV DJBOD_STATE_DIR=/var/lib/djbod \
    DJBOD_LISTEN=0.0.0.0:5263 \
    DJBOD_DEVICES=/data/d0,/data/d1
EXPOSE 5263
VOLUME ["/var/lib/djbod", "/data/d0", "/data/d1"]
ENTRYPOINT ["djbod-entrypoint"]
