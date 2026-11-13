FROM node:22-bookworm-slim AS node
FROM rust:1.99.1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c

COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && nice -n 10 apt-get update \
 && nice -n 10 apt-get install -y --no-install-recommends \
      python3 python3-psutil git curl ca-certificates pkg-config libssl-dev \
      protobuf-compiler zstd unzip \
 && rm -rf /var/lib/apt/lists/*
# rust-toolchain.toml names these; without them every container's first
# Cargo call in a checkout downloads them, and fails without network.
RUN nice -n 10 rustup component add clippy rustfmt \
 && cargo clippy --version \
 && rustfmt --version

ENV PLAYWRIGHT_BROWSERS_PATH=/opt/playwright-browsers
COPY e2e/terminal-resize/package*.json /opt/terminal-observer/
RUN MAKEFLAGS=-j1 nice -n 10 npm ci --prefix /opt/terminal-observer \
 && nice -n 10 /opt/terminal-observer/node_modules/.bin/playwright install --with-deps chromium

ENV CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0
ENTRYPOINT ["python3"]
