# HTTP-режим и Docker

Обычно MCP-клиент сам запускает `rustore-mcp serve` на машине агента и говорит с ним через stdio. HTTP-режим (`serve --http`) нужен, когда у клиента нет локального бинарника (облачный агент, клиент только с удалёнными MCP-серверами) или когда один сервер с одним ключом RuStore API компании обслуживает нескольких агентов.

Ключ RuStore API при этом остаётся на стороне сервера и по сети не ходит. Клиенты приходят со своими токенами доступа к MCP; у каждого токена свой уровень доступа и наборы инструментов. Инструменты, уровни доступа, наборы ([MCP-инструменты](tools.md)) и вырезание учётных данных из ответов ([Безопасность](security.md)) — те же, что у stdio-сервера.

## Как это устроено

- `POST /mcp` — Streamable HTTP без сессий: каждый запрос обрабатывается отдельно, ответы — `application/json`. Другие методы отвечают `405`. `GET /health` — проверка готовности без токена: `{"status":"ok","service":"rustore-mcp",…}`.
- Адрес по умолчанию — `127.0.0.1:8766` (`--listen`), то есть только локальные подключения.
- Ключ RuStore API — из профиля `--profile` в системном хранилище машины сервера, как у stdio-сервера. В контейнере, где системного хранилища нет, — из файла секрета `--key-file` (только Unix) с правами `0600` или `0400`: JSON `{"keyId", "privateKey"}` или только приватный ключ вместе с `--key-id`. Токен RuStore API сервер получает по ключу сам и держит только в памяти; один токен используют все клиенты.
- Проект и сервисный токен push (набор `push`) — из того же профиля (`auth push login`) или из файла секрета `--push-file` с теми же правами: JSON `{"projectId", "serviceToken"}`. С `--key-file` системное хранилище не читается, и push настраивается только `--push-file`.
- Клиент присылает `Authorization: Bearer <токен доступа>`. Сервер знает только SHA-256 токенов (файл `--access-tokens`) и сравнивает их за постоянное время: без токена, с чужим или коротким токеном — `401`. Заголовок `Authorization` в MCP дальше не передаётся.
- Уровень доступа сервера (`--allow-write`, `--allow-destructive`) — потолок для токенов: токен с уровнем выше — ошибка запуска. `--toolsets` — наборы для токенов, у которых своих наборов нет.
- Динамических наборов (`--dynamic-toolsets`), `--status` и `--allow-path` в HTTP-режиме нет. Пользовательские инструменты — только из `--tools-file`. Инструменты с локальными файлами (загрузка сборок, иконки и скриншотов, выгрузка отзывов, `upload_build`, `reviews_export`, `listing_export`, `listing_apply`, `sync_screenshots`) не регистрируются, а `save_to` у `payments_report` и `catalog_summary` недоступен: у сервера нет доступа к файлам машины агента.
- Из настроек профиля берутся только имена пакетов по умолчанию; флаги доступа и наборы профиля — нет: в HTTP-режиме уровень задают флаги сервера и файл токенов.

## Токены доступа

Токен доступа — случайная строка из 32–512 символов, которую оператор сервера выдаёт клиенту. Это не ключ и не токен RuStore API: токен только пускает к серверу. Создайте токен и его хеш:

```bash
TOKEN="$(openssl rand -hex 32)"
printf '%s' "$TOKEN" | sha256sum      # macOS: shasum -a 256
```

Токен отдайте клиенту (в конфигурацию его MCP-клиента), в файл сервера запишите только хеш:

```toml
# tokens.toml
[[tokens]]
name = "ci"              # имя в логах сервера: A-Z, a-z, 0-9, ".", "_", "-", до 64 символов
sha256 = "<64 hex-символа>"
# access = "read"        # read (по умолчанию) | write | destructive
# toolsets = ["<набор>"] # по умолчанию --toolsets сервера

[[tokens]]
name = "release-bot"
sha256 = "<64 hex-символа>"
access = "write"
```

