//! Регистрация MCP-сервера в клиентах (Claude Code, Codex).

pub mod claude;
pub mod codex;

use std::path::Path;

use anyhow::{Context, bail};
use clap::ValueEnum;

use crate::profiles::{DEFAULT_PROFILE, ProfileSettings};

/// Абсолютный путь к бинарнику для тестов. В Windows путь без буквы диска не абсолютный,
/// поэтому `/usr/local/bin/...` там не годится.
#[cfg(test)]
pub(crate) const TEST_EXE: &str = if cfg!(windows) {
    r"C:\tools\rustore-mcp.exe"
} else {
    "/usr/local/bin/rustore-mcp"
};

/// Имя сервера в конфигурации MCP-клиентов для профиля `default`.
pub const DEFAULT_SERVER_NAME: &str = "rustore";

/// Префикс имени сервера для остальных профилей: `rustore-<профиль>`.
pub const PROFILE_SERVER_PREFIX: &str = "rustore-";

/// Имя исполняемого файла `rustore-mcp` без расширения: по нему узнаём свои записи в
/// конфигурации клиентов.
pub const EXE_STEM: &str = "rustore-mcp";

/// Предел длины полного имени инструмента у клиентов (`mcp__<сервер>__<инструмент>`):
/// имя инструмента в API модели — не длиннее 64 символов.
pub const CLIENT_TOOL_NAME_LIMIT: usize = 64;

/// Самое длинное имя инструмента: генератор операций отклоняет имена длиннее 40 символов.
pub const MAX_TOOL_NAME_LEN: usize = 40;

/// Самое длинное имя сервера, при котором любой инструмент укладывается в
/// [`CLIENT_TOOL_NAME_LIMIT`]: 64 − 40 − `mcp__` − `__` = 17. `rustore` (7) влезает.
pub const MAX_SERVER_NAME_LEN: usize =
    CLIENT_TOOL_NAME_LIMIT - MAX_TOOL_NAME_LEN - "mcp__".len() - "__".len();

/// Поддерживаемые MCP-клиенты.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ClientKind {
    /// Claude Code (`claude mcp add`).
    ClaudeCode,
    /// OpenAI Codex (`$CODEX_HOME/config.toml`).
    Codex,
}

impl ClientKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// Область конфигурации клиента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Scope {
    /// Available in all projects of the current user.
    #[default]
    User,
    /// Shared with the project (Claude Code: `.mcp.json` in the current directory).
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Имя сервера по умолчанию для профиля: `rustore` для `default`, иначе
/// `rustore-<профиль>` (`.` в имени профиля заменяется на `-`: в имени сервера точки нельзя).
pub fn default_server_name(profile: &str) -> String {
    if profile == DEFAULT_PROFILE {
        DEFAULT_SERVER_NAME.to_owned()
    } else {
        format!("{PROFILE_SERVER_PREFIX}{}", profile.replace('.', "-"))
    }
}

/// Проверяет имя сервера в конфигурации клиента. Оно становится ключом TOML Codex и
/// аргументом `claude mcp add`, поэтому допускаются только `[A-Za-z0-9_-]`, без `-` в
/// начале. Длина — не больше [`MAX_SERVER_NAME_LEN`]: иначе полные имена самых длинных
/// инструментов (`mcp__<сервер>__<инструмент>`) не уложатся в предел клиентов.
pub fn validate_server_name(name: &str) -> anyhow::Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if name.is_empty() || !valid_chars || name.starts_with('-') {
        bail!(
            "invalid server name `{name}`: use characters from A-Z, a-z, 0-9, `_`, `-`, not \
             starting with `-`; choose one with --name"
        );
    }
    if name.len() > MAX_SERVER_NAME_LEN {
        bail!(
            "server name `{name}` is longer than {MAX_SERVER_NAME_LEN} characters: clients name \
             tools mcp__<server>__<tool> and limit that to {CLIENT_TOOL_NAME_LIMIT} characters, \
             and tool names are up to {MAX_TOOL_NAME_LEN} characters; choose a shorter name with \
             --name"
        );
    }
    Ok(())
}

/// Команда запуска сервера, которую получает MCP-клиент. Ключа в ней нет: `serve` берёт его
/// из системного хранилища по профилю.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// Имя сервера в конфигурации клиента.
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

impl ServerSpec {
    /// `exe` — абсолютный путь к бинарнику. Путь не канонизируется: при установке через
    /// Homebrew symlink в `bin/` переживает обновление версии, а путь в `Cellar` — нет.
    pub fn new(
        name: &str,
        exe: &Path,
        profile: &str,
        settings: &ProfileSettings,
    ) -> anyhow::Result<Self> {
        validate_server_name(name)?;
        if !exe.is_absolute() {
            bail!("executable path `{}` is not absolute", exe.display());
        }
        let command = exe
            .to_str()
            .with_context(|| format!("executable path `{}` is not valid UTF-8", exe.display()))?
            .to_owned();

        let mut args = vec![
            "serve".to_owned(),
            "--profile".to_owned(),
            profile.to_owned(),
        ];
        // Флаги — по действующему уровню: удаление всегда передаётся вместе с записью.
        if settings.writes_allowed() {
            args.push("--allow-write".to_owned());
        }
        if settings.allow_destructive {
            args.push("--allow-destructive".to_owned());
        }
        if !settings.toolsets.is_empty() {
            args.push("--toolsets".to_owned());
            args.push(settings.toolsets.join(","));
        }
        if settings.dynamic_toolsets {
            args.push("--dynamic-toolsets".to_owned());
        }
        Ok(Self {
            name: name.to_owned(),
            command,
            args,
        })
    }

