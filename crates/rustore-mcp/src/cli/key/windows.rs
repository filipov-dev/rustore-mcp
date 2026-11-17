//! `key exec` на Windows: права на каталог ключа, дерево процессов команды и консольные
//! сигналы.
//!
//! - Права. Режимов 0600/0700 на Windows нет: у каталога ключа отключается наследование
//!   ACL и остаётся одна запись — полный доступ текущего пользователя (по SID) с
//!   наследованием на файлы. Файл ключа создаётся в каталоге после этого и получает те же
//!   права. Права меняет системный `icacls.exe`, SID даёт `whoami.exe /user`: так не нужен
//!   `unsafe` для вызовов Win32.
//! - Дерево процессов. Сигналов Unix нет, а `TerminateProcess` завершает только сам процесс.
//!   Команда включается в объект задания (Job Object) с `KILL_ON_JOB_CLOSE`: закрытие
//!   задания завершает команду и всех её потомков — когда закрывается консоль, когда
//!   команда завершилась (оставшиеся потомки не держат файл ключа открытым, и его можно
//!   удалить) и когда наш процесс завершён снаружи (система закрывает его дескрипторы).
//!   Потомки, запущенные командой до включения в задание (первые миллисекунды), в задание
//!   не попадают.
//! - Сигналы. Ctrl+C и Ctrl+Break консоль доставляет всем процессам консоли, в том числе
//!   команде, — их только переживаем. Закрытие окна консоли, выход из системы и выключение:
//!   tokio не возвращается из обработчика (система ждёт около 5 секунд), за это время
//!   задание закрывается, команда завершается и файл удаляется.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use tokio::signal::windows::{
    CtrlBreak, CtrlC, CtrlClose, CtrlLogoff, CtrlShutdown, ctrl_break, ctrl_c, ctrl_close,
    ctrl_logoff, ctrl_shutdown,
};
use win32job::{ExtendedLimitInfo, Job};

use super::{Forward, SignalSource};

/// Системная программа из `%SystemRoot%\System32`: не из PATH и не из текущего каталога.
fn system_program(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .filter(|root| !root.is_empty())
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    root.join("System32").join(name)
}

/// SID из вывода `whoami /user /fo csv /nh`: `"DOMAIN\user","S-1-5-21-…"`.
fn parse_whoami_sid(output: &str) -> Option<String> {
    let sid = output.trim().rsplit(',').next()?.trim().trim_matches('"');
    let valid = sid.starts_with("S-1-")
        && sid.len() > 4
        && sid
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == 'S');
    valid.then(|| sid.to_owned())
}

