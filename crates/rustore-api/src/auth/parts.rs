//! Хранение ключа в нескольких записях: для Windows Credential Manager.
//!
//! У записи Credential Manager предел 2560 байт (`CRED_MAX_CREDENTIAL_BLOB_SIZE`), а keyring
//! пишет пароль в UTF-16 — вдвое больше байт, чем символов. Запись ключа RuStore API (JSON с
//! `keyId` и приватным ключом RSA 2048 в Base64) — около 1700 символов, RSA 4096 — около
//! 3200: паролем в одну запись не помещается ни та, ни другая. Поэтому на Windows ключ
//! пишется сырыми байтами UTF-8 и, если нужно, делится на части:
//!
//! - ключ не длиннее [`PART_SIZE`] — одна запись `<профиль>` с JSON как есть (начинается с `{`);
//! - длиннее — первая часть в записи `<профиль>` с заголовком `parts:<N>\n`, остальные — в
//!   записях `<профиль>/part-2` … `<профиль>/part-<N>`. Символ `/` в имени профиля
//!   запрещён, поэтому записи частей не совпадут с записью другого профиля.
//!
//! Части пишутся до записи `<профиль>`, а лишние части прежнего ключа удаляются после неё.
//! Значения частей наружу не попадают: ошибки называют только профиль.

use zeroize::Zeroizing;

use super::AuthError;
#[cfg(test)]
use crate::key::ApiKey;

/// Байт данных ключа в одной записи. С заголовком `parts:NN\n` запись остаётся меньше предела
/// Credential Manager (2560 байт).
pub(super) const PART_SIZE: usize = 2048;
/// Наибольшее число частей: 16 КиБ, как предел файла ключа (`MAX_KEY_FILE_BYTES`).
pub(super) const MAX_PARTS: usize = 8;
const HEADER_PREFIX: &[u8] = b"parts:";

