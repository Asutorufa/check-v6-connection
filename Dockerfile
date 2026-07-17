FROM rust:1-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY openwrt ./openwrt
RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --no-install-recommends -y \
        ca-certificates \
        iputils-ping \
        iproute2 \
        openssh-client \
        sshpass \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 10001 ipv6-watch \
    && useradd --system --uid 10001 --gid 10001 \
        --home-dir /var/lib/openwrt-ipv6-watch \
        --create-home --shell /usr/sbin/nologin ipv6-watch \
    && install -d -o ipv6-watch -g ipv6-watch -m 0750 \
        /etc/openwrt-ipv6-watch

COPY --from=builder /build/target/release/openwrt-ipv6-watch \
    /usr/local/bin/openwrt-ipv6-watch

ENV RUST_LOG=info
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/openwrt-ipv6-watch"]
CMD ["/etc/openwrt-ipv6-watch/config.toml"]