    /// Команда целиком для показа пользователю.
    pub fn display(&self) -> String {
        shell_join(
            std::iter::once(self.command.as_str()).chain(self.args.iter().map(String::as_str)),
        )
    }
}

/// Запускает ли команда `command` бинарник `rustore-mcp` (по имени файла, с `.exe` или без).
pub fn is_own_command(command: &str) -> bool {
    // Разделители обеих систем: конфиг мог быть записан на другой ОС.
    let file = command.rsplit(['/', '\\']).next().unwrap_or(command);
    // Имя с `.exe` — путь Windows, где регистр в именах файлов не различается
    // (`RUSTORE-MCP.EXE` — тот же файл). Без `.exe` имя сравнивается точно.
    match file.len().checked_sub(".exe".len()) {
        Some(split)
            if file.is_char_boundary(split) && file[split..].eq_ignore_ascii_case(".exe") =>
        {
            file[..split].eq_ignore_ascii_case(EXE_STEM)
        }
        _ => file == EXE_STEM,
    }
}

/// Склеивает аргументы в строку для показа, экранируя их как в POSIX shell.
pub fn shell_join<'a>(args: impl IntoIterator<Item = &'a str>) -> String {
    args.into_iter()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | ',' | ':' | '=' | '@' | '+')
        });
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn server_spec_args() {
        let exe = PathBuf::from(TEST_EXE);
        let spec = ServerSpec::new("rustore", &exe, "codex", &ProfileSettings::default()).unwrap();
        assert_eq!(spec.name, "rustore");
        assert_eq!(spec.command, TEST_EXE);
        assert_eq!(spec.args, ["serve", "--profile", "codex"]);

        let settings = ProfileSettings {
            allow_write: true,
            allow_destructive: true,
            toolsets: vec!["releases".into(), "reviews".into()],
            dynamic_toolsets: true,
            ..ProfileSettings::default()
        };
        let spec = ServerSpec::new("rustore", &exe, "ci", &settings).unwrap();
        assert_eq!(
            spec.args,
            [
                "serve",
                "--profile",
                "ci",
                "--allow-write",
                "--allow-destructive",
                "--toolsets",
                "releases,reviews",
                "--dynamic-toolsets"
            ]
        );
    }

    #[test]
    fn server_spec_destructive_implies_write() {
        let exe = PathBuf::from(TEST_EXE);
        let settings = ProfileSettings {
            allow_destructive: true,
            ..ProfileSettings::default()
        };
        let spec = ServerSpec::new("rustore", &exe, "ci", &settings).unwrap();
        assert_eq!(
            spec.args,
            [
                "serve",
                "--profile",
                "ci",
                "--allow-write",
                "--allow-destructive"
            ]
        );
    }

    #[test]
    fn server_spec_requires_absolute_path_and_valid_name() {
        let settings = ProfileSettings::default();
        assert!(ServerSpec::new("rustore", Path::new("rustore-mcp"), "x", &settings).is_err());
        let exe = PathBuf::from(TEST_EXE);
        for bad in ["", "-x", "a b", "a.b", "a/b", "рустор", &"x".repeat(18)] {
            assert!(ServerSpec::new(bad, &exe, "x", &settings).is_err(), "{bad}");
        }
        for good in ["rustore", "rustore_2", "rustore-codex", &"x".repeat(17)] {
            validate_server_name(good).unwrap();
        }
        let err = validate_server_name("rustore-claude-code").unwrap_err();
        assert!(err.to_string().contains("--name"), "{err}");
    }

    #[test]
    fn default_server_names() {
        assert_eq!(MAX_SERVER_NAME_LEN, 17);
        assert_eq!(default_server_name("default"), "rustore");
        assert_eq!(default_server_name("ci"), "rustore-ci");
        assert_eq!(default_server_name("a.b"), "rustore-a-b");
    }

    #[test]
    fn recognizes_own_command() {
        assert!(is_own_command("/opt/homebrew/bin/rustore-mcp"));
        assert!(is_own_command(r"C:\tools\rustore-mcp.exe"));
        assert!(is_own_command("rustore-mcp"));
        // Пути Windows: регистр не важен, разделители `\` и `/`, путь из `%LOCALAPPDATA%`.
        assert!(is_own_command(r"C:\Tools\RUSTORE-MCP.EXE"));
        assert!(is_own_command(
            r"C:\Users\u\AppData\Local\Programs\rustore-mcp\rustore-mcp.exe"
        ));
        assert!(is_own_command("C:/tools/RuStore-Mcp.Exe"));
        assert!(is_own_command("rustore-mcp.exe"));
        assert!(!is_own_command(r"C:\tools\rustore-mcp-old.exe"));
        assert!(!is_own_command(r"C:\tools\rustore-mcp.exe.bak"));
        assert!(!is_own_command(r"C:\rustore-mcp\node.exe"));
        // Без `.exe` имя сравнивается точно: в Unix это другой файл.
        assert!(!is_own_command("/usr/local/bin/RuStore-MCP"));
        assert!(!is_own_command(".exe"));
        assert!(!is_own_command("рустор.exe"));
        assert!(!is_own_command("/usr/bin/node"));
        assert!(!is_own_command("/opt/legacy-rustore/dist/index.js"));
        assert!(!is_own_command("legacy_rustore_mcp"));
    }

    #[test]
    fn shell_join_quotes_unsafe_args() {
        assert_eq!(
            shell_join(["/My Apps/bin", "it's", "plain-arg"]),
            r"'/My Apps/bin' 'it'\''s' plain-arg"
        );
    }
}