/// Записи системного хранилища с сырыми байтами.
pub(super) trait RawEntries {
    /// Значение записи; `None` — записи нет.
    fn read(&self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>, AuthError>;
    fn write(&self, account: &str, value: &[u8]) -> Result<(), AuthError>;
    /// Удаляет запись; `false` — записи не было.
    fn remove(&self, account: &str) -> Result<bool, AuthError>;
}

/// Имя записи части `index` (с 2) профиля.
pub(super) fn part_account(profile: &str, index: usize) -> String {
    format!("{profile}/part-{index}")
}

fn corrupted(profile: &str) -> AuthError {
    AuthError::Corrupted {
        profile: profile.to_owned(),
    }
}

/// Число частей из заголовка первой записи и данные после него; `None` — заголовка нет.
fn split_header(value: &[u8]) -> Option<Result<(usize, &[u8]), ()>> {
    let rest = value.strip_prefix(HEADER_PREFIX)?;
    let parsed = (|| {
        let end = rest.iter().position(|b| *b == b'\n')?;
        let count: usize = std::str::from_utf8(&rest[..end]).ok()?.parse().ok()?;
        (2..=MAX_PARTS)
            .contains(&count)
            .then_some((count, &rest[end + 1..]))
    })();
    Some(parsed.ok_or(()))
}

/// Читает текст записи профиля из одной или нескольких записей.
pub(super) fn read_text(
    entries: &dyn RawEntries,
    profile: &str,
) -> Result<Zeroizing<String>, AuthError> {
    let Some(head) = entries.read(profile)? else {
        return Err(AuthError::NotFound {
            profile: profile.to_owned(),
        });
    };
    let json = match split_header(&head) {
        None => head,
        Some(Err(())) => return Err(corrupted(profile)),
        Some(Ok((count, first))) => {
            let mut json = Zeroizing::new(Vec::with_capacity(count * PART_SIZE));
            json.extend_from_slice(first);
            for index in 2..=count {
                let part = entries
                    .read(&part_account(profile, index))?
                    .ok_or_else(|| corrupted(profile))?;
                json.extend_from_slice(&part);
            }
            json
        }
    };
    let text = std::str::from_utf8(&json).map_err(|_| corrupted(profile))?;
    Ok(Zeroizing::new(text.to_owned()))
}

/// Читает ключ профиля из одной или нескольких записей.
#[cfg(test)]
pub(super) fn read_key(entries: &dyn RawEntries, profile: &str) -> Result<ApiKey, AuthError> {
    let text = read_text(entries, profile)?;
    ApiKey::from_stored(&text).map_err(|_| corrupted(profile))
}

/// Записывает текст записи профиля, заменяя прежний.
pub(super) fn write_text(
    entries: &dyn RawEntries,
    profile: &str,
    json: &str,
) -> Result<(), AuthError> {
    write_parts(entries, profile, json.as_bytes(), PART_SIZE)
}

/// Записывает ключ профиля, заменяя прежний.
#[cfg(test)]
pub(super) fn write_key(
    entries: &dyn RawEntries,
    profile: &str,
    key: &ApiKey,
) -> Result<(), AuthError> {
    write_text(entries, profile, &key.to_stored())
}

/// [`write_key`] с явным размером части: так оба вида записи проверяются в тестах.
fn write_parts(
    entries: &dyn RawEntries,
    profile: &str,
    json: &[u8],
    part_size: usize,
) -> Result<(), AuthError> {
    let chunks: Vec<&[u8]> = json.chunks(part_size).collect();
    if chunks.len() > MAX_PARTS {
        return Err(AuthError::Backend(format!(
            "the credentials are too large for the system keychain ({} bytes, at most {} \
             bytes)",
            json.len(),
            part_size * MAX_PARTS
        )));
    }
    if chunks.len() <= 1 {
        entries.write(profile, json)?;
    } else {
        // Сначала части 2..N: запись профиля, которая на них ссылается, — последней.
        for (offset, chunk) in chunks.iter().enumerate().skip(1) {
            entries.write(&part_account(profile, offset + 1), chunk)?;
        }
        let mut head = Zeroizing::new(format!("parts:{}\n", chunks.len()).into_bytes());
        head.extend_from_slice(chunks[0]);
        entries.write(profile, &head)?;
    }
    remove_parts_from(entries, profile, chunks.len().max(1) + 1)
}

/// Удаляет ключ профиля и все его части.
pub(super) fn delete_key(entries: &dyn RawEntries, profile: &str) -> Result<(), AuthError> {
    let existed = entries.remove(profile)?;
    // Части удаляются и без записи профиля: от прерванной записи могли остаться только они.
    remove_parts_from(entries, profile, 2)?;
    if existed {
        Ok(())
    } else {
        Err(AuthError::NotFound {
            profile: profile.to_owned(),
        })
    }
}

/// Удаляет части с номерами от `first` до [`MAX_PARTS`].
fn remove_parts_from(
    entries: &dyn RawEntries,
    profile: &str,
    first: usize,
) -> Result<(), AuthError> {
    for index in first..=MAX_PARTS {
        entries.remove(&part_account(profile, index))?;
    }
    Ok(())
}

/// Записи Credential Manager через keyring: сырые байты (`set_secret`), без UTF-16.
#[cfg(windows)]
pub(super) struct Credentials;

#[cfg(windows)]
impl RawEntries for Credentials {
    fn read(&self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>, AuthError> {
        match super::KeyringStore::entry(account)?.get_secret() {
            Ok(value) => Ok(Some(Zeroizing::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(super::map_error(account, err)),
        }
    }

    fn write(&self, account: &str, value: &[u8]) -> Result<(), AuthError> {
        super::KeyringStore::entry(account)?
            .set_secret(value)
            .map_err(|err| super::map_error(account, err))
    }

    fn remove(&self, account: &str) -> Result<bool, AuthError> {
        match super::KeyringStore::entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(err) => Err(super::map_error(account, err)),
        }
    }
}

#[cfg(test)]
pub(super) mod testing {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    /// Записи в памяти: проверка деления на любой ОС без Credential Manager.
    #[derive(Debug, Default)]
    pub struct MemoryEntries {
        pub entries: Mutex<BTreeMap<String, Vec<u8>>>,
        /// Наибольший размер записи, как у Credential Manager.
        pub limit: usize,
    }

    impl MemoryEntries {
        pub fn with_limit(limit: usize) -> Self {
            Self {
                limit,
                ..Default::default()
            }
        }

        pub fn accounts(&self) -> Vec<String> {
            self.entries.lock().unwrap().keys().cloned().collect()
        }
    }

    impl RawEntries for MemoryEntries {
        fn read(&self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>, AuthError> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .get(account)
                .cloned()
                .map(Zeroizing::new))
        }

        fn write(&self, account: &str, value: &[u8]) -> Result<(), AuthError> {
            if value.len() > self.limit {
                return Err(AuthError::Backend(format!(
                    "{account}: {} bytes is over the limit",
                    value.len()
                )));
            }
            self.entries
                .lock()
                .unwrap()
                .insert(account.to_owned(), value.to_vec());
            Ok(())
        }

        fn remove(&self, account: &str) -> Result<bool, AuthError> {
            Ok(self.entries.lock().unwrap().remove(account).is_some())
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use rsa::pkcs8::EncodePrivateKey;

    use super::testing::MemoryEntries;
    use super::*;
    use crate::key::testing::{TEST_KEY_ID, private_key_base64};

    /// Предел записи Credential Manager.
    const CRED_LIMIT: usize = 2560;

    fn key(id: &str) -> ApiKey {
        ApiKey::new(id, &private_key_base64()).unwrap()
    }

    /// Ключ RSA 4096: его запись длиннее [`PART_SIZE`]. Генерация медленная, ключ один.
    fn large_key(id: &str) -> ApiKey {
        static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let base64 = KEY.get_or_init(|| {
            let key = rsa::PrivateKey::new(&mut getrandom::SysRng, 4096).unwrap();
            STANDARD.encode(key.to_pkcs8_der().unwrap().as_bytes())
        });
        ApiKey::new(id, base64).unwrap()
    }

    #[test]
    fn a_real_key_does_not_fit_one_utf16_entry() {
        let stored = key(TEST_KEY_ID).to_stored();
        let utf16 = stored.encode_utf16().count() * 2;
        assert!(utf16 > CRED_LIMIT, "{utf16}");
        assert!(stored.len() <= PART_SIZE, "{}", stored.len());
        assert!(large_key(TEST_KEY_ID).to_stored().len() > PART_SIZE);
    }

    #[test]
    fn small_key_is_one_plain_entry() {
        let entries = MemoryEntries::with_limit(CRED_LIMIT);
        let key = key(TEST_KEY_ID);
        write_key(&entries, "work", &key).unwrap();
        assert_eq!(entries.accounts(), ["work"]);
        let head = entries.read("work").unwrap().unwrap();
        assert_eq!(head.as_slice(), key.to_stored().as_bytes());
        assert_eq!(read_key(&entries, "work").unwrap(), key);

        // Ключ, записанный частями, заменяется одной записью: лишних частей не остаётся.
        write_key(&entries, "work", &large_key(TEST_KEY_ID)).unwrap();
        assert!(entries.accounts().len() > 1);
        write_key(&entries, "work", &key).unwrap();
        assert_eq!(entries.accounts(), ["work"]);
    }

    #[test]
    fn large_key_is_split_and_read_back() {
        let entries = MemoryEntries::with_limit(CRED_LIMIT);
        let key = large_key("key-one");
        let len = key.to_stored().len();
        write_key(&entries, "work", &key).unwrap();
        let count = len.div_ceil(PART_SIZE);
        assert!(count >= 2, "{len}");
        let mut expected = vec!["work".to_owned()];
        expected.extend((2..=count).map(|i| part_account("work", i)));
        expected.sort();
        assert_eq!(entries.accounts(), expected);
        let head = entries.read("work").unwrap().unwrap();
        assert!(head.starts_with(format!("parts:{count}\n{{").as_bytes()));
        assert_eq!(read_key(&entries, "work").unwrap(), key);

        // Другой профиль не задет.
        let other = large_key("key-two");
        write_key(&entries, "work.2", &other).unwrap();
        assert_eq!(read_key(&entries, "work").unwrap(), key);
        assert_eq!(read_key(&entries, "work.2").unwrap(), other);
    }

    #[test]
    fn replacing_with_a_shorter_value_removes_stale_parts() {
        let entries = MemoryEntries::with_limit(CRED_LIMIT);
        let long = vec![b'x'; PART_SIZE * 2 + 1];
        write_parts(&entries, "work", &long, PART_SIZE).unwrap();
        assert!(entries.accounts().contains(&part_account("work", 3)));
        let short = key(TEST_KEY_ID);
        write_key(&entries, "work", &short).unwrap();
        assert!(!entries.accounts().contains(&part_account("work", 3)));
        assert_eq!(read_key(&entries, "work").unwrap(), short);
    }

    #[test]
    fn delete_removes_every_part() {
        let entries = MemoryEntries::with_limit(CRED_LIMIT);
        write_parts(&entries, "work", &[b'x'; PART_SIZE * 2], PART_SIZE).unwrap();
        write_key(&entries, "other", &key(TEST_KEY_ID)).unwrap();
        delete_key(&entries, "work").unwrap();
        assert!(entries.accounts().iter().all(|a| a.starts_with("other")));
        assert!(matches!(
            read_key(&entries, "work"),
            Err(AuthError::NotFound { .. })
        ));
        assert!(matches!(
            delete_key(&entries, "work"),
            Err(AuthError::NotFound { .. })
        ));

        // Части без записи профиля (прерванная запись) тоже удаляются.
        entries.write(&part_account("lost", 2), b"tail").unwrap();
        assert!(matches!(
            delete_key(&entries, "lost"),
            Err(AuthError::NotFound { .. })
        ));
        assert!(entries.accounts().iter().all(|a| a.starts_with("other")));
    }

    #[test]
    fn broken_entries_are_corrupted_without_values() {
        let entries = MemoryEntries::with_limit(CRED_LIMIT);
        write_key(&entries, "work", &large_key(TEST_KEY_ID)).unwrap();
        entries.remove(&part_account("work", 2)).unwrap();
        let err = read_key(&entries, "work").unwrap_err();
        assert!(matches!(err, AuthError::Corrupted { .. }), "{err:?}");

        for head in [
            &b"parts:1\n{"[..],
            b"parts:99\n{",
            b"parts:x\n{",
            b"parts:2",
            b"not a key",
            b"\xff\xfe",
        ] {
            entries.write("bad", head).unwrap();
            let err = read_key(&entries, "bad").unwrap_err();
            assert!(matches!(err, AuthError::Corrupted { .. }), "{err:?}");
            assert!(!err.to_string().contains("parts:"), "{err}");
        }
    }

    #[test]
    fn too_large_value_is_rejected() {
        let entries = MemoryEntries::with_limit(CRED_LIMIT);
        let huge = vec![b'x'; PART_SIZE * MAX_PARTS + 1];
        let err = write_parts(&entries, "work", &huge, PART_SIZE).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
        assert!(entries.accounts().is_empty());
    }
}
