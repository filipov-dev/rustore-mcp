# rustore-mcp в HTTP-режиме (serve --http).
#
#   docker build -t rustore-mcp .
#   docker run --rm -p 127.0.0.1:8766:8766 \
#     -v "$PWD/tokens.toml:/etc/rustore-mcp/tokens.toml:ro" \
#     -v "$PWD/rustore-key.json:/run/secrets/rustore-key:ro" \
#     rustore-mcp
#
# Ключ RuStore API в образ не попадает: он монтируется файлом секрета
# (/run/secrets/rustore-key, JSON {"keyId", "privateKey"}) с правами 0600 или 0400,
# владелец — пользователь контейнера (uid 10001). Клиенты MCP приходят со своими
# токенами доступа в заголовке Authorization; в tokens.toml — только SHA-256 этих
# токенов. Подробности — docs/http.md.
#
# Бинарник собирается в Alpine статически (musl, без libdbus и OpenSSL), как бинарники
# релизов. Итоговый образ — минимальный alpine: из пакетов только корневые сертификаты
# для rustls, а busybox wget нужен HEALTHCHECK.

ARG RUST_VERSION=1.99.1
ARG ALPINE_VERSION=3.22

FROM docker.io/library/rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS build

# musl-dev — libc; perl, make и cmake — сборка aws-lc-sys (криптография rustls).
RUN apk add --no-cache musl-dev perl make cmake

WORKDIR /src
# rust-toolchain.toml не копируется: тулчейн задаёт образ (RUST_VERSION).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Цель musl по умолчанию линкуется статически (crt-static). Без символов бинарник
# заметно меньше.
ENV CARGO_PROFILE_RELEASE_STRIP=symbols \
    CARGO_TERM_COLOR=never
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rustore-mcp \
    && cp target/release/rustore-mcp /usr/local/bin/rustore-mcp
# Статическая сборка: у бинарника нет интерпретатора (PT_INTERP), ldd musl на нём
# завершается ошибкой. Динамический бинарник здесь — регрессия, сборка падает.
RUN if ldd /usr/local/bin/rustore-mcp >/dev/null 2>&1; then \
        echo "rustore-mcp is dynamically linked" >&2; exit 1; \
    fi

FROM docker.io/library/alpine:${ALPINE_VERSION}

# ca-certificates — rustls проверяет сертификат RuStore API по системным корневым
# сертификатам. Других библиотек статическому бинарнику не нужно.
RUN apk add --no-cache ca-certificates \
    && adduser -D -H -u 10001 -s /sbin/nologin rustore

COPY --from=build /usr/local/bin/rustore-mcp /usr/local/bin/rustore-mcp
RUN /usr/local/bin/rustore-mcp --version

# Непривилегированный пользователь; файлов сервер не пишет, образ можно запускать read-only.
USER 10001:10001
EXPOSE 8766

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8766/health || exit 1

ENTRYPOINT ["/usr/local/bin/rustore-mcp"]
# Внутри контейнера слушаем все интерфейсы; на хосте публикуйте порт только на loopback
# (-p 127.0.0.1:8766:8766) или за TLS reverse proxy.
CMD ["serve", "--http", "--listen", "0.0.0.0:8766", \
     "--access-tokens", "/etc/rustore-mcp/tokens.toml", \
     "--key-file", "/run/secrets/rustore-key"]
