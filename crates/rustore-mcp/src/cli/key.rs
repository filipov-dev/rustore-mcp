//! `rustore-mcp key exec` — ключ RuStore API профиля для внешних инструментов (Gradle-плагин
//! публикации RuStore, fastlane, скрипты) временным файлом.
//!
//! Формат файла — JSON `{"key_id": "…", "client_secret": "…"}`, где `client_secret` —
//! приватный ключ строкой Base64 (DER PKCS#8, как его показывает консоль RuStore). Это файл
//! `credentialsPath` Gradle-плагина публикации RuStore; скрипты и fastlane читают из него
//! поля (`jq -r .client_secret`), а `auth login --key-file` и `serve --http --key-file`
//! принимают его как есть. В хранилище ключ лежит в том же виде, только с полями `keyId` и
//! `privateKey`.
//!
//! Ключ пишется в новый файл `rustore-credentials.json` с правами 0600 в новом каталоге с
//! правами 0700, команда запускается без оболочки с путём к файлу в переменной окружения (по
//! умолчанию [`KEY_FILE_ENV`]) и `keyId` в [`KEY_ID_ENV`]; её stdout и stderr проходят через
//! [`mask::Masker`]. После завершения команды файл и каталог удаляются — и при ошибке, и при
//! ненулевом коде, и при Ctrl-C/SIGTERM: Ctrl-C команда получает от терминала сама (общая
//! группа процессов), а мы его только переживаем; SIGTERM и SIGHUP пересылаются команде.
//! Обработчики сигналов ставятся до записи файла. От SIGKILL защиты нет: файл останется во
//! временном каталоге пользователя, доступный только ему.
//!
//! На Windows вместо режимов — ACL только для текущего пользователя, вместо пересылки
//! сигналов — объект задания для дерева процессов команды (модуль `windows`).
//!
//! Приватный ключ в переменные окружения не кладётся — только путь к файлу; унаследованная
//! переменная с содержимым ключа ([`KEY_CONTENT_ENVS`]) убирается. Аргументы команды мы не
//! раскрываем: `$RUSTORE_KEY_FILE` раскрывает оболочка дочерней команды (`-- sh -c '…'`).

mod mask;
#[cfg(windows)]
mod windows;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use rustore_api::ApiKey;
use rustore_api::auth::{AuthError, KeyStore, KeyringStore};
use serde::Serialize;
use tokio::io::AsyncWrite;
use tokio::process::Command;
use zeroize::Zeroizing;

use self::mask::{Masker, needles_for_key, pump};
use crate::profiles::{DEFAULT_PROFILE, validate_profile_name};

/// Путь к файлу ключа.
pub const KEY_FILE_ENV: &str = "RUSTORE_KEY_FILE";
/// `keyId` ключа: не секрет, его же принимает fastlane-плагин RuStore.
pub const KEY_ID_ENV: &str = "RUSTORE_KEY_ID";
/// Переменные с содержимым приватного ключа: унаследованные от родителя убираются, чтобы
/// инструмент не взял вместо файла чужой ключ (fastlane-плагин RuStore).
pub const KEY_CONTENT_ENVS: [&str; 1] = ["RUSTORE_PRIVATE_KEY"];

/// Имя файла ключа.
const KEY_FILE_NAME: &str = "rustore-credentials.json";
/// Префикс имени временного каталога.
const TEMP_DIR_PREFIX: &str = "rustore-mcp-key-";

const EXEC_HELP: &str = "\
The RuStore API key of the profile is written to a new temporary file rustore-credentials.json \
(mode 0600, in a new directory with mode 0700) as JSON {\"key_id\": ..., \"client_secret\": ...}, \
where client_secret is the private key in Base64 as the RuStore console shows it. This is the \
credentials file of the RuStore publishing Gradle plugin (credentialsPath); auth login \
--key-file accepts it too. The command runs with these environment variables:
  RUSTORE_KEY_FILE  path to the key file
  RUSTORE_KEY_ID    key ID (keyId) of the key
--env NAME (repeatable) sets the path in NAME instead of RUSTORE_KEY_FILE, which is then removed \
from the environment of the command. The private key itself is never put into the environment; \
an inherited RUSTORE_PRIVATE_KEY is removed.

