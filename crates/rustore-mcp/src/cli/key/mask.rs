//! Маскирование ключа RuStore API и токенов в потоковом выводе дочерней команды
//! (`key exec`).
//!
//! Образцы для поиска ([`needles_for_key`]):
//!
//! - текст файла ключа целиком (с завершающим переводом строки и без) и его base64;
//! - приватный ключ (Base64 DER PKCS#8) целиком, его base64 и куски по 64 и 76 символов —
//!   так ключ выглядит в строках PEM (`openssl`) и в base64 с переносами;
//! - `keyId`: он не секрет, но относится к кабинету компании и в логи не пишется.
//!
//! Так `cat "$RUSTORE_KEY_FILE"`, `grep client_secret …`, вывод отдельных строк и
//! `base64 < …` показывают `***`. Строки `-----BEGIN/END PRIVATE KEY-----` сами по себе не
//! секрет и не маскируются.
//!
//! Кроме образцов маскируются токены доступа RuStore (JWE) и другие токены JWT/JWE: от
//! префикса `eyJ` (начало заголовка `{"` в base64url) до конца символов токена (буквы, цифры,
//! `-`, `_`, `.`) — дочерняя команда получает их по ключу сама.
//!
//! Алгоритм ([`Masker`]) — как в соседних серверах:
//!
//! - Ищутся все вхождения всех образцов, в том числе перекрывающиеся. Пересекающиеся и
//!   соседние вхождения сливаются в одну область, и вся область заменяется одним [`MASK`]:
//!   так при перекрытии двух образцов не остаётся видимого «хвоста» ни одного из них.
//! - Вывод приходит кусками. Позиция в буфере «решена», если любой образец (или префикс
//!   токена), начинающийся в ней, либо целиком помещается в буфер, либо уже не совпадает.
//!   Нерешённый хвост (короче самого длинного образца и совпадает с началом какого-то
//!   образца) переносится в следующий кусок, поэтому значение, разрезанное границей кусков,
//!   всё равно маскируется. Хвост, который не может быть началом значения, выводится сразу:
//!   приглашения без перевода строки не задерживаются.
//! - Открытая область маски и «внутри токена» тоже переносятся между кусками: [`MASK`]
//!   выводится один раз, а токен, разрезанный границей кусков, скрывается целиком.
//!
//! Маскирование — страховка от случайной утечки, а не граница безопасности: ключ,
//! преобразованный дочерней командой (hex, PKCS#1, другая разбивка на строки), не
//! распознаётся.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use rustore_api::ApiKey;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Чем заменяется секрет в выводе.
pub const MASK: &[u8] = b"***";

/// Префиксы токенов: после префикса скрываются все символы токена.
const TOKEN_PREFIXES: [&[u8]; 1] = [b"eyJ"];

/// Образцы короче этого не маскируются: короткая строка совпадёт с чем угодно и испортит
/// вывод.
const MIN_MASK_LEN: usize = 4;

/// Ширина строк, на которые разбивают Base64: PEM (64) и MIME (76).
const LINE_WIDTHS: [usize; 2] = [64, 76];

/// Минимальная длина отдельной строки, которая маскируется сама по себе (последняя строка
/// бывает короткой).
const MIN_LINE_LEN: usize = 8;

/// Размер буфера чтения вывода дочерней команды.
const READ_BUF: usize = 8 * 1024;

/// Текст и его base64 (стандартный и URL-safe алфавиты без `=`, так base64 находится и
/// внутри более длинной строки, и с паддингом); то же без завершающих пробельных символов и
/// с одним переводом строки в конце (так текст лежит в файле).
fn with_base64(needles: &mut Vec<Vec<u8>>, value: &str) {
    let value = value.as_bytes();
    let trimmed = value.trim_ascii_end();
    let mut line = trimmed.to_vec();
    line.push(b'\n');
    for form in [value, trimmed, line.as_slice()] {
        needles.push(form.to_vec());
        needles.push(STANDARD_NO_PAD.encode(form).into_bytes());
        needles.push(URL_SAFE_NO_PAD.encode(form).into_bytes());
    }
}

