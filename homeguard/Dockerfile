# Multi-stage build: compile in the full Rust image, ship only the binary in
# a slim Debian image, running as an unprivileged user.
FROM rust:1.99.1-slim-trixie AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked && strip target/release/homeguard

FROM debian:trixie-20261020-slim
# ca-certificates: TLS to the Telegram API. iputils-ping: presence probes.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates iputils-ping \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /var/lib/homeguard --shell /usr/sbin/nologin homeguard \
    && mkdir -p /var/lib/homeguard /etc/homeguard \
    && chown homeguard:homeguard /var/lib/homeguard
COPY --from=build /src/target/release/homeguard /usr/local/bin/homeguard
USER homeguard
WORKDIR /var/lib/homeguard
ENTRYPOINT ["homeguard"]
CMD ["run", "--config", "/etc/homeguard/homeguard.toml"]
