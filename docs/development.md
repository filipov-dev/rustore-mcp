# Разработка

## Структура

| Крейт | Назначение |
| --- | --- |
| `crates/rustore-api` | Спецификация RuStore API, генератор таблицы операций (`build.rs`, `build/`), исполнитель HTTP-запросов, ключ RuStore API и токен доступа (`src/key.rs`, `src/access.rs`), хранилище ключей (`src/auth.rs`). |
| `crates/rustore-mcp-core` | MCP-инструменты: реестр, уровни доступа, наборы, `api_request`, `api_search`, составные инструменты (`src/composite/`), пользовательские инструменты, фильтр `jq`, вырезание учётных данных, политика файлов, инструкции сервера для агента. Не зависит от транспорта. |
| `crates/rustore-mcp` | Бинарник: CLI (`serve`, `connect`, `auth`, `key exec`, `tools`) и транспорты stdio и HTTP (`src/cli/serve/http_server.rs`). |

## Сборка и проверки

Нужен Rust 1.88 или новее; `rust-toolchain.toml` выбирает стабильный канал с `rustfmt` и `clippy`. Для криптографии TLS нужен C-компилятор; системные библиотеки D-Bus и OpenSSL не нужны.

```bash
cargo build --release
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Тесты не обращаются к реальному RuStore API (HTTP подменяется `wiremock`) и не используют системное хранилище ключей; ключи RSA генерируются в тестах, время токена — управляемые часы. Тесты `key exec` — свои для Unix (режимы файла, сигналы) и для Windows (ACL, объект задания, пакетные файлы); тесты Windows выполняются только там.

Готовые сборки: macOS (universal), Linux x86_64 и aarch64 (статические, musl) и Windows x86_64 (`x86_64-pc-windows-msvc`).

### Windows

Сборка — нативная, целевая платформа `x86_64-pc-windows-msvc`: нужны Visual Studio Build Tools с компонентом «Разработка классических приложений на C++» и `rustup`. Релизный бинарник собирается со статическим CRT, чтобы на машине пользователя не требовался Visual C++ Redistributable:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc -p rustore-mcp
cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc
```

Ветки кода для Windows (ключ частями в Credential Manager, запуск `claude.cmd`, сравнение файлов без inode в исполнителе, ACL, объект задания и консольные сигналы `key exec`, запасные `.cmd` и `.bat`) компилируются или выполняются только там, поэтому проверяйте изменения в них этими командами. С macOS и Linux `cargo check --target x86_64-pc-windows-msvc` без Windows SDK не проходит: C-код `aws-lc-sys` требует заголовков Windows.

#### Clippy для Windows с macOS и Linux

Типы и предупреждения кода под `cfg(windows)` можно проверить без Windows: для цели `x86_64-pc-windows-gnu` C-компилятором служит [Zig](https://ziglang.org/) (`brew install zig` или архив с сайта Zig). Крейт `cc` добавляет к вызову компилятора аргумент `--target=x86_64-pc-windows-gnu`, которого Zig не понимает, поэтому компилятор подключается через обёртки, которые этот аргумент убирают. Обёртки создаются один раз:

```bash
rustup target add x86_64-pc-windows-gnu
W=~/.local/zig-windows
mkdir -p "$W"
for tool in cc c++; do
  cat > "$W/zig-$tool" <<EOF
#!/bin/bash
args=()
for a in "\$@"; do case "\$a" in --target=*) ;; *) args+=("\$a") ;; esac; done
exec zig $tool -target x86_64-windows-gnu "\${args[@]}"
EOF
done
printf '#!/bin/sh\nexec zig ar "$@"\n' > "$W/zig-ar"
chmod +x "$W/zig-cc" "$W/zig-c++" "$W/zig-ar"
```

Проверка из корня репозитория:

```bash
W=~/.local/zig-windows
CC_x86_64_pc_windows_gnu="$W/zig-cc" CXX_x86_64_pc_windows_gnu="$W/zig-c++" AR_x86_64_pc_windows_gnu="$W/zig-ar" \
  cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings
```

Так проверяются только компиляция и clippy: тесты для Windows так не запустить, а релизный бинарник собирается для `x86_64-pc-windows-msvc` на Windows (команды выше).

Запустить CLI из исходников:

```bash
cargo run -p rustore-mcp -- --help
cargo run -p rustore-mcp -- serve --help
```

## Спецификация и генерация инструментов

Официальной OpenAPI-спецификации у RuStore API нет. `crates/rustore-api/spec/rustore.openapi.json` собирается скриптом `build_openapi.py` из страниц [справочника RuStore API](https://www.rustore.ru/help/work-with-rustore-api) (Python 3.10+, без сторонних пакетов):

```bash
python3 crates/rustore-api/spec/build_openapi.py --cache /tmp/rustore-api-docs
```

Проверить, не отстала ли спецификация от справочника (новые и удалённые страницы, операции и параметры; код выхода `3`, если есть разница): `python3 crates/rustore-api/spec/build_openapi.py --check`.

Что внутри спецификации, какие расхождения справочника с реальным API исправлены и что не проверено — в [описании спецификации](../crates/rustore-api/spec/README.md).

При сборке `build.rs` превращает спецификацию в таблицу операций: имя инструмента, метод, путь, уровень доступа (`x-access-level`), набор (тег спецификации) и JSON Schema аргументов. Чтобы обновить инструменты:

1. Пересоберите спецификацию скриптом. JSON не правится руками: расхождения справочника с API исправляются исключениями в скрипте, чтобы не терялись при пересборке.
2. Соберите проект. Новый тег спецификации без slug набора — ошибка сборки: добавьте slug в `crates/rustore-api/build/toolsets.rs`.
3. Проверьте снапшоты операций (`crates/rustore-api/tests/snapshots/`) и уровни доступа новых операций; снапшоты обновляются осознанно: `cargo insta review` или `INSTA_UPDATE=always cargo test -p rustore-api`.
4. Обновите [MCP-инструменты](tools.md), если изменились наборы или число инструментов.

Сгенерированный код не правится вручную: единственный источник операций — спецификация.
