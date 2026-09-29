# Build from the repository root:
#   docker build -f infrastructure/docker/Dockerfile -t connexa-signaling .

FROM node:22-alpine AS clients
WORKDIR /src/clients
COPY clients/package.json clients/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY clients/ ./
RUN npm run build:web

FROM rust:1-slim-bookworm AS server
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY apps/ apps/
RUN cargo build --release -p signaling-server

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 connexa
COPY --from=server /src/target/release/signaling-server /usr/local/bin/signaling-server
COPY --from=clients /src/clients/web/dist /srv/web
ENV CONNEXA_BIND=0.0.0.0:8080 \
    CONNEXA_WEB_ROOT=/srv/web \
    RUST_LOG=info
USER connexa
# 8080: HTTP + WebSocket. 3479/udp: SFU media (when CONNEXA_SFU=true).
EXPOSE 8080 3479/udp
CMD ["signaling-server"]