/// Строки, которые нужно маскировать для ключа и текста его файла (см. описание модуля).
pub fn needles_for_key(key: &ApiKey, file_text: &str) -> Vec<Vec<u8>> {
    let mut needles = Vec::new();
    with_base64(&mut needles, file_text);
    let private = key.expose_private_key_base64();
    with_base64(&mut needles, private);
    for width in LINE_WIDTHS {
        for line in private.as_bytes().chunks(width) {
            if line.len() >= MIN_LINE_LEN {
                needles.push(line.to_vec());
            }
        }
    }
    needles.push(key.key_id().as_str().as_bytes().to_vec());
    needles.retain(|needle| needle.len() >= MIN_MASK_LEN);
    needles.sort_unstable();
    needles.dedup();
    needles
}

/// Символ токена JWT/JWE.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// Потоковый маскировщик. Не `Clone`/`Debug`: внутри части ключа.
pub struct Masker {
    needles: Vec<Vec<u8>>,
    /// Байты, с которых начинается хотя бы один образец или префикс токена (быстрый пропуск
    /// остальных).
    starts: [bool; 256],
    max_len: usize,
    /// Нерешённый хвост предыдущего куска.
    carry: Vec<u8>,
    /// До какой позиции (относительно начала `carry`) байты уже скрыты открытой областью.
    masked_until: usize,
    /// Область маски открыта: [`MASK`] для неё уже выведен.
    region_open: bool,
    /// Идёт токен: следующие символы токена тоже скрываются.
    in_token: bool,
}

impl Masker {
    pub fn new(needles: impl IntoIterator<Item = Vec<u8>>) -> Self {
        let needles: Vec<Vec<u8>> = needles.into_iter().filter(|n| !n.is_empty()).collect();
        let mut starts = [false; 256];
        for needle in needles.iter().map(Vec::as_slice).chain(TOKEN_PREFIXES) {
            if let Some(&first) = needle.first() {
                starts[usize::from(first)] = true;
            }
        }
        let max_len = needles
            .iter()
            .map(Vec::len)
            .chain(TOKEN_PREFIXES.iter().map(|p| p.len()))
            .max()
            .unwrap_or(0);
        Self {
            needles,
            starts,
            max_len,
            carry: Vec::new(),
            masked_until: 0,
            region_open: false,
            in_token: false,
        }
    }

    /// Обрабатывает очередной кусок; возвращает то, что уже можно вывести.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.process(chunk, false)
    }

    /// Конец потока: выводит остаток.
    pub fn finish(&mut self) -> Vec<u8> {
        self.process(&[], true)
    }

    fn open_region(&mut self, out: &mut Vec<u8>, until: usize) {
        if !self.region_open {
            out.extend_from_slice(MASK);
            self.region_open = true;
        }
        self.masked_until = self.masked_until.max(until);
    }

    fn process(&mut self, chunk: &[u8], eof: bool) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let limit = if eof {
            buf.len()
        } else {
            self.decided_len(&buf)
        };

        let mut out = Vec::with_capacity(limit + MASK.len());
        for pos in 0..limit {
            let byte = buf[pos];
            if self.starts[usize::from(byte)] {
                let rest = &buf[pos..];
                let mut until = None;
                for needle in &self.needles {
                    if rest.starts_with(needle) {
                        until = until.max(Some(pos + needle.len()));
                    }
                }
                for prefix in TOKEN_PREFIXES {
                    if rest.starts_with(prefix) {
                        until = until.max(Some(pos + prefix.len()));
                        self.in_token = true;
                    }
                }
                if let Some(until) = until {
                    self.open_region(&mut out, until);
                }
            }
            if pos < self.masked_until {
                continue;
            }
            if self.in_token && is_token_byte(byte) {
                // Область маски остаётся открытой: токен скрывается одним `***`.
                continue;
            }
            self.in_token = false;
            self.region_open = false;
            out.push(byte);
        }

        if eof {
            self.masked_until = 0;
            self.region_open = false;
            self.in_token = false;
        } else {
            self.masked_until = self.masked_until.saturating_sub(limit);
            self.carry = buf.split_off(limit);
        }
        out
    }

    /// Длина решённой части буфера: первая позиция, с которой хвост буфера — собственный
    /// префикс какого-то образца или префикса токена (дальше без следующего куска решить
    /// нельзя).
    fn decided_len(&self, buf: &[u8]) -> usize {
        let from = buf.len().saturating_sub(self.max_len.saturating_sub(1));
        (from..buf.len())
            .find(|&pos| {
                let tail = &buf[pos..];
                self.needles
                    .iter()
                    .map(Vec::as_slice)
                    .chain(TOKEN_PREFIXES)
                    .any(|needle| needle.len() > tail.len() && needle.starts_with(tail))
            })
            .unwrap_or(buf.len())
    }
}

