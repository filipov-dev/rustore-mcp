//! Наборы инструментов: slug для каждого тега спецификации.
//!
//! Теги в спецификации — русские названия разделов справочника, а флаг `--toolsets`
//! принимает короткие английские slug. Таблица задаётся явно: новый раздел справочника
//! должен получить slug осознанно, поэтому неизвестный тег — ошибка сборки.

/// Тег спецификации → slug набора инструментов.
pub const TOOLSET_SLUGS: &[(&str, &str)] = &[
    ("Приложения", "apps"),
    ("Версии и публикация", "releases"),
    ("Иконка и скриншоты", "listing"),
    ("Отзывы", "reviews"),
    ("Платежи и покупки", "purchases"),
    ("Подписки", "subscriptions"),
    ("Продуктовый каталог", "catalog"),
    ("Доступы", "access"),
];

/// Slug набора для тега; `None`, если тега нет в [`TOOLSET_SLUGS`].
pub fn slug_for(tag: &str) -> Option<&'static str> {
    TOOLSET_SLUGS
        .iter()
        .find(|(name, _)| *name == tag)
        .map(|(_, slug)| *slug)
}

/// Slug: строчные латинские буквы, цифры и `_`, начинается с буквы.
pub fn is_valid_slug(slug: &str) -> bool {
    slug.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
