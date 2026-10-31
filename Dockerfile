# syntax=docker/dockerfile:1

# Pinned to bookworm so the glibc here matches the torrential build stage.
# Node 24 is pinned explicitly: the moving `lts` tag is now Node 25, which no
# longer bundles corepack, and the base stage runs `corepack enable`.
FROM node:24-bookworm-slim AS base
ENV PNPM_HOME="/pnpm"
ENV PATH="$PNPM_HOME:$PATH"
RUN corepack enable
WORKDIR /app

## so corepack knows pnpm's version
COPY . .
## prevent prompt to download
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0
## setup for offline
RUN corepack pack
## don't call out to network anymore
ENV COREPACK_ENABLE_NETWORK=0

### INSTALL DEPS ONCE
FROM base AS deps
RUN pnpm install --frozen-lockfile --ignore-scripts

### BUILD TORRENTIAL
# Bookworm-pinned to match the runtime image's glibc (a trixie build would not run on bookworm).
FROM rustlang/rust:nightly-bookworm-slim AS torrential-build
## torrential no longer links libarchive (the archive backend shells out to the 7z
## CLI instead), so only protobuf-compiler is needed here.
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN cargo build --release --manifest-path ./torrential/Cargo.toml

### BUILD APP
FROM base AS build-system

ENV NODE_ENV=production
ENV NUXT_TELEMETRY_DISABLED=1

## add git so drop can determine its git ref at build
RUN apt-get update && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/*

## copy deps and rest of project files
COPY . .
COPY --from=deps /app/node_modules ./node_modules


ARG BUILD_DROP_VERSION
ARG BUILD_GIT_REF

## build
RUN pnpm run --filter=drop postinstall && pnpm run --filter=drop build


# create run environment for Drop
FROM base AS run-system

ENV NODE_ENV=production
ENV NUXT_TELEMETRY_DISABLED=1

# The base stage's `COPY . .` puts the whole repo into the runtime WORKDIR (/app),
# but at runtime only the artifacts copied explicitly below are needed. Drop the
# inherited `torrential` source dir: the service resolves the binary by scanning
# the cwd for `torrential`, and a directory there is spawned as ./torrential and
# fails with EACCES. With it gone, resolution falls through to the `torrential`
# binary installed on PATH (/usr/bin/torrential) below.
RUN rm -rf /app/torrential

# RUN --mount=type=cache,target=/root/.yarn YARN_CACHE_FOLDER=/root/.yarn yarn add --network-timeout 1000000 --no-lockfile --ignore-scripts prisma@6.11.1
## runtime deps:
##  - p7zip-full: provides the 7z CLI - used by the archive backend to list and
##    stream entries out of .7z/.zip/.rar/... versions
##  - nginx: front-end proxy
##  - openssl + ca-certificates: required by Prisma's query engine on Debian
## pnpm itself is provided by corepack (enabled in the base stage)
RUN apt-get update && apt-get install -y --no-install-recommends \
    p7zip-full \
    nginx \
    openssl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN pnpm install prisma@7.7.0 --global
# init prisma to download all required files
RUN pnpm prisma init

COPY --from=build-system /app/server/prisma.config.ts ./
COPY --from=build-system /app/server/.output ./app
COPY --from=build-system /app/server/prisma ./prisma
COPY --from=build-system /app/server/build ./startup
COPY --from=build-system /app/server/build/nginx.conf /nginx.conf
COPY --from=torrential-build /build/torrential/target/release/torrential /usr/bin/

ENV LIBRARY="/library"
ENV DATA="/data"
ENV NGINX_CONFIG="/nginx.conf"
# Nuxt's port
ENV PORT=4000

CMD ["sh", "/app/startup/launch.sh"]
