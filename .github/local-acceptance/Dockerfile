FROM node:22-bookworm-slim AS node
FROM rust:1.99-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6

COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && nice -n 10 apt-get update \
 && nice -n 10 apt-get install -y --no-install-recommends \
      python3 python3-psutil git curl ca-certificates pkg-config libssl-dev \
      protobuf-compiler zstd unzip \
 && rm -rf /var/lib/apt/lists/*

ENV PLAYWRIGHT_BROWSERS_PATH=/opt/playwright-browsers
COPY e2e/terminal-resize/package*.json /opt/terminal-observer/
RUN MAKEFLAGS=-j1 nice -n 10 npm ci --prefix /opt/terminal-observer \
 && nice -n 10 /opt/terminal-observer/node_modules/.bin/playwright install --with-deps chromium

ENV CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0
ENTRYPOINT ["python3"]
