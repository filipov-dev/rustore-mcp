//! Уровни доступа всех операций: полная таблица «операция → уровень».
//!
//! Уровень удаления, кроме DELETE, получают действия с последствиями для пользователей
//! RuStore и денег: отправка на модерацию, публикация, раскатка, архивирование, ответы на
//! отзывы, возвраты, отмены и подтверждения покупок и подписок, выдача и отзыв доступов.
//! Изменения черновика версии — уровень записи, всё остальное — чтение. Новая операция
//! спецификации без строки в таблице — ошибка теста: её уровень нужно решить явно.

use std::collections::BTreeMap;

use rustore_api::spec::{AccessLevel, HttpMethod, find, operations};

use AccessLevel::{Destructive, Read, Write};

/// Ожидаемый уровень каждой операции спецификации.
const EXPECTED: &[(&str, AccessLevel)] = &[
    // Приложения.
    ("list_apps", Read),
    // Версии и публикация: черновик — запись, выход к пользователям — удаление.
    ("list_versions", Read),
    ("get_version", Read),
    ("create_draft_version", Write),
    ("update_draft_version", Write),
    ("delete_draft_version", Write),
    ("upload_apk", Write),
    ("upload_aab", Write),
    // При автопубликации одобренная версия сразу уходит пользователям.
    ("submit_draft_for_moderation", Destructive),
    ("publish_version", Destructive),
    // Тем же вызовом меняется процент раскатки опубликованной версии.
    ("update_publish_settings", Destructive),
    ("archive_version", Destructive),
    // Иконка и скриншоты черновика.
    ("list_screenshots", Read),
    ("upload_icon", Write),
    ("upload_screenshot", Write),
    ("upload_screenshot_v2", Write),
    ("delete_screenshot", Write),
    // Отзывы: ответы публичны.
    ("list_reviews", Read),
    ("export_reviews_csv", Read),
    ("get_app_rating", Read),
    ("get_review_answer_status", Read),
    ("get_review_answer_statuses", Read),
    ("reply_to_review", Destructive),
    ("update_review_answer", Destructive),
    ("delete_review_answer", Destructive),
    // Платежи и покупки: деньги покупателей.
    ("list_payments_by_date", Read),
    ("get_payment", Read),
    ("get_payment_sandbox", Read),
    ("list_user_purchases", Read),
    ("list_user_purchases_sandbox", Read),
    ("refund_payment", Destructive),
    ("refund_payment_sandbox", Destructive),
    ("cancel_purchase", Destructive),
    ("cancel_purchase_sandbox", Destructive),
    ("confirm_purchase", Destructive),
    ("confirm_purchase_sandbox", Destructive),
    // Подписки.
    ("get_subscription", Read),
    ("get_subscription_sandbox", Read),
    ("get_subscription_v5", Read),
    ("get_subscription_v5_sandbox", Read),
    ("acknowledge_subscription", Destructive),
    ("acknowledge_subscription_sandbox", Destructive),
    ("cancel_subscription", Destructive),
    ("cancel_subscription_sandbox", Destructive),
    // Продуктовый каталог.
    ("list_catalog_products", Read),
    ("list_catalog_subscriptions", Read),
    // Доступы пользователей.
    ("list_app_access", Read),
    ("grant_app_access", Destructive),
    ("revoke_app_access", Destructive),
];

#[test]
fn every_operation_has_the_expected_level() {
    let expected: BTreeMap<&str, AccessLevel> = EXPECTED.iter().copied().collect();
    assert_eq!(expected.len(), EXPECTED.len(), "повтор в таблице");
    for op in operations() {
        let Some(level) = expected.get(op.tool_name) else {
            panic!(
                "{}: операции нет в таблице уровней доступа; решите её уровень и добавьте \
                 строку в EXPECTED",
                op.tool_name
            );
        };
        assert_eq!(op.access, *level, "{}", op.tool_name);
    }
    for name in expected.keys() {
        assert!(find(name).is_some(), "{name}: нет такой операции");
    }
    assert_eq!(operations().len(), EXPECTED.len());
}

#[test]
fn money_users_and_access_changes_are_destructive() {
    // Любое изменение в наборах отзывов, платежей, подписок и доступов доходит до
    // пользователей или денег; чтение остаётся чтением.
    for op in operations() {
        if !matches!(
            op.toolset,
            "reviews" | "purchases" | "subscriptions" | "access"
        ) {
            continue;
        }
        let expected = if op.method == HttpMethod::Get {
            Read
        } else {
            Destructive
        };
        assert_eq!(op.access, expected, "{} ({})", op.tool_name, op.toolset);
    }
    // Каталог только читается.
    for op in operations().iter().filter(|op| op.toolset == "catalog") {
        assert_eq!(op.access, Read, "{}", op.tool_name);
    }
    // Изменения иконки и скриншотов — только черновик.
    for op in operations().iter().filter(|op| op.toolset == "listing") {
        let expected = if op.method == HttpMethod::Get {
            Read
        } else {
            Write
        };
        assert_eq!(op.access, expected, "{}", op.tool_name);
    }
}

#[test]
fn sandbox_variants_match_their_main_operations() {
    let mut sandbox = 0;
    for op in operations() {
        let Some(base) = op.tool_name.strip_suffix("_sandbox") else {
            continue;
        };
        sandbox += 1;
        let base = find(base).unwrap_or_else(|| panic!("{}: нет {base}", op.tool_name));
        assert_eq!(op.access, base.access, "{}", op.tool_name);
        assert_eq!(op.method, base.method, "{}", op.tool_name);
        assert_eq!(op.toolset, base.toolset, "{}", op.tool_name);
    }
    assert!(sandbox >= 8, "вариантов песочницы: {sandbox}");
}

#[test]
fn reads_are_gets_and_deletes_are_not_reads() {
    for op in operations() {
        match op.method {
            HttpMethod::Get => assert_eq!(op.access, Read, "{}", op.tool_name),
            _ => assert_ne!(op.access, Read, "{}", op.tool_name),
        }
    }
}
