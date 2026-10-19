FROM rust:1.98-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home bridge
COPY --from=build /src/target/release/rakazo-beeper /usr/local/bin/rakazo-beeper
USER 10001:10001
WORKDIR /bridge
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s \
    CMD curl --fail --silent http://127.0.0.1:8080/readyz || exit 1
ENTRYPOINT ["rakazo-beeper"]
CMD ["--config", "/bridge/config.toml", "run"]