The file and its directory are deleted when the command exits, also when it fails or is \
interrupted: Ctrl-C reaches the command through the terminal, SIGTERM and SIGHUP are forwarded \
to it. The exit code of the command is returned.

On Windows the directory and the file are accessible only to the current user (the directory \
does not inherit permissions), the command and the processes it starts run in a job object: \
when the command exits or the console window is closed, the remaining processes are stopped \
and the file is deleted. A command without an extension is also looked up as .cmd and .bat \
(gradlew.bat).

The command runs directly, without a shell; its stdin is inherited. The key (the whole file, \
the private key as a whole, in lines and in base64), the key ID and RuStore access tokens \
(JWE) are replaced with *** in its stdout and stderr. To use the variables in arguments, run a \
shell yourself, in single quotes, so that the child shell expands it:
  rustore-mcp key exec --profile work -- sh -c \\
    './gradlew publishRustoreRelease --credentialsPath \"$RUSTORE_KEY_FILE\"'
  rustore-mcp key exec --profile work -- sh -c \\
    'RUSTORE_PRIVATE_KEY=\"$(jq -r .client_secret \"$RUSTORE_KEY_FILE\")\" fastlane android rustore'
  rustore-mcp key exec --profile work --env CREDENTIALS_PATH -- ./scripts/upload.sh

Never print the key: masking is a safety net against accidental output, not a security \
boundary.";

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Run a command with the RuStore API key of the profile in a temporary file (mode 0600)
    /// that is deleted when the command exits.
    #[command(after_help = EXEC_HELP)]
    Exec(ExecArgs),
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Environment variable for the path to the key file (repeatable). Replaces the default
    /// RUSTORE_KEY_FILE.
    #[arg(long = "env", value_name = "NAME")]
    pub env: Vec<String>,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

pub async fn run(command: KeyCommand) -> anyhow::Result<ExitCode> {
    match command {
        KeyCommand::Exec(args) => {
            // Всё, что может не пройти, — до создания файла.
            let names = path_env_names(&args.env)?;
            let key = load_key(&KeyringStore, &args.profile)?;
            // Обработчики ставятся до создания файла: сигнал между записью ключа и запуском
            // команды не должен убить процесс, оставив файл на диске.
            let mut signals = ProcessSignals::new()?;
            let code = exec_with_key(
                &std::env::temp_dir(),
                &key,
                &names,
                &args.command,
                tokio::io::stdout(),
                tokio::io::stderr(),
                &mut signals,
            )
            .await?;
            Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
        }
    }
}

/// Ключ профиля из хранилища с понятной подсказкой, если его нет.
pub fn load_key(store: &dyn KeyStore, profile: &str) -> anyhow::Result<ApiKey> {
    validate_profile_name(profile)?;
    match store.get(profile) {
        Ok(key) => Ok(key),
        Err(AuthError::NotFound { .. }) => bail!(
            "no RuStore API key is stored for profile `{profile}`; store one with \
             `rustore-mcp auth login --profile {profile} --key-file <key-file>`"
        ),
        Err(err) => Err(err.into()),
    }
}

/// Проверяет имя переменной окружения: латинские буквы, цифры и `_`, не с цифры.
fn validate_env_name(name: &str) -> anyhow::Result<()> {
    let valid = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.chars().next().is_some_and(|c| !c.is_ascii_digit());
    if !valid {
        bail!(
            "invalid environment variable name `{name}`: use A-Z, a-z, 0-9 and `_`, not \
             starting with a digit"
        );
    }
    if KEY_CONTENT_ENVS.contains(&name) {
        bail!(
            "`{name}` holds the private key, not a path; key exec removes it and passes only \
             the path to the key file"
        );
    }
    if name == KEY_ID_ENV {
        bail!("`{name}` holds the key ID; choose another name for the path to the key file");
    }
    Ok(())
}

/// Переменные для пути к файлу: из `--env` (без повторов) или по умолчанию.
pub fn path_env_names(requested: &[String]) -> anyhow::Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(vec![KEY_FILE_ENV.to_owned()]);
    }
    let mut names: Vec<String> = Vec::new();
    for name in requested {
        validate_env_name(name)?;
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    Ok(names)
}

