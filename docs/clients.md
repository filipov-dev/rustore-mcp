# Подключение к MCP-клиентам

Сервер работает через stdio: MCP-клиент сам запускает `rustore-mcp serve --profile <профиль>` и общается с ним через stdin и stdout. Ключ RuStore API сервер берёт из профиля в системном хранилище, поэтому в конфигурации клиента его нет.

Рекомендуемая схема: **один ключ компании — один профиль**. Ключ создаётся в консоли RuStore («Компания → RuStore API») с теми методами и приложениями, которые нужны агенту ([ключ RuStore API](cli.md#ключ-rustore-api)); ключ, который больше не нужен, удаляется там же, не трогая остальных.

Если на машине агента нет системного хранилища или клиент подключается к серверу по адресу, используйте [HTTP-режим](http.md#подключение-клиента).

## Claude Code и Codex: connect

```bash
rustore-mcp connect claude-code --key-file ~/Downloads/rustore-key.json
rustore-mcp connect codex
```

`connect` по шагам:

1. Читает ключ из `--key-file` или спрашивает путь к файлу ключа (и keyId, если в файле только приватный ключ). Если рабочий ключ уже сохранён, предлагает его оставить.
2. Проверяет ключ: получает токен RuStore API и запрашивает список приложений `GET /public/v1/application`. Если проверка не прошла, ничего не сохраняется, а сообщение подсказывает, что отметить у ключа в консоли RuStore.
3. Сохраняет ключ в системное хранилище, а настройки — в профиль `~/.config/rustore-mcp/profiles.toml` (в Windows — `%APPDATA%\rustore-mcp\profiles.toml`, см. [Windows](#windows)).
4. Показывает, что изменится в конфигурации клиента, и после подтверждения регистрирует сервер `rustore`: в Claude Code — через `claude mcp add`, в Codex — таблицей `[mcp_servers.rustore]` в `~/.codex/config.toml`.
5. В терминале предлагает удалить исходный файл ключа (по умолчанию — нет: консоль RuStore показывает приватный ключ только при создании).

После этого перезапустите клиент. Профиль по умолчанию — `default`, имя сервера — `rustore` (для другого профиля — `rustore-<профиль>`). Все флаги и подробности — в [справочнике CLI](cli.md#connect).

### Права и наборы

Флаги `connect` сохраняются в профиль и попадают в команду запуска сервера:

```bash
# Только чтение (по умолчанию)
rustore-mcp connect codex

# Права профиля: черновики версий, тексты, APK и AAB, иконка и скриншоты
rustore-mcp connect codex --allow-write

# Права профиля: плюс модерация, публикация, ответы на отзывы, платежи и доступы
rustore-mcp connect codex --allow-destructive

# Дополнительные наборы инструментов
rustore-mcp connect claude-code --toolsets apps,releases,listing,reviews,purchases
```

Флаги доступа `connect` меняют права профиля, а не запись клиента: в клиенте регистрируется `serve --profile <профиль>` без флагов. Повторный `connect` без флагов права профиля не трогает. Поменять права можно и в `rustore-mcp ui` (вкладка Access); на macOS повышение прав подтверждается Touch ID или паролем. Для записи у ключа в консоли RuStore должны быть отмечены нужные методы API.

С `--scope project` (только Claude Code) сервер регистрируется в `.mcp.json` текущего каталога. Ключ при этом остаётся в хранилище того, кто запускал `connect`, а в команде записан абсолютный путь к бинарнику на его машине: другим участникам проекта нужен свой ключ под тем же именем профиля (`rustore-mcp auth login --profile <профиль> --key-file <файл>`) и бинарник по тому же пути.

### Несколько компаний

`--profile` и `--name` задают профиль и имя сервера в клиенте. Так в одном клиенте можно подключить ключи нескольких компаний: `rustore-mcp connect claude-code --profile partner --key-file <файл>` регистрирует сервер `rustore-partner`. Имя сервера — не длиннее 17 символов: иначе полные имена инструментов `mcp__<сервер>__<инструмент>` не уложатся в предел клиентов в 64 символа. Запись с тем же именем, которая запускает не `rustore-mcp`, `connect` заменяет только после явного ответа в терминале.

## Другие клиенты: ручное подключение

`connect` настраивает только Claude Code и Codex. Любой клиент, который умеет запускать stdio MCP-серверы, подключается вручную. Ниже — общий порядок, затем особенности [Google Antigravity](#google-antigravity), [Kimi Code](#kimi-code) и [ZCode](#zcode).

1. Сохраните ключ в отдельный профиль:

   ```bash
   rustore-mcp auth login --profile cursor --key-file ~/Downloads/rustore-key.json
   ```

   Новый профиль создаётся с доступом только на чтение.

2. Узнайте абсолютный путь к бинарнику: `command -v rustore-mcp` (в Windows — `(Get-Command rustore-mcp).Source`). Клиенты, запущенные не из терминала, часто не видят `PATH` оболочки.

3. Зарегистрируйте в клиенте stdio-сервер: команда — путь к `rustore-mcp`, аргументы — `serve --profile <профиль>`. Права задаются в профиле (`rustore-mcp ui`), а не флагами в записи клиента: флаги `serve` их только сужают. Формат `mcpServers`, который понимает большинство клиентов:

   ```json
   {
     "mcpServers": {
       "rustore": {
         "command": "/opt/homebrew/bin/rustore-mcp",
         "args": ["serve", "--profile", "cursor"]
       }
     }
   }
   ```

   Переменные окружения и ключ в записи не нужны.

4. Перезапустите клиент и проверьте, что у сервера `rustore` появились инструменты.

Права сервер всегда берёт из профиля: флаги доступа в `args` их только сужают, а флаг сверх прав профиля — ошибка запуска. Если `--toolsets` не указан, наборы тоже берутся из профиля, а без настроек сервер работает только на чтение с наборами по умолчанию. Проверить, с какими настройками запустится сервер: `rustore-mcp serve --status --profile <профиль>`.

### Ручная регистрация в Claude Code и Codex

Если вы не хотите, чтобы `connect` менял конфигурацию, зарегистрируйте сервер сами после `auth login`:

```bash
claude mcp add --scope user rustore -- "$(command -v rustore-mcp)" serve --profile claude-code
```

```toml
# ~/.codex/config.toml
[mcp_servers.rustore]
command = "/opt/homebrew/bin/rustore-mcp"
args = ["serve", "--profile", "codex"]
```

### Google Antigravity

Профиль: `rustore-mcp auth login --profile antigravity`.

Глобальный конфиг MCP — `~/.gemini/config/mcp_config.json`, общий для Antigravity App, IDE и CLI (версия 2.x). Antigravity IDE 1.x читает `~/.gemini/antigravity/mcp_config.json`. Если не уверены, какой файл действует, откройте его из интерфейса: в IDE — «…» на панели агента → MCP Servers → Manage MCP Servers → View raw config, в App — Customizations → Installed → MCP Servers → Copy Config File Path. Конфиг одного проекта — `<проект>/.agents/mcp_config.json`.

```json
{
  "mcpServers": {
    "rustore": {
      "command": "/opt/homebrew/bin/rustore-mcp",
      "args": ["serve", "--profile", "antigravity"]
    }
  }
}
```

После сохранения нажмите Refresh в Manage MCP Servers (в App — Restart у сервера, в CLI — `/mcp`). Сервер `rustore` должен появиться со списком инструментов.

Antigravity не включает больше 100 инструментов. У `rustore-mcp` их меньше даже со всеми наборами и уровнем удаления (52 операции RuStore API, 4 операции push, 16 составных и встроенные инструменты), так что сужать наборы ради этого лимита не нужно; лишние можно отключить полем `disabledTools` записи.

### Kimi Code

Профиль: `rustore-mcp auth login --profile kimi`.

Kimi Code CLI читает серверы из `~/.kimi-code/mcp.json` (или `$KIMI_CODE_HOME/mcp.json`), конфиг одного проекта — `.kimi-code/mcp.json`. Запись можно добавить командой `/mcp-config` в сессии или вписать в файл:

```json
{
  "mcpServers": {
    "rustore": {
      "command": "/opt/homebrew/bin/rustore-mcp",
      "args": ["serve", "--profile", "kimi"]
    }
  }
}
```

Сервер появляется только в новой сессии; проверьте его командой `/mcp`. Инструменты в Kimi называются `mcp__rustore__<инструмент>`, вызовы без постоянного разрешения клиент подтверждает.

Предыдущий клиент, kimi-cli, хранит тот же формат в `~/.kimi/mcp.json` и регистрирует сервер командой:

```bash
kimi mcp add --transport stdio rustore -- "$(command -v rustore-mcp)" serve --profile kimi
```

### ZCode

Профиль: `rustore-mcp auth login --profile zcode`.

Сервер добавляется в Settings → MCP servers → New MCP server: область User (все проекты) или Workspace (текущий проект), имя `rustore`, тип stdio, в `command` — абсолютный путь к `rustore-mcp`, в `args` — `serve --profile zcode`. Проверьте, что переключатель сервера включён.

Панель пишет в `~/.zcode/cli/config.json` (проект — `<проект>/.zcode/config.json`). Серверы там лежат под ключом `mcp.servers`, а не `mcpServers`:

```json
{
  "mcp": {
    "servers": {
      "rustore": {
        "command": "/opt/homebrew/bin/rustore-mcp",
        "args": ["serve", "--profile", "zcode"]
      }
    }
  }
}
```

Импорт серверов из Claude Code и Codex на той же странице перенесёт их запись вместе с профилем: агент ZCode будет работать с ключом и правами профиля другого клиента. Чтобы у ZCode был свой профиль, добавляйте запись вручную.

## Windows

`connect`, `auth` и `serve` работают в Windows так же, команды можно запускать из PowerShell, `cmd` или Git Bash. Установка — [Установка: Windows](install.md#windows).

- **Ключ** хранится в Windows Credential Manager: общие учётные данные `<профиль>.dev.filipov.rustore-mcp` (длинный ключ — частями, в записях `<профиль>/part-2.dev.filipov.rustore-mcp` и далее) (Панель управления → Диспетчер учётных данных → Учётные данные Windows). Настройки профилей — в `%APPDATA%\rustore-mcp\profiles.toml`. Credential Manager доступен только в сеансе, вошедшем с паролем: в SSH-сеансе со входом по ключу, в службе или задаче планировщика без сохранённого пароля Windows не даёт хранилища (ошибка «A specified logon session does not exist»). Запускайте `connect`, `auth` и MCP-клиент в обычном сеансе пользователя; для агента на другой машине есть [HTTP-режим](http.md).
- **Claude Code**: `connect claude-code` вызывает `claude mcp add`, поэтому команда `claude` должна быть в `PATH` (подходят и `claude.exe` из установщика Claude Code, и `claude.cmd` из npm).
- **Codex**: `connect codex` пишет в `%USERPROFILE%\.codex\config.toml` (или в `config.toml` каталога из `CODEX_HOME`).
- В конфигурацию клиента записывается полный путь к `rustore-mcp.exe`, из которого запущен `connect`, например `C:\Users\<имя>\AppData\Local\Programs\rustore-mcp\rustore-mcp.exe`. Обновляйте бинарник заменой файла по тому же пути, тогда `connect` повторять не нужно.

Ручная регистрация в Codex в Windows (в TOML обратные косые черты внутри одинарных кавычек не экранируются):

```toml
# %USERPROFILE%\.codex\config.toml
[mcp_servers.rustore]
command = 'C:\Users\<имя>\AppData\Local\Programs\rustore-mcp\rustore-mcp.exe'
args = ["serve", "--profile", "codex"]
```

## Рабочий каталог и локальные файлы

Инструменты, которые загружают или сохраняют файлы, работают только с путями внутри рабочего каталога сервера, системного временного каталога и каталогов `allow_path` профиля. Рабочий каталог сервера — тот, в котором клиент его запустил (обычно каталог проекта). Если клиент запускает сервер в домашнем каталоге или в корне файловой системы, рабочий каталог в разрешённые не попадает: добавьте нужные каталоги в права профиля (`rustore-mcp ui`, вкладка Access). Подробнее — [Безопасность](security.md#локальные-файлы).

## Несколько профилей

Профилей может быть сколько угодно: например, `claude-code` только на чтение с ключом одной компании и `codex` с правом записи с ключом другой. Проверить профиль:

```bash
rustore-mcp auth status --profile codex
```

Команда показывает замаскированный keyId (не сам ключ), результат проверки ключа, имена пакетов, клиент, разрешены ли запись и удаление и выбранные наборы.

## Отключение

1. Удалите запись `rustore` из клиента: `claude mcp remove --scope user rustore`, таблицу `[mcp_servers.rustore]` из `~/.codex/config.toml` или запись из конфигурации другого клиента.
2. Удалите ключ из хранилища и настройки профиля: `rustore-mcp auth logout --profile codex --forget`.
3. Если ключ больше нигде не нужен, удалите его в консоли RuStore («Компания → RuStore API»).