/// SID текущего пользователя.
fn current_user_sid() -> anyhow::Result<String> {
    let whoami = system_program("whoami.exe");
    let output = Command::new(&whoami)
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .with_context(|| format!("cannot run `{}`", whoami.display()))?;
    if !output.status.success() {
        bail!(
            "`whoami /user` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_whoami_sid(&String::from_utf8_lossy(&output.stdout))
        .context("`whoami /user` did not print the SID of the current user")
}

/// Аргументы `icacls`: без унаследованных записей, полный доступ только пользователю `sid`,
/// с наследованием на файлы (`OI`) и подкаталоги (`CI`).
fn icacls_args(dir: &Path, sid: &str) -> Vec<OsString> {
    vec![
        dir.as_os_str().to_owned(),
        "/inheritance:r".into(),
        "/grant:r".into(),
        format!("*{sid}:(OI)(CI)F").into(),
        "/Q".into(),
    ]
}

/// Оставляет доступ к пустому каталогу `dir` только текущему пользователю.
pub fn restrict_to_current_user(dir: &Path) -> anyhow::Result<()> {
    let sid = current_user_sid()?;
    let icacls = system_program("icacls.exe");
    let output = Command::new(&icacls)
        .args(icacls_args(dir, &sid))
        .output()
        .with_context(|| format!("cannot run `{}`", icacls.display()))?;
    if !output.status.success() {
        bail!(
            "`icacls` failed: {}{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Запасные имена команды: `gradlew` → `gradlew.cmd`, `gradlew.bat`. `Command` на Windows
/// ищет в PATH только `<имя>.exe`, а `gradlew`, `fastlane` (через RubyInstaller) и обёртки
/// npm — пакетные файлы. Аргументы для них Rust экранирует сам и отказывается запускать, если
/// безопасно экранировать нельзя.
pub fn batch_fallbacks(program: &OsStr) -> Vec<OsString> {
    let has_extension = Path::new(program).extension().is_some();
    if has_extension {
        return Vec::new();
    }
    [".cmd", ".bat"]
        .iter()
        .map(|ext| {
            let mut name = program.to_owned();
            name.push(ext);
            name
        })
        .collect()
}

/// Запускает `program`, а если её нет — запасные имена из [`batch_fallbacks`].
pub fn spawn_with_fallbacks<T>(
    program: &OsString,
    mut spawn: impl FnMut(&OsString) -> io::Result<T>,
) -> io::Result<T> {
    let mut result = spawn(program);
    for fallback in batch_fallbacks(program) {
        match &result {
            Err(err) if err.kind() == io::ErrorKind::NotFound => result = spawn(&fallback),
            _ => break,
        }
    }
    result
}

/// Дерево процессов команды: объект задания, который завершает его при закрытии.
pub struct ChildTree {
    job: Option<Job>,
}

impl ChildTree {
    /// Включает запущенную команду в новое задание. Если не вышло, команда работает без
    /// него: файл ключа всё равно удаляется после её завершения.
    pub fn adopt(child: &tokio::process::Child) -> Self {
        let job = (|| {
            let mut limits = ExtendedLimitInfo::new();
            limits.limit_kill_on_job_close();
            let job = Job::create_with_limit_info(&limits).context("cannot create a job object")?;
            let handle = child
                .raw_handle()
                .context("the command has already exited")?;
            job.assign_process(handle as isize)
                .context("cannot assign the command to a job object")?;
            anyhow::Ok(job)
        })();
        match job {
            Ok(job) => Self { job: Some(job) },
            Err(err) => {
                super::warn(&format!(
                    "{err:#}; processes started by the command are not stopped with it"
                ));
                Self { job: None }
            }
        }
    }

    /// Закрытие консоли или сеанса: завершить команду и её потомков.
    pub fn forward(&mut self, signal: Forward) {
        match signal {
            Forward::Close => self.release(),
        }
    }

    /// Команда завершилась (или её надо завершить): закрыть задание, завершив оставшихся
    /// потомков.
    pub fn release(&mut self) {
        drop(self.job.take());
    }
}

/// Консольные сигналы процесса.
pub struct ProcessSignals {
    ctrl_c: CtrlC,
    ctrl_break: CtrlBreak,
    close: CtrlClose,
    logoff: CtrlLogoff,
    shutdown: CtrlShutdown,
}

impl ProcessSignals {
    pub fn new() -> anyhow::Result<Self> {
        let context = "cannot install console control handlers";
        Ok(Self {
            ctrl_c: ctrl_c().context(context)?,
            ctrl_break: ctrl_break().context(context)?,
            close: ctrl_close().context(context)?,
            logoff: ctrl_logoff().context(context)?,
            shutdown: ctrl_shutdown().context(context)?,
        })
    }
}

impl SignalSource for ProcessSignals {
    async fn recv(&mut self) -> Option<Forward> {
        loop {
            tokio::select! {
                // Ctrl+C и Ctrl+Break команда получает от консоли сама.
                got = self.ctrl_c.recv() => got?,
                got = self.ctrl_break.recv() => got?,
                got = self.close.recv() => return got.map(|()| Forward::Close),
                got = self.logoff.recv() => return got.map(|()| Forward::Close),
                got = self.shutdown.recv() => return got.map(|()| Forward::Close),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whoami_sid_is_parsed() {
        assert_eq!(
            parse_whoami_sid("\"desktop-1\\jane doe\",\"S-1-5-21-1-2-3-1001\"\r\n").as_deref(),
            Some("S-1-5-21-1-2-3-1001")
        );
        for bad in ["", "\"user\"", "\"user\",\"S-1-\"", "\"u\",\"S-1-5;rm\""] {
            assert_eq!(parse_whoami_sid(bad), None, "{bad}");
        }
    }

    #[test]
    fn icacls_grants_only_the_user() {
        let args = icacls_args(
            Path::new(r"C:\Users\Jane Doe\AppData\Local\Temp\k"),
            "S-1-5-21-7",
        );
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                r"C:\Users\Jane Doe\AppData\Local\Temp\k",
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-21-7:(OI)(CI)F",
                "/Q"
            ]
        );
    }

    #[test]
    fn batch_files_are_fallbacks_for_bare_names() {
        assert_eq!(
            batch_fallbacks("gradlew".as_ref()),
            ["gradlew.cmd", "gradlew.bat"]
        );
        assert!(batch_fallbacks("fastlane.bat".as_ref()).is_empty());
        assert!(batch_fallbacks(r"C:\tools\gradle.exe".as_ref()).is_empty());
    }

    /// ACL каталога и файла ключа: одна запись — текущий пользователь (PowerShell Get-Acl).
    #[test]
    fn key_directory_is_private_to_the_user() {
        let root = tempfile::tempdir().unwrap();
        let key = rustore_api::testing::api_key();
        let file = super::super::KeyFile::create(root.path(), &key).unwrap();
        let sid = current_user_sid().unwrap();
        for path in [file.path(), file.path().parent().unwrap()] {
            let script = format!(
                "$acl = Get-Acl -LiteralPath '{}'; \
                 $acl.AreAccessRulesProtected; \
                 $acl.Access | ForEach-Object {{ \
                   $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value \
                 }}",
                path.display().to_string().replace('\'', "''")
            );
            let output = Command::new(system_program(r"WindowsPowerShell\v1.0\powershell.exe"))
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let text = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            // Каталог защищён от наследования; у файла записи только унаследованные от него.
            if path == file.path().parent().unwrap() {
                assert_eq!(lines.first().copied(), Some("True"), "{text}");
            }
            assert!(
                lines[1..].iter().all(|line| *line == sid),
                "{path:?}: {text}"
            );
            assert!(lines.len() >= 2, "{text}");
        }
        file.remove().unwrap();
    }

    /// Источник сигналов без сигналов.
    struct NoSignals;

    impl SignalSource for NoSignals {
        async fn recv(&mut self) -> Option<Forward> {
            std::future::pending().await
        }
    }

    /// Пакетный файл без расширения в пути с пробелами: запуск, путь к ключу в окружении,
    /// маскирование вывода и удаление файла.
    #[tokio::test]
    async fn exec_runs_a_batch_file_and_removes_the_key() {
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::Builder::new()
            .prefix("with space ")
            .tempdir()
            .unwrap();
        std::fs::write(
            bin.path().join("show-key.cmd"),
            "@echo off\r\necho %RUSTORE_KEY_FILE%\r\ntype \"%RUSTORE_KEY_FILE%\"\r\nexit /b 5\r\n",
        )
        .unwrap();
        let key = rustore_api::testing::api_key();
        let names = super::super::path_env_names(&[]).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = super::super::exec_with_key(
            root.path(),
            &key,
            &names,
            &[bin.path().join("show-key").into_os_string()],
            &mut out,
            &mut err,
            &mut NoSignals,
        )
        .await
        .unwrap();
        let out = String::from_utf8_lossy(&out);
        assert_eq!(code, 5, "{out} {}", String::from_utf8_lossy(&err));
        let path = PathBuf::from(out.lines().next().unwrap().trim());
        assert!(path.ends_with("rustore-credentials.json"), "{out}");
        assert!(!path.exists());
        assert!(out.contains("***"), "{out}");
        assert!(!out.contains(key.expose_private_key_base64()), "{out}");
        assert!(!out.contains(rustore_api::testing::TEST_KEY_ID), "{out}");
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }

    /// Команда и её потомки завершаются с закрытием задания.
    #[tokio::test]
    async fn closing_the_job_stops_the_command_tree() {
        let mut child = tokio::process::Command::new(system_program("cmd.exe"))
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        let mut tree = ChildTree::adopt(&child);
        assert!(tree.job.is_some());
        let started = std::time::Instant::now();
        tree.forward(Forward::Close);
        // Код завершения при закрытии задания не задан: важно только, что ping не ждали.
        child.wait().await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