/// Сигнал, который пересылается дочерней команде.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forward {
    #[cfg(unix)]
    Terminate,
    #[cfg(unix)]
    Hangup,
    /// Windows: закрывается консоль, сеанс или система — завершить дерево процессов команды.
    #[cfg(windows)]
    Close,
}

/// Источник сигналов для пересылки; в тестах подменяется каналом, чтобы не трогать
/// обработчики сигналов процесса тестов.
pub trait SignalSource {
    /// Следующий сигнал для пересылки; `None` — сигналов больше не будет.
    async fn recv(&mut self) -> Option<Forward>;
}

/// Сигналы процесса.
#[cfg(unix)]
pub struct ProcessSignals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ProcessSignals {
    pub fn new() -> anyhow::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let context = "cannot install signal handlers";
        Ok(Self {
            term: signal(SignalKind::terminate()).context(context)?,
            hup: signal(SignalKind::hangup()).context(context)?,
            int: signal(SignalKind::interrupt()).context(context)?,
        })
    }
}

#[cfg(unix)]
impl SignalSource for ProcessSignals {
    async fn recv(&mut self) -> Option<Forward> {
        loop {
            tokio::select! {
                got = self.term.recv() => return got.map(|()| Forward::Terminate),
                got = self.hup.recv() => return got.map(|()| Forward::Hangup),
                // Ctrl-C команда получает от терминала сама; повторная пересылка дала бы ей
                // два сигнала. Сами переживаем его, чтобы дождаться команды и удалить файл.
                got = self.int.recv() => got?,
            }
        }
    }
}

#[cfg(windows)]
use self::windows::ChildTree;
#[cfg(windows)]
pub use self::windows::ProcessSignals;

/// Дочерняя команда для пересылки сигналов (на Unix — по PID).
#[cfg(unix)]
struct ChildTree {
    pid: Option<u32>,
}

#[cfg(unix)]
impl ChildTree {
    fn adopt(child: &tokio::process::Child) -> Self {
        Self { pid: child.id() }
    }

    /// Пересылает сигнал дочернему процессу. Процесс ещё не «пожат» (`wait` не завершён),
    /// поэтому его PID не может достаться другому процессу.
    fn forward(&mut self, signal: Forward) {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) else {
            return;
        };
        let signal = match signal {
            Forward::Terminate => Signal::SIGTERM,
            Forward::Hangup => Signal::SIGHUP,
        };
        // Процесс мог уже завершиться: ошибка `ESRCH` не важна.
        let _ = kill(Pid::from_raw(pid), signal);
    }

    /// Команда завершилась: потомки, которые она оставила, на Unix продолжают работать.
    fn release(&mut self) {
        self.pid = None;
    }
}

/// Предупреждение в stderr. Сбой записи не важен (на Windows консоль могла уже закрыться),
/// а `eprintln!` в этом случае паникует.
fn warn(message: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "rustore-mcp: warning: {message}");
}

/// Попыток удалить каталог ключа. На Windows файл, открытый другим процессом (антивирус,
/// индексатор), удалить нельзя, пока его не закроют: несколько повторов с паузой.
const REMOVE_ATTEMPTS: u32 = if cfg!(windows) { 10 } else { 1 };
const REMOVE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Содержимое файла ключа: поля файла `credentialsPath` Gradle-плагина публикации RuStore.
#[derive(Serialize)]
struct CredentialsFile<'a> {
    key_id: &'a str,
    client_secret: &'a str,
}

/// Текст файла ключа (с переводом строки в конце). Не выводить и не логировать.
pub fn key_file_text(key: &ApiKey) -> Zeroizing<String> {
    let file = CredentialsFile {
        key_id: key.key_id().as_str(),
        client_secret: key.expose_private_key_base64(),
    };
    // Сериализация структуры из строк не может завершиться ошибкой.
    let mut text = Zeroizing::new(serde_json::to_string_pretty(&file).unwrap_or_default());
    text.push('\n');
    text
}