Имена и хеши не должны повторяться; неизвестные поля, неизвестные наборы и уровни — ошибка запуска. Чтобы отозвать токен, удалите его запись и перезапустите сервер.

## Запуск на машине

```bash
rustore-mcp auth login --profile shared --key-file ~/Downloads/rustore-key.json
rustore-mcp serve --http --profile shared --access-tokens ~/rustore/tokens.toml
# rustore-mcp: MCP endpoint http://127.0.0.1:8766/mcp
```

Флаги HTTP-режима:

| Флаг | Что делает |
| --- | --- |
| `--http` | Обслуживать MCP по HTTP вместо stdio. |
| `--listen <IP:порт>` | Адрес, по умолчанию `127.0.0.1:8766`. |
| `--access-tokens <путь>` | Файл токенов доступа (обязателен). |
| `--key-file <путь>` | Ключ из файла секрета (`0600` или `0400`, только Unix) вместо системного хранилища. |
| `--key-id <ID>` | keyId для `--key-file`, в котором только приватный ключ. |
| `--push-file <путь>` | Проект и сервисный токен push из файла секрета (`0600` или `0400`, только Unix) вместо системного хранилища. |
| `--allowed-host <имя>` | Дополнительное допустимое значение `Host` (имя — с любым портом, или `имя:порт`); повторяемый. |
| `--allowed-origin <origin>` | Допустимый `Origin` браузерного клиента, например `http://localhost:6274` (`:*` — любой порт); повторяемый. |
| `--max-request-bytes <байты>` | Лимит тела запроса, по умолчанию 1048576 (1 МиБ); больше — `413`. |