/// Копирует поток `reader` в `writer`, маскируя значения. Ошибка записи (например, закрытый
/// канал у `| head`) прекращает копирование: `reader` закрывается, как в обычном конвейере.
pub async fn pump<R, W>(mut reader: R, mut writer: W, mut masker: Masker) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; READ_BUF];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let out = masker.push(&buf[..read]);
        if !out.is_empty() {
            writer.write_all(&out).await?;
            writer.flush().await?;
        }
    }
    let out = masker.finish();
    writer.write_all(&out).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use rustore_api::testing;

    use super::*;
    use crate::cli::key::key_file_text;

    fn masker(values: &[&str]) -> Masker {
        Masker::new(values.iter().map(|v| v.as_bytes().to_vec()))
    }

    /// Прогоняет текст кусками заданного размера.
    fn run(masker: &mut Masker, text: &str, chunk: usize) -> String {
        let mut out = Vec::new();
        for part in text.as_bytes().chunks(chunk) {
            out.extend(masker.push(part));
        }
        out.extend(masker.finish());
        String::from_utf8(out).unwrap()
    }

    /// Результат должен совпадать при любом разбиении на куски.
    fn all_chunkings(make: impl Fn() -> Masker, text: &str) -> String {
        let whole = run(&mut make(), text, text.len().max(1));
        for size in 1..=text.len().max(1) {
            assert_eq!(run(&mut make(), text, size), whole, "chunk size {size}");
        }
        whole
    }

    #[test]
    fn masks_single_value() {
        assert_eq!(
            all_chunkings(
                || masker(&["s3cret-value"]),
                "token=s3cret-value; again s3cret-value\n"
            ),
            "token=***; again ***\n"
        );
    }

    #[test]
    fn masks_value_split_across_chunks() {
        let mut m = masker(&["s3cret-value"]);
        let mut out = m.push(b"before s3cr");
        // Начало значения задержано, остальное выведено сразу.
        assert_eq!(out, b"before ");
        out.extend(m.push(b"et-val"));
        out.extend(m.push(b"ue after"));
        out.extend(m.finish());
        assert_eq!(String::from_utf8(out).unwrap(), "before *** after");
    }

    #[test]
    fn does_not_hold_unrelated_tail() {
        let mut m = masker(&["s3cret-value"]);
        assert_eq!(m.push(b"Password: "), b"Password: ");
        assert_eq!(m.push(b"xyz"), b"xyz");
        // Хвост, похожий на начало значения, ждёт; на конце потока выводится как есть.
        assert_eq!(m.push(b" s3c"), b" ");
        assert_eq!(m.finish(), b"s3c");
    }

    #[test]
    fn masks_overlapping_values_as_one_region() {
        assert_eq!(
            all_chunkings(|| masker(&["abcdefgh", "efghijkl"]), "x abcdefghijkl y"),
            "x *** y"
        );
        assert_eq!(
            all_chunkings(
                || masker(&["pass", "password-long"]),
                "1 password-long 2 pass 3 passw"
            ),
            "1 *** 2 *** 3 ***w"
        );
        assert_eq!(all_chunkings(|| masker(&["abcd"]), "abcdabcd!"), "***!");
    }

    #[test]
    fn masks_access_tokens() {
        let token = ["eyJhbGciOiJSU0EtT0FFUCJ9", "a0Af-B_y", "Zq9"].join(".");
        assert_eq!(
            all_chunkings(
                || Masker::new(Vec::new()),
                &format!("Public-Token: {token}\n{{\"jwe\":\"{token}\"}}\n")
            ),
            "Public-Token: ***\n{\"jwe\":\"***\"}\n"
        );
        // Похожее начало без `J` — не токен.
        assert_eq!(
            all_chunkings(|| Masker::new(Vec::new()), "eye is fine; ey"),
            "eye is fine; ey"
        );
    }

    #[test]
    fn masks_every_form_of_the_key() {
        let key = testing::api_key();
        let file = key_file_text(&key).to_string();
        let private = key.expose_private_key_base64().to_owned();
        let masker = || Masker::new(needles_for_key(&key, &file));

        // `cat` файла ключа.
        assert_eq!(run(&mut masker(), &file, 7), "***");
        // Строка файла: `grep client_secret`.
        assert_eq!(
            run(
                &mut masker(),
                &format!("  \"client_secret\": \"{private}\"\n"),
                5
            ),
            "  \"client_secret\": \"***\"\n"
        );
        // keyId.
        assert_eq!(
            run(&mut masker(), &format!("id={}\n", testing::TEST_KEY_ID), 3),
            "id=***\n"
        );
        // Ключ в PEM (строки по 64 символа) и в base64 с переносами по 76.
        let pem_body: Vec<&str> = private
            .as_bytes()
            .chunks(64)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect();
        let pem = format!(
            "{}{}\n{}\n{}{}\n",
            "-----BEGIN PRIVATE",
            " KEY-----",
            pem_body.join("\n"),
            "-----END PRIVATE",
            " KEY-----"
        );
        let out = run(&mut masker(), &pem, 13);
        for line in &pem_body {
            if line.len() >= MIN_LINE_LEN {
                assert!(!out.contains(line), "{out}");
            }
        }
        // Заголовки PEM остаются. Маркер собирается из частей: gitleaks экспорта не должен
        // принимать тест за ключ.
        assert!(
            out.starts_with(&format!("{}{}\n", "-----BEGIN PRIVATE", " KEY-----")),
            "{out}"
        );
        let mime: Vec<&str> = private
            .as_bytes()
            .chunks(76)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect();
        let out = run(&mut masker(), &mime.join("\r\n"), 50);
        assert!(!out.contains(mime[1]), "{out}");
        // base64 всего файла (`base64 < rustore-credentials.json`), с паддингом.
        let encoded = base64::engine::general_purpose::STANDARD.encode(&file);
        let out = run(&mut masker(), &encoded, 11);
        assert!(!out.contains(encoded.trim_end_matches('=')), "{out}");
        assert!(out.starts_with("***"), "{out}");
        // Поля без секретов остаются.
        assert_eq!(run(&mut masker(), "\"key_id\": \n", 4), "\"key_id\": \n");
    }

    #[test]
    fn passes_through_without_values() {
        let mut m = Masker::new(Vec::new());
        assert_eq!(m.push(b"plain"), b"plain");
        assert_eq!(m.finish(), b"");
    }

    #[test]
    fn handles_binary_output() {
        let value = [0xffu8, 0x00, 0x10, 0x80, 0x7f];
        let mut m = Masker::new([value.to_vec()]);
        let mut input = vec![1u8, 2];
        input.extend_from_slice(&value);
        input.push(3);
        let mut out = Vec::new();
        for byte in &input {
            out.extend(m.push(std::slice::from_ref(byte)));
        }
        out.extend(m.finish());
        assert_eq!(out, [1, 2, b'*', b'*', b'*', 3]);
    }

    #[tokio::test]
    async fn pump_masks_stream() {
        let input: &[u8] = b"line one s3cret-value\nline two\n";
        let mut output = Vec::new();
        pump(input, &mut output, masker(&["s3cret-value"]))
            .await
            .unwrap();
        assert_eq!(output, b"line one ***\nline two\n");
    }
}