/// Временный файл ключа в собственном каталоге. Каталог удаляется и при `drop` (паника,
/// ранний выход), но обычный путь — [`KeyFile::remove`] с проверкой результата.
pub struct KeyFile {
    dir: tempfile::TempDir,
    path: PathBuf,
}

impl KeyFile {
    /// Создаёт в `root` новый каталог (0700) и в нём новый файл `rustore-credentials.json`
    /// (0600) с ключом.
    pub fn create(root: &Path, key: &ApiKey) -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix(TEMP_DIR_PREFIX)
            .tempdir_in(root)
            .context("cannot create a temporary directory for the key file")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
                .context("cannot restrict the temporary directory to the current user")?;
        }
        // Права каталога — до создания файла: файл наследует их при создании.
        #[cfg(windows)]
        windows::restrict_to_current_user(dir.path())
            .context("cannot restrict the temporary directory to the current user")?;
        let path = dir.path().join(KEY_FILE_NAME);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("cannot create `{}`", path.display()))?;
        // Права задаются явно ещё раз: `mode` при создании урезается umask (только сужает,
        // но так права не зависят от umask вовсе).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .with_context(|| format!("cannot set the mode of `{}`", path.display()))?;
        }
        file.write_all(key_file_text(key).as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        Ok(Self { dir, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Удаляет файл и каталог.
    pub fn remove(self) -> anyhow::Result<()> {
        let dir = self.dir.keep();
        let mut attempt = 1;
        loop {
            match fs::remove_dir_all(&dir) {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(_) if attempt < REMOVE_ATTEMPTS => {
                    attempt += 1;
                    std::thread::sleep(REMOVE_RETRY_DELAY);
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "could not delete the temporary key directory {}; delete it yourself",
                            dir.display()
                        )
                    });
                }
            }
        }
    }
}

/// Переменные окружения команды: что задать и что убрать из унаследованных.
pub struct KeyEnv {
    pub set: Vec<(String, OsString)>,
    pub remove: Vec<String>,
}

/// Путь к файлу — в каждой из `names`, `keyId` — в [`KEY_ID_ENV`]; переменные с содержимым
/// ключа и не выбранная переменная по умолчанию (она указывала бы на другой ключ)
/// убираются.
pub fn key_env(file: &KeyFile, key: &ApiKey, names: &[String]) -> KeyEnv {
    let path = file.path().as_os_str().to_os_string();
    let mut set: Vec<(String, OsString)> = names
        .iter()
        .map(|name| (name.clone(), path.clone()))
        .collect();
    set.push((KEY_ID_ENV.to_owned(), key.key_id().as_str().into()));
    let remove = KEY_CONTENT_ENVS
        .iter()
        .chain([KEY_FILE_ENV].iter())
        .map(|name| (*name).to_owned())
        .filter(|name| !names.contains(name))
        .collect();
    KeyEnv { set, remove }
}

/// Пишет ключ во временный файл в `temp_root`, запускает команду и удаляет файл. Возвращает
/// код завершения команды (для завершения сигналом на Unix — `128 + номер сигнала`).
pub async fn exec_with_key<O, E, S>(
    temp_root: &Path,
    key: &ApiKey,
    names: &[String],
    command: &[OsString],
    stdout: O,
    stderr: E,
    signals: &mut S,
) -> anyhow::Result<i32>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
    S: SignalSource,
{
    let Some((program, args)) = command.split_first() else {
        bail!("no command given: pass it after `--`");
    };
    let file = KeyFile::create(temp_root, key)?;
    let env = key_env(&file, key, names);
    let needles = needles_for_key(key, &key_file_text(key));
    let result = run_masked(program, args, &env, &needles, stdout, stderr, signals).await;
    let removed = file.remove();
    match (result, removed) {
        (Ok(code), Ok(())) => Ok(code),
        (Ok(code), Err(err)) => Err(err.context(format!("the command exited with code {code}"))),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(removal)) => Err(err.context(format!("{removal:#}"))),
    }
}

