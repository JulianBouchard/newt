# One image with every newt binary: newt, newt-report, newt-replay,
# newt-demo and newt-loadtest. `docker compose up` runs the demo with it.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p newt-server -p newt-demo -p newt-loadtest \
    && mkdir /out \
    && cp target/release/newt target/release/newt-report target/release/newt-replay \
          target/release/newt-demo target/release/newt-loadtest /out/

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /usr/local/bin/
ENV NEWT_CONFIG=/etc/newt/server.toml
EXPOSE 7878
CMD ["newt"]