Остальные флаги `serve` (`--allow-write`, `--allow-destructive`, `--toolsets`, `--tools-file`, `--timeout`, `--max-response-bytes`, `--log-level`) работают как в [справочнике CLI](cli.md#serve).

`Host` запроса должен быть loopback-именем (`localhost`, `127.0.0.1`, `::1` с любым портом), адресом `--listen` или `--allowed-host`, иначе `403` ещё до проверки токена (защита от DNS rebinding). Запросы из браузера с заголовком `Origin` отклоняются, пока `Origin` не разрешён `--allowed-origin`. Сервер завершается по SIGINT и SIGTERM.

Вне loopback ставьте сервер за reverse proxy с TLS (nginx, Caddy, Traefik) или в закрытую сеть и добавьте имя прокси через `--allowed-host`: сервер говорит по обычному HTTP, а токены доступа идут в заголовках.

### systemd

Файл `/etc/systemd/system/rustore-mcp.service` для бинарника `/usr/local/bin/rustore-mcp`; ключ — файл секрета, доступный только пользователю сервиса (например, через `LoadCredential=`):

```ini
[Unit]
Description=RuStore MCP server (HTTP)
After=network-online.target
Wants=network-online.target

[Service]
LoadCredential=rustore-key:/etc/rustore-mcp/rustore-key.json
ExecStart=/usr/local/bin/rustore-mcp serve --http --listen 127.0.0.1:8766 \
  --access-tokens /etc/rustore-mcp/tokens.toml --key-file ${CREDENTIALS_DIRECTORY}/rustore-key
Environment=RUST_LOG=warn
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rustore-mcp
curl http://127.0.0.1:8766/health
```

### За reverse proxy с TLS

Пример для nginx:

```nginx
server {
    listen 443 ssl;
    server_name mcp.example.com;
    # ssl_certificate и ssl_certificate_key — ваши сертификаты

    location /mcp {
        proxy_pass http://127.0.0.1:8766;
        proxy_set_header Host $host;
        proxy_http_version 1.1;
        proxy_buffering off;
        proxy_read_timeout 120s;
    }
}
```

Сервер запускается с `--allowed-host mcp.example.com`. Proxy не должен логировать заголовок `Authorization`.

## Docker

В репозитории есть `Dockerfile` (статический бинарник musl в минимальном образе Alpine, из пакетов только корневые сертификаты для TLS RuStore API, пользователь `10001`, порт `8766`, `HEALTHCHECK` по `/health`) и пример `compose.yaml`.

```bash
mkdir -p secrets
cp ~/Downloads/rustore-key.json secrets/rustore-key.json
sudo chown 10001 secrets/rustore-key.json   # владелец — пользователь контейнера
chmod 600 secrets/rustore-key.json
# tokens.toml — рядом с compose.yaml, см. «Токены доступа»
docker compose up --build -d
docker compose logs -f rustore-mcp
curl http://127.0.0.1:8766/health
```

- Ключ монтируется секретом `/run/secrets/rustore-key` и в образ не попадает (`secrets/`, `tokens.toml` и файлы ключей исключены `.dockerignore` и `.gitignore`). Файл секрета Docker Compose монтирует с правами и владельцем файла хоста, поэтому владельцем должен быть uid `10001` (или запускайте контейнер с `user:` владельца файла). Ключ с правами для группы или остальных сервер не принимает.
- Файл токенов монтируется только на чтение в `/etc/rustore-mcp/tokens.toml`.
- Порт публикуется только на loopback хоста (`127.0.0.1:8766`). `--allowed-host rustore-mcp` в примере разрешает обращения других контейнеров той же сети по имени сервиса.
- Уровень доступа сервера и наборы по умолчанию добавляйте в `command` (`--allow-write`, `--toolsets …`). Контейнер работает с `read_only`, без capabilities и с `no-new-privileges`.

Без Compose:

```bash
docker build -t rustore-mcp .
docker run --rm -p 127.0.0.1:8766:8766 \
  -v "$PWD/tokens.toml:/etc/rustore-mcp/tokens.toml:ro" \
  -v "$PWD/secrets/rustore-key.json:/run/secrets/rustore-key:ro" \
  rustore-mcp
```

## Подключение клиента

Клиенту нужны адрес `/mcp` (а не `/health`) и токен доступа в заголовке `Authorization`. Claude Code:

```bash
claude mcp add --transport http --scope user rustore http://127.0.0.1:8766/mcp \
  --header "Authorization: Bearer $TOKEN"
```

Токен сохраняется в конфигурации Claude Code (для `--scope project` — в `.mcp.json` проекта, не коммитьте его). Проверка: `claude mcp list` или `/mcp` в сессии.

Codex (`~/.codex/config.toml`), токен — из переменной окружения, в которой запускается Codex (сам `rustore-mcp` её не читает):

```toml
[mcp_servers.rustore]
url = "http://127.0.0.1:8766/mcp"
bearer_token_env_var = "RUSTORE_MCP_TOKEN"
```

Клиенты с форматом `mcpServers`:

```json
{
  "mcpServers": {
    "rustore": {
      "type": "http",
      "url": "http://127.0.0.1:8766/mcp",
      "headers": { "Authorization": "Bearer <токен доступа>" }
    }
  }
}
```

`connect` к HTTP-режиму не относится: он работает с локальным бинарником и системным хранилищем машины агента.

## Проверка

```bash
curl http://127.0.0.1:8766/health
curl -s http://127.0.0.1:8766/mcp \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -H "Authorization: Bearer $TOKEN" \
  --data '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

| Ответ | Причина |
| --- | --- |
| `401` с `WWW-Authenticate: Bearer` | Нет `Authorization: Bearer …`, заголовок повторён или в другом формате, токен короче 32 символов или его хеша нет в файле токенов (`error="invalid_token"`). |
| `403` | `Host` не разрешён (`--allowed-host`) или браузерный `Origin` не разрешён (`--allowed-origin`). |
| `413` | Тело больше `--max-request-bytes`. |
| Инструмент вернул ошибку `HTTP 401` или ошибку получения токена от RuStore | RuStore не принял ключ сервера: ключ удалён или отключён в консоли RuStore. Замените ключ на сервере. |
| Инструмент вернул ошибку `HTTP 403` от RuStore | У ключа сервера не отмечен метод API или приложение: исправьте ключ в консоли RuStore («Компания → RuStore API»). |