/// Запускает команду, копирует её вывод с маскированием и пересылает ей сигналы.
async fn run_masked<O, E, S>(
    program: &OsString,
    args: &[OsString],
    env: &KeyEnv,
    needles: &[Vec<u8>],
    stdout: O,
    stderr: E,
    signals: &mut S,
) -> anyhow::Result<i32>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
    S: SignalSource,
{
    let command = |program: &OsString| {
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(env.set.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in &env.remove {
            command.env_remove(name);
        }
        command
    };
    #[cfg(windows)]
    let spawned = windows::spawn_with_fallbacks(program, |program| command(program).spawn());
    #[cfg(not(windows))]
    let spawned = command(program).spawn();
    let mut child =
        spawned.with_context(|| format!("cannot run `{}`", program.to_string_lossy()))?;
    let mut tree = ChildTree::adopt(&child);
    let child_out = child.stdout.take().context("child stdout is not piped")?;
    let child_err = child.stderr.take().context("child stderr is not piped")?;

    let wait = async {
        let mut signals_open = true;
        loop {
            tokio::select! {
                status = child.wait() => {
                    // На Windows закрытие задания завершает оставленных командой потомков:
                    // они держали бы файл ключа и вывод команды открытыми.
                    tree.release();
                    return status;
                }
                signal = signals.recv(), if signals_open => match signal {
                    Some(signal) => tree.forward(signal),
                    None => signals_open = false,
                },
            }
        }
    };
    let (status, out_result, err_result) = tokio::join!(
        wait,
        pump(child_out, stdout, Masker::new(needles.iter().cloned())),
        pump(child_err, stderr, Masker::new(needles.iter().cloned())),
    );
    for (stream, result) in [("stdout", out_result), ("stderr", err_result)] {
        if let Err(err) = result {
            // Закрытый канал (`| head`) — обычная ситуация конвейера.
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                warn(&format!("copying the command {stream} failed: {err}"));
            }
        }
    }
    let status = status.context("waiting for the command failed")?;
    Ok(exit_code(&status))
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod name_tests {
    use rustore_api::auth::MemoryStore;
    use rustore_api::testing;

    use super::*;

    #[test]
    fn env_names() {
        assert_eq!(path_env_names(&[]).unwrap(), [KEY_FILE_ENV]);
        assert_eq!(
            path_env_names(&["CREDENTIALS".into(), "_X1".into(), "CREDENTIALS".into()]).unwrap(),
            ["CREDENTIALS", "_X1"]
        );
        for bad in [
            "",
            "1KEY",
            "A-B",
            "A B",
            "KEY=1",
            "RUSTORE_PRIVATE_KEY",
            "RUSTORE_KEY_ID",
        ] {
            assert!(path_env_names(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_file_is_the_gradle_plugin_format_and_auth_login_reads_it() {
        let key = testing::api_key();
        let text = key_file_text(&key);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["key_id"], testing::TEST_KEY_ID);
        assert_eq!(value["client_secret"], testing::private_key_base64());
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert!(text.ends_with('\n'));
        // Тот же файл принимает `auth login --key-file`.
        assert_eq!(ApiKey::parse_key_file(&text, None).unwrap(), Some(key));
    }

    #[test]
    fn missing_key_is_explained() {
        let store = MemoryStore::new();
        let err = load_key(&store, "work").unwrap_err().to_string();
        assert!(err.contains("auth login --profile work"), "{err}");
        assert!(load_key(&store, "-bad").is_err());
        store.set("work", &testing::api_key()).unwrap();
        assert_eq!(load_key(&store, "work").unwrap(), testing::api_key());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use rustore_api::testing;
    use tokio::sync::mpsc;

    use super::*;

    /// Сигналы из канала: обработчики сигналов процесса тестов не трогаются.
    struct ChannelSignals(mpsc::UnboundedReceiver<Forward>);

    impl SignalSource for ChannelSignals {
        async fn recv(&mut self) -> Option<Forward> {
            self.0.recv().await
        }
    }

    fn channel() -> (mpsc::UnboundedSender<Forward>, ChannelSignals) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, ChannelSignals(rx))
    }

    fn strings(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    struct Run {
        code: i32,
        out: String,
        err: String,
    }

    /// Запуск во временном корне; после запуска в корне ничего не должно остаться.
    struct Exec {
        root: tempfile::TempDir,
        key: ApiKey,
        names: Vec<String>,
    }

    impl Exec {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
                key: testing::api_key(),
                names: path_env_names(&[]).unwrap(),
            }
        }

        async fn command(
            &self,
            command: &[OsString],
            signals: &mut ChannelSignals,
        ) -> anyhow::Result<Run> {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let result = exec_with_key(
                self.root.path(),
                &self.key,
                &self.names,
                command,
                &mut out,
                &mut err,
                signals,
            )
            .await;
            assert!(
                fs::read_dir(self.root.path()).unwrap().next().is_none(),
                "the temporary key directory is left behind"
            );
            Ok(Run {
                code: result?,
                out: String::from_utf8(out).unwrap(),
                err: String::from_utf8(err).unwrap(),
            })
        }

        async fn sh(&self, script: &str) -> Run {
            let (_tx, mut signals) = channel();
            self.command(&strings(&["sh", "-c", script]), &mut signals)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn child_gets_env_and_private_key_file() {
        let exec = Exec::new();
        // Путь печатается до проверок: маскировщик не трогает его, а keyId — трогает.
        let run = exec
            .sh(r#"echo "$RUSTORE_KEY_FILE"
test -f "$RUSTORE_KEY_FILE" && echo exists
ls -ld "$RUSTORE_KEY_FILE" "$(dirname "$RUSTORE_KEY_FILE")" | cut -c1-10
grep -c '"client_secret"' "$RUSTORE_KEY_FILE"
echo "id=$RUSTORE_KEY_ID""#)
            .await;
        assert_eq!(run.code, 0, "{}", run.err);
        let path = PathBuf::from(run.out.lines().next().unwrap());
        assert_eq!(path.file_name().unwrap(), KEY_FILE_NAME);
        let dir = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(dir.starts_with(TEMP_DIR_PREFIX), "{dir}");
        // Файл существовал во время команды и удалён после.
        assert!(!path.exists());
        let text = run.out;
        assert!(text.contains("exists"), "{text}");
        assert!(text.contains("-rw-------"), "{text}");
        assert!(text.contains("drwx------"), "{text}");
        assert!(text.contains("\n1\n"), "{text}");
        // keyId передан, но в выводе скрыт.
        assert!(text.ends_with("id=***\n"), "{text}");
    }

    #[tokio::test]
    async fn custom_env_names_replace_the_default() {
        let mut exec = Exec::new();
        exec.names = vec!["CREDENTIALS_PATH".into()];
        let run = exec
            .sh(r#"test -f "$CREDENTIALS_PATH" && echo file; echo "[${RUSTORE_KEY_FILE-unset}] [${RUSTORE_KEY_ID:+set}]""#)
            .await;
        assert_eq!(run.out, "file\n[unset] [set]\n", "{}", run.err);
    }

    #[tokio::test]
    async fn child_reads_the_key_and_output_is_masked() {
        let exec = Exec::new();
        let base64 = exec.key.expose_private_key_base64().to_owned();
        let token = [
            "eyJhbGciOiJSU0EtT0FFUCJ9",
            "c2Vjb25k",
            "dGhpcmQ",
            "Zm91cnRo",
            "Zmlm",
        ]
        .join(".");
        let script = format!(
            r#"f="$RUSTORE_KEY_FILE"
cat "$f"; echo; grep key_id "$f" >&2
grep client_secret "$f"
base64 < "$f" | tr -d '\n'; echo
sed -n 's/.*"client_secret": "\(.*\)".*/\1/p' "$f" | fold -w 64 | sed -n 2p
echo "token {token} end"
wc -c < "$f"
env | grep -c 'MII'"#
        );
        let run = exec.sh(&script).await;
        assert_eq!(run.code, 1, "grep -c finds nothing: {}", run.err);
        let lines: Vec<&str> = run.out.lines().collect();
        // Весь файл, приватный ключ в JSON, base64 файла, строки по 64 символа и токен
        // скрыты.
        assert_eq!(lines[0], "***", "{}", run.out);
        assert_eq!(lines[1], r#"  "client_secret": "***""#, "{}", run.out);
        assert!(lines[2].starts_with("***"), "{}", run.out);
        assert!(!lines[3].contains(&base64[64..96]), "{}", run.out);
        assert_eq!(lines[4], "token *** end");
        // Размер файла — как у текста ключа; в окружении ключа нет.
        assert_eq!(lines[5].trim(), key_file_text(&exec.key).len().to_string());
        assert_eq!(lines[6].trim(), "0");
        assert_eq!(run.err, "  \"key_id\": \"***\",\n");
        for secret in [&base64[..40], testing::TEST_KEY_ID, "c2Vjb25k"] {
            assert!(!run.out.contains(secret), "{secret}: {}", run.out);
        }
    }

    #[test]
    fn inherited_key_variables_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let key = testing::api_key();
        let file = KeyFile::create(root.path(), &key).unwrap();
        let env = key_env(&file, &key, &path_env_names(&[]).unwrap());
        assert_eq!(env.remove, KEY_CONTENT_ENVS);
        let names: Vec<&str> = env.set.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, [KEY_FILE_ENV, KEY_ID_ENV]);
        assert_eq!(env.set[0].1, file.path().as_os_str());
        assert_eq!(env.set[1].1, testing::TEST_KEY_ID);

        let custom = key_env(&file, &key, &["CREDENTIALS_PATH".into()]);
        assert_eq!(custom.remove, ["RUSTORE_PRIVATE_KEY", KEY_FILE_ENV]);
        file.remove().unwrap();
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn exit_code_is_returned_and_file_is_removed_after_failure() {
        let exec = Exec::new();
        let run = exec.sh("cat \"$RUSTORE_KEY_FILE\" >&2; exit 7").await;
        assert_eq!(run.code, 7);
        assert_eq!(run.err, "***");
        assert_eq!(exec.sh("exit 0").await.code, 0);
        assert_eq!(exec.sh("exit 255").await.code, 255);

        // Команда не запустилась: ошибка, файла тоже нет (это проверяет `Exec::command`).
        let (_tx, mut signals) = channel();
        let err = exec
            .command(&strings(&["/nonexistent/program"]), &mut signals)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("cannot run `/nonexistent/program`"), "{err}");

        let err = exec
            .command(&[], &mut signals)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("no command"), "{err}");
    }

    #[tokio::test]
    async fn signals_are_forwarded_and_file_is_removed() {
        let exec = Exec::new();
        let (tx, mut signals) = channel();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.send(Forward::Terminate).unwrap();
            // Канал остаётся открытым, пока команда не завершится.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let started = std::time::Instant::now();
        let run = exec
            .command(&strings(&["sh", "-c", "exec sleep 20"]), &mut signals)
            .await
            .unwrap();
        sender.abort();
        assert_eq!(run.code, 128 + 15);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));

        // SIGHUP тоже пересылается.
        let (tx, mut signals) = channel();
        tx.send(Forward::Hangup).unwrap();
        let run = exec
            .command(&strings(&["sh", "-c", "exec sleep 20"]), &mut signals)
            .await
            .unwrap();
        assert_eq!(run.code, 128 + 1);

        // Закрытый источник сигналов не мешает дождаться команды.
        let (tx, mut signals) = channel();
        drop(tx);
        let run = exec
            .command(&strings(&["sh", "-c", "exit 3"]), &mut signals)
            .await
            .unwrap();
        assert_eq!(run.code, 3);
    }

    #[test]
    fn key_file_is_created_new_with_private_modes() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let key = testing::api_key();
        let file = KeyFile::create(root.path(), &key).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(file.path()), 0o600);
        assert_eq!(mode(file.path().parent().unwrap()), 0o700);
        let text = fs::read_to_string(file.path()).unwrap();
        assert_eq!(
            ApiKey::parse_key_file(&text, None).unwrap(),
            Some(key.clone())
        );
        // Второй файл — в своём каталоге, первый не перезаписывается.
        let second = KeyFile::create(root.path(), &key).unwrap();
        assert_ne!(second.path(), file.path());
        file.remove().unwrap();
        second.remove().unwrap();
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }
}
