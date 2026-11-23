# Установка и обновление

`rustore-mcp` — один бинарник: и MCP-сервер (`serve`), и CLI для настройки (`connect`, `auth`). Готовые сборки есть для macOS, Linux и Windows; на других платформах соберите бинарник из исходников.

## Платформы

| Платформа | Сборка | Хранилище ключа |
| --- | --- | --- |
| macOS, Apple silicon и Intel | универсальный бинарник, подписан и нотаризован | Keychain |
| Linux x86_64 | статический бинарник (musl), без зависимостей | Secret Service |
| Linux aarch64 | статический бинарник (musl), без зависимостей | Secret Service |
| Windows x86_64 | [архив `.zip`](#windows), без зависимостей, не подписан | Credential Manager |

Linux-бинарник собран статически: ему не нужны glibc определённой версии, `libdbus-1` и другие системные библиотеки, он запускается в любом дистрибутиве, включая Alpine и slim-образы. Для HTTPS-запросов к RuStore API нужны корневые сертификаты системы (пакет `ca-certificates`).

В Linux ключ RuStore API хранится через Secret Service (GNOME Keyring, KeePassXC, KWallet и т. п.): провайдер должен быть запущен в сессии D-Bus пользователя ([Linux без Secret Service](troubleshooting.md#linux-secret-service-недоступен)). На машине без системного хранилища используйте [HTTP-режим](http.md).

## Установочный скрипт

Одна команда для Linux (x86_64, aarch64) и macOS, Homebrew не нужен:

```bash
curl -fsSL https://codeload.github.com/filipov-dev/rustore-mcp/tar.gz/refs/heads/install | tar -xzO | sh
```

Адрес — архив ветки `install` копии репозитория на GitHub: в ней лежит только `install.sh` со списком релизов (адреса архивов и их SHA-256), его обновляет релизный конвейер. `tar -xzO` печатает этот скрипт, `sh` выполняет. Архивы релизов тоже лежат на GitHub. Прежний адрес `https://codeload.sourcecraft.tech/filipov-dev/rustore-mcp/tarball/refs/heads/install` работает и ставит те же архивы.

Что делает скрипт:

- определяет ОС и архитектуру и выбирает архив релиза (`linux-x86_64`, `linux-aarch64` или `macos-universal`);
- скачивает архив и `SHA256SUMS` релиза (через `curl` или `wget`, только HTTPS) и сверяет сумму архива с `SHA256SUMS` и со списком в скрипте; при расхождении ничего не устанавливает;
- проверяет, что скачанный бинарник запускается, и кладёт его в `~/.local/bin/rustore-mcp` без `sudo`; замена атомарная, уже запущенный сервер продолжает работать со старой копией;
- если каталога нет в `PATH`, подсказывает, как его добавить.

Повторный запуск обновляет бинарник до последней версии (если она уже стоит — ничего не скачивает). Путь не меняется, поэтому `connect` повторять не нужно.

Параметры передаются через `sh -s --`:

```bash
# Конкретная версия
curl -fsSL https://codeload.github.com/filipov-dev/rustore-mcp/tar.gz/refs/heads/install | tar -xzO | sh -s -- --version v0.1.0

# Другой каталог: бинарник попадёт в /opt/rustore/bin
curl -fsSL https://codeload.github.com/filipov-dev/rustore-mcp/tar.gz/refs/heads/install | tar -xzO | sh -s -- --prefix /opt/rustore
```

| Параметр | Что делает |
| --- | --- |
| `--version vX.Y.Z` | Версия вместо последней (с `v` или без). Список — `--list`. |
| `--prefix DIR` | Установить в `DIR/bin` (по умолчанию `~/.local`). |
| `--bin-dir DIR` | Установить ровно в `DIR` (важнее `--prefix`). |
| `--force` | Переустановить, даже если эта версия уже стоит. |
| `--list` | Показать доступные версии и выйти. |

Нужны `sh`, `tar`, `curl` или `wget`, `sha256sum` или `shasum`. Чтобы прочитать скрипт перед запуском, сохраните его в файл: `curl -fsSL https://codeload.github.com/filipov-dev/rustore-mcp/tar.gz/refs/heads/install | tar -xzO > install.sh`.

## Homebrew

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/rustore-mcp
rustore-mcp --version
```

- Tap размещён на SourceCraft, а не на GitHub, поэтому `brew tap` нужен полный адрес репозитория.
- Формулу устанавливайте по полному имени `filipov-dev/tap/rustore-mcp`: так Homebrew фиксирует, что вы доверяете формуле из стороннего tap.
- В Linux формула ставит статический бинарник, зависимостей у неё нет.

Обновление и удаление:

```bash
brew update && brew upgrade filipov-dev/tap/rustore-mcp
brew uninstall filipov-dev/tap/rustore-mcp
```

`connect` записывает в конфигурацию клиента путь `…/bin/rustore-mcp`, а не путь внутри `Cellar`, поэтому после `brew upgrade` клиенты запускают новую версию без повторного подключения.

## Готовый архив

Архивы и файл `SHA256SUMS` публикуются на [странице релизов](https://github.com/filipov-dev/rustore-mcp/releases). Имя архива: `rustore-mcp-<тег>-<платформа>.tar.gz`, например `rustore-mcp-v0.1.0-linux-x86_64.tar.gz` (для Windows — `.zip`, см. [ниже](#windows)). Внутри — каталог с бинарником `rustore-mcp`, `README.md` и лицензиями.

Проверьте контрольную сумму (из каталога, куда скачаны архив и `SHA256SUMS`):

```bash
# Linux
grep linux-x86_64 SHA256SUMS | sha256sum -c -

# macOS
grep macos-universal SHA256SUMS | shasum -a 256 -c -
```

Распакуйте и установите бинарник в каталог из `PATH`:

```bash
tar -xzf rustore-mcp-v0.1.0-linux-x86_64.tar.gz
sudo install -m 0755 rustore-mcp-v0.1.0-linux-x86_64/rustore-mcp /usr/local/bin/
rustore-mcp --version
```

Чтобы обновиться, замените бинарник новым по тому же пути: конфигурацию клиентов трогать не нужно.

## Windows

Для Windows есть только архив `rustore-mcp-<тег>-windows-x86_64.zip` на [странице релизов](https://github.com/filipov-dev/rustore-mcp/releases): установочный скрипт и Homebrew Windows не поддерживают. Внутри архива — каталог с `rustore-mcp.exe`, `README.md` и лицензиями. Бинарнику не нужен Visual C++ Redistributable и другие библиотеки. Ключ RuStore API хранится в Windows Credential Manager (общие учётные данные `<профиль>.dev.filipov.rustore-mcp`), настройки профилей — в `%APPDATA%\rustore-mcp\profiles.toml`.

Скачайте архив и `SHA256SUMS` в один каталог и выполните в PowerShell из него (тег замените на нужный):

```powershell
$tag = 'v0.1.0'
$zip = "rustore-mcp-$tag-windows-x86_64.zip"

# Контрольная сумма: значение должно совпасть со строкой архива в SHA256SUMS
(Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
Select-String -SimpleMatch $zip SHA256SUMS

# Распаковка и установка в профиль пользователя (права администратора не нужны)
Unblock-File $zip
Expand-Archive $zip -DestinationPath "$env:TEMP\rustore-mcp" -Force
$dir = "$env:LOCALAPPDATA\Programs\rustore-mcp"
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item "$env:TEMP\rustore-mcp\rustore-mcp-$tag-windows-x86_64\rustore-mcp.exe" $dir -Force
```

Добавьте каталог в `PATH` пользователя (один раз) и откройте новое окно терминала:

```powershell
$dir = "$env:LOCALAPPDATA\Programs\rustore-mcp"
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($path -split ';') -notcontains $dir) {
  [Environment]::SetEnvironmentVariable('Path', "$path;$dir", 'User')
}
```

```powershell
rustore-mcp --version
```

Бинарник не подписан сертификатом подписи кода. Если открыть его из Проводника или из браузера, SmartScreen может показать «Система Windows защитила ваш компьютер»: проверьте контрольную сумму и нажмите «Подробнее» → «Выполнить в любом случае». `Unblock-File` снимает с архива отметку «загружено из интернета», и распакованный бинарник её не получает. В Windows 11 со включённым Smart App Control неподписанные программы блокируются без возможности запуска — используйте другую машину или соберите бинарник из исходников.

Чтобы обновиться, замените `rustore-mcp.exe` новым по тому же пути: конфигурацию клиентов трогать не нужно. Пока MCP-клиент держит сервер запущенным, файл занят — закройте клиент или сначала переименуйте старый файл (`Rename-Item rustore-mcp.exe rustore-mcp.old.exe`; переименовать запущенный файл Windows разрешает) и удалите его после перезапуска клиента.

Особенности `connect` и Credential Manager в Windows — в разделе [Подключение к MCP-клиентам](clients.md#windows).

## Из исходников

Нужен Rust 1.88 или новее (`rustup` подхватит версию из `rust-toolchain.toml`) и C-компилятор для криптографии TLS. Системные библиотеки D-Bus и OpenSSL не нужны.

```bash
cargo build --release
```

Бинарник: `target/release/rustore-mcp` (в Windows — `target\release\rustore-mcp.exe`). Скопируйте его в каталог из `PATH` или установите сразу:

```bash
cargo install --path crates/rustore-mcp
```

На macOS собранный локально бинарник не подписан Developer ID: после каждой пересборки Keychain снова спросит разрешение на доступ к ключу ([Решение проблем](troubleshooting.md#macos-keychain-спрашивает-разрешение)).

Для сборки в Windows нужны Visual Studio Build Tools с компилятором MSVC ([Разработка](development.md#windows)).

## Docker

Для [HTTP-режима](http.md#docker) в репозитории есть `Dockerfile` и `compose.yaml`: образ собирается из исходников.

## Первое подключение

После установки подключите сервер к клиенту: `rustore-mcp connect claude-code` или `rustore-mcp connect codex`. `connect` примет ключ RuStore API из файла (`--key-file`), проверит и сохранит его в системном хранилище и зарегистрирует сервер в клиенте. Ключ создаётся в консоли RuStore: «Компания → RuStore API». Подробнее — [Подключение к MCP-клиентам](clients.md) и [ключ RuStore API](cli.md#ключ-rustore-api).

## Удаление

1. Отключите сервер в клиентах: `claude mcp remove --scope user rustore`, таблица `[mcp_servers.rustore]` в `~/.codex/config.toml` или запись `rustore` в конфигурации другого клиента.
2. Удалите ключи и настройки профилей: `rustore-mcp auth logout --profile <профиль> --forget` для каждого профиля.
3. Удалите ключи, которые больше не нужны, в консоли RuStore («Компания → RuStore API»).
4. Удалите бинарник (`brew uninstall filipov-dev/tap/rustore-mcp` или файл из `PATH`) и, если осталась, папку `~/.config/rustore-mcp` (в Windows — `%APPDATA%\rustore-mcp`).
