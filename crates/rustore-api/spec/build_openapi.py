#!/usr/bin/env python3
"""Собирает OpenAPI 3.1 для RuStore Public API из справочника RuStore API.

Официальной спецификации у RuStore API нет, поэтому источник — HTML-страницы
справочника https://www.rustore.ru/help/work-with-rustore-api (сайт на Docusaurus).
Список страниц не задан заранее: он собирается из боковой панели и ссылок на
корневой странице справочника и на страницах разделов.

Использование:
    python3 crates/rustore-api/spec/build_openapi.py [--cache <каталог>] [--offline] [--check] [--out crates/rustore-api/spec/rustore.openapi.json]

Без --offline страницы скачиваются в каталог кеша (без --cache — во временный
каталог) по одной, с паузой между запросами: сайт ограничивает частоту запросов
и вместо страницы отдаёт заглушку «429». С --offline используется уже скачанное.
Отчёт о том, что пропущено или не удалось разобрать, печатается в stderr.

С --check файл --out не перезаписывается: свежая сборка сравнивается с ним и
печатается разница (страницы, операции, параметры). Код выхода: 0 — изменений
нет, 3 — спецификация отстала от справочника, 1 — справочник недоступен или не
разобран, 2 — неверные аргументы.

Расхождения справочника с реальным API и то, что из HTML надёжно не извлекается,
исправляются исключениями в разделе «исключения» ниже (OPERATION_NAMES,
ACCESS_RULES, PARAM_OVERRIDES и соседние таблицы), а не правкой JSON.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from html.parser import HTMLParser

DOCS_HOST = "https://www.rustore.ru"
DOCS_ROOT = "/help/work-with-rustore-api"
API_SERVER = "https://public-api-m.rustore.ru"
# Адрес метода: полный или только путь (на части страниц хост не указан).
API_URL_RE = re.compile(r"(?:https://public-api-m\.rustore\.ru)?(/public/[^\s`'\"?<>]*)(\?[^\s`'\"<>]*)?")
METHOD_RE = re.compile(r"\b(GET|POST|PUT|PATCH|DELETE)\b")

# Пауза между запросами и ожидание после заглушки «429». Сайт за защитой VK: примерно
# после двух десятков запросов за несколько минут он перенаправляет на /challenge.html
# (страница «У вас большие запросы!») примерно на десять минут. Обходить защиту нельзя и
# не нужно: скрипт ждёт и повторяет запрос, поэтому полная сборка занимает до получаса.
FETCH_DELAY = 1.0
RATE_LIMIT_WAIT = 600.0
RATE_LIMIT_RETRIES = 3


# ---------- разделы и наборы инструментов ----------

# Теги спецификации: (ключ, название, описание). Один тег — один набор инструментов
# (slug задаёт build/toolsets.rs). Порядок — порядок наборов в list_toolsets.
TAGS = [
    ("apps", "Приложения", "Список приложений, доступных ключу RuStore API."),
    (
        "releases",
        "Версии и публикация",
        "Черновики и версии приложения, загрузка APK и AAB, статусы, настройки публикации, "
        "модерация, публикация и архивирование.",
    ),
    ("listing", "Иконка и скриншоты", "Иконка приложения и скриншоты черновика версии."),
    ("reviews", "Отзывы", "Отзывы пользователей, ответы разработчика и рейтинг приложения."),
    ("purchases", "Платежи и покупки", "Платежи, покупки и возвраты средств."),
    ("subscriptions", "Подписки", "Данные подписок, подтверждение и отмена подписок."),
    ("catalog", "Продуктовый каталог", "Продукты и подписки из продуктового каталога приложения."),
    ("access", "Доступы", "Доступы пользователей к приложению."),
]
TAG_NAME = {key: name for key, name, _ in TAGS}

# Раздел справочника (первый сегмент страницы после DOCS_ROOT) -> правила выбора тега.
# Правило — (регулярное выражение по пути API, тег); первое совпадение выигрывает,
# последний элемент — тег раздела по умолчанию. Разделы со своим набором — без правил.
SECTION_TAGS = {
    "api-upload-publication-app": [
        (r"^/public/v1/application$", "apps"),
        (r"/(image|screenshot)", "listing"),
        ("", "releases"),
    ],
    "api-subscription-payment": [
        (r"subscription", "subscriptions"),
        ("", "purchases"),
    ],
    "api-product-catalog": [("", "catalog")],
    "api-feedback-process": [("", "reviews")],
    "api-access-control": [("", "access")],
}

# Страницы без методов API: обзор, процессы, авторизация. Их ссылки обходятся, но
# операций на них не ищут.
OVERVIEW_PAGES = {
    "",
    "api-authorization-process",
    "api-authorization-token",
    "api-feedback-process/feedback-process",
}

# Операции, которые не становятся инструментами, с причиной (ключ — метод и путь без
# завершающего «/»).
EXCLUDED_OPERATIONS = {
    # Токен доступа выпускает исполнитель по ключу профиля; агенту он не нужен и не
    # должен попадать в ответы.
    ("POST", "/public/auth"): "выпуск токена доступа: его вызывает исполнитель",
}


# ---------- исключения: имена операций ----------

# Метод и путь -> operationId (он же имя инструмента, не длиннее 40 символов).
# Справочник не даёт машинных имён, поэтому имена заданы явно. Новая операция без имени
# получает имя из адреса страницы и попадает в отчёт.
OPERATION_NAMES: dict[tuple[str, str], str] = {
    # Приложения.
    ("GET", "/public/v1/application"): "list_apps",
    # Отзывы и рейтинг.
    ("GET", "/public/v1/application/{packageName}/comment"): "list_reviews",
    ("GET", "/public/v1/application/{packageName}/comment/export"): "export_reviews_csv",
    ("GET", "/public/v1/application/{packageName}/comment/statistic"): "get_app_rating",
    ("POST", "/public/v1/application/{packageName}/feedback"): "reply_to_review",
    ("GET", "/public/v1/application/{packageName}/feedback"): "get_review_answer_statuses",
    ("GET", "/public/v1/application/{packageName}/feedback/{feedbackId}"): "get_review_answer_status",
    ("POST", "/public/v1/application/{packageName}/feedback/{feedbackId}"): "update_review_answer",
    ("DELETE", "/public/v1/application/{packageName}/feedback/{feedbackId}"): "delete_review_answer",
    # Доступы.
    ("GET", "/public/v1/application/{packageName}/developer"): "list_app_access",
    ("PUT", "/public/v1/application/{packageName}/developer/{devVkId}/role/{roleName}"): "grant_app_access",
    ("DELETE", "/public/v1/application/{packageName}/developer/{devVkId}/role/{roleName}"): "revoke_app_access",
    # Продуктовый каталог.
    ("GET", "/public/applications/{appId}/catalog/products"): "list_catalog_products",
    ("GET", "/public/applications/{appId}/catalog/subscriptions"): "list_catalog_subscriptions",
    # Платежи и покупки; варианты для тестовых платежей — с суффиксом _sandbox.
    ("GET", "/public/v2/invoices/{invoiceId}"): "get_payment",
    ("GET", "/public/sandbox/v2/invoices/{invoiceId}"): "get_payment_sandbox",
    ("GET", "/public/applications/{appId}/invoices"): "list_payments_by_date",
    ("PATCH", "/public/applications/{appId}/invoices/{invoiceId}"): "refund_payment",
    ("PATCH", "/public/sandbox/applications/{appId}/invoices/{invoiceId}"): "refund_payment_sandbox",
    ("GET", "/public/applications/{appId}/purchases"): "list_user_purchases",
    ("GET", "/public/sandbox/applications/{appId}/purchases"): "list_user_purchases_sandbox",
    ("PUT", "/public/applications/{appId}/purchases/{purchaseId}:confirm"): "confirm_purchase",
    ("PUT", "/public/sandbox/applications/{appId}/purchases/{purchaseId}:confirm"): "confirm_purchase_sandbox",
    ("PUT", "/public/applications/{appId}/purchases/{purchaseId}:cancel"): "cancel_purchase",
    ("PUT", "/public/sandbox/applications/{appId}/purchases/{purchaseId}:cancel"): "cancel_purchase_sandbox",
    # Подписки.
    ("GET", "/public/v4/subscription/{packageName}/{subscriptionId}/{purchaseId}"): "get_subscription",
    ("GET", "/public/sandbox/v4/subscription/{packageName}/{subscriptionId}/{purchaseId}"): "get_subscription_sandbox",
    ("GET", "/public/v5/subscription/{packageName}/{subscriptionId}/{purchaseId}"): "get_subscription_v5",
    ("GET", "/public/sandbox/v5/subscription/{packageName}/{subscriptionId}/{purchaseId}"): "get_subscription_v5_sandbox",
    ("POST", "/public/v2/subscription/{packageName}/{subscriptionId}/{purchaseId}:acknowledge"): "acknowledge_subscription",
    ("POST", "/public/sandbox/v2/subscription/{packageName}/{subscriptionId}/{purchaseId}:acknowledge"): "acknowledge_subscription_sandbox",
    ("PATCH", "/public/v1/applications/{appId}/subscriptions/{purchaseId}:cancel"): "cancel_subscription",
    ("PATCH", "/public/sandbox/v1/applications/{appId}/subscriptions/{purchaseId}:cancel"): "cancel_subscription_sandbox",
    # Приложения: черновики и версии.
    ("POST", "/public/v1/application/{packageName}/version"): "create_draft_version",
    ("GET", "/public/v1/application/{packageName}/version"): "list_versions",
    ("GET", "/public/v2/application/{packageName}/version/{versionId}"): "get_version",
    ("PATCH", "/public/v2/application/{packageName}/version/{versionId}"): "update_draft_version",
    ("DELETE", "/public/v1/application/{packageName}/version/{versionId}"): "delete_draft_version",
    ("POST", "/public/v1/application/{packageName}/version/{versionId}/apk"): "upload_apk",
    ("POST", "/public/v1/application/{packageName}/version/{versionId}/aab"): "upload_aab",
    ("POST", "/public/v1/application/{packageName}/version/{versionId}/commit"): "submit_draft_for_moderation",
    ("POST", "/public/v1/application/{packageName}/version/{versionId}/publish-settings"): "update_publish_settings",
    ("POST", "/public/v1/application/{packageName}/version/{versionId}/publish"): "publish_version",
    ("POST", "/public/v1/application/{packageName}/version/{versionId}/archive"): "archive_version",
    # Иконка и скриншоты черновика.
    ("POST", "/public/v1/application/{packageName}/version/{versionId}/image/icon"): "upload_icon",
    ("POST", "/public/v1/application/{packageName}/version/{versionId}/image/screenshot/{orientation}/{ordinal}"): "upload_screenshot",
    ("GET", "/public/v2/application/{packageName}/version/{versionId}/image/screenshot"): "list_screenshots",
    ("DELETE", "/public/v2/application/{packageName}/version/{versionId}/image/screenshot/{id}"): "delete_screenshot",
    ("POST", "/public/v2/application/{packageName}/version/{versionId}/image/screenshot/{orientation}/{ordinal}/{type}"): "upload_screenshot_v2",
}


# ---------- исключения: уровни доступа ----------

# Правила x-access-level: (метод или «*», регулярное выражение по пути, уровень, причина).
# Первое совпадение выигрывает; если правил не нашлось — уровень по методу
# (DEFAULT_ACCESS). Уровень «destructive» сверх DELETE получают действия с последствиями
# для пользователей RuStore и денег; изменения черновика версии — уровень «write».
ACCESS_RULES: list[tuple[str, str, str, str]] = [
    # Публичные ответы на отзывы видят пользователи RuStore: создание, изменение и удаление
    # ответа — уровень удаления.
    ("POST", r"/feedback(/\{feedbackId\})?$", "destructive", "публичный ответ на отзыв"),
    # Платежи и покупки: возврат, подтверждение (списание) и отмена — деньги пользователей.
    ("PATCH", r"/invoices/\{invoiceId\}$", "destructive", "возврат средств"),
    ("PUT", r"/purchases/\{purchaseId\}:(confirm|cancel)$", "destructive", "подтверждение или отмена покупки"),
    # Подписки: подтверждение получения и отмена.
    ("*", r"/subscriptions?/.+:(acknowledge|cancel)$", "destructive", "подтверждение или отмена подписки"),
    # Доступы пользователей к приложению: выдача и изменение роли.
    ("PUT", r"/developer/\{devVkId\}/role/", "destructive", "выдача доступа"),
    # Отправка черновика на модерацию: после одобрения версия может опубликоваться сама
    # (тип публикации INSTANTLY или DELAYED).
    ("POST", r"/version/\{versionId\}/commit$", "destructive", "отправка на модерацию"),
    # Ручная публикация и архивирование меняют то, что видят пользователи RuStore.
    ("POST", r"/version/\{versionId\}/publish$", "destructive", "публикация версии"),
    ("POST", r"/version/\{versionId\}/archive$", "destructive", "архивирование версии"),
    # Настройки публикации тем же вызовом меняют процент частичной публикации уже
    # опубликованной версии (только в большую сторону, назад не вернуть), поэтому весь
    # метод — уровень удаления.
    ("POST", r"/version/\{versionId\}/publish-settings$", "destructive", "процент частичной публикации"),
    # Удаление черновика версии и скриншота черновика не задевает опубликованную версию:
    # это правка черновика, уровень записи.
    ("DELETE", r"/version/\{versionId\}$", "write", "удаление черновика"),
    ("DELETE", r"/image/screenshot/\{id\}$", "write", "удаление скриншота черновика"),
]

# Уровень по методу: GET читает, DELETE удаляет, остальные методы меняют данные.
DEFAULT_ACCESS = {"GET": "read", "DELETE": "destructive", "POST": "write", "PUT": "write", "PATCH": "write"}


# ---------- исключения: параметры, тела и ответы ----------

# operationId -> правки параметров: {имя: {поле: значение}}. Поле «in» переносит параметр,
# «required», «schema» и «description» заменяют разобранное значение.
PARAM_OVERRIDES: dict[str, dict[str, dict]] = {
    # В списке значений справочника `RELEАSE` написан с кириллической «А».
    "list_versions": {
        "filterTestingType": {
            "schema": {"type": "string", "enum": ["ALL", "RELEASE", "ALPHA"]},
            "description": (
                "Тип версии: `ALL` — релизные и альфа-версии, `RELEASE` (по умолчанию) — только "
                "опубликованные на пользователей, `ALPHA` — только версии альфа-тестирования."
            ),
        },
    },
}

# Параметры с одинаковым смыслом во всех методах: имя -> (схема, слово, которое должно быть
# в описании справочника, описание взамен). Справочник местами пишет `packageName` с типом
# number и описанием «Идентификатор приложения»: на деле это имя пакета, строка.
NAMED_PARAMS: dict[str, tuple[dict, str, str]] = {
    "packageName": ({"type": "string"}, "пакет", "Наименование пакета приложения, например `com.example.app`."),
}

# operationId -> {поле тела запроса: схема взамен разобранной}. Справочник описывает часть
# полей черновика неверно; правка — по примерам запросов и ответам того же справочника.
_CATEGORIES = {
    "type": "array",
    "items": {"type": "string"},
    "maxItems": 2,
    "description": "Категории версии, не больше двух, например `[\"health\", \"news\"]`.",
}
_DEVELOPER_CONTACTS = {
    "type": "object",
    "description": (
        "Контакты разработчика; нужен хотя бы один способ связи, если ни один не заполнен "
        "в консоли разработчика."
    ),
    "properties": {
        "email": {"type": "string", "description": "Email, например `test@mail.ru`."},
        "website": {"type": "string", "description": "Сайт, например `https://www.rustore.ru/`."},
        "vkCommunity": {"type": "string", "description": "Сообщество VK, `https://vk.com/...`."},
        "maxMessenger": {"type": "string", "description": "Канал в MAX, `https://max.ru/...`."},
    },
}
BODY_FIELD_OVERRIDES: dict[str, dict[str, dict]] = {
    # Таблица — string с примером `"health", "news"`, ответ get_version — массив строк.
    # Таблица — array, описание и пример запроса — объект с полями email, website, phone.
    "create_draft_version": {"categories": _CATEGORIES, "developerContacts": _DEVELOPER_CONTACTS},
    "update_draft_version": {"categories": _CATEGORIES, "developerContacts": _DEVELOPER_CONTACTS},
}

# operationId -> схема поля body ответа взамен таблицы. Таблица ответа создания и правки
# черновика описывает `content.versionId`, а пример ответа — число в `body`.
RESPONSE_BODY_OVERRIDES: dict[str, dict] = {
    "create_draft_version": {"type": "integer", "description": "Идентификатор версии (`versionId`)."},
    "update_draft_version": {"type": "integer", "description": "Идентификатор версии (`versionId`)."},
}

# operationId -> поля multipart-тела, если справочник описывает загрузку файла не таблицей.
MULTIPART_FIELDS: dict[str, dict] = {}

# operationId, у которых успешный ответ — файл (application/octet-stream), а не конверт JSON.
BINARY_RESPONSES: set[str] = set()


# ---------- загрузка ----------


class SourceError(Exception):
    """Справочник недоступен или отдал не то, что ожидается."""


def is_rate_limited(final_url: str, text: str) -> bool:
    return "/challenge.html" in final_url or 'data-lang-key="heading"' in text or "hash429" in final_url


def fetch(path: str, delay: float) -> str:
    url = DOCS_HOST + urllib.parse.quote(path)
    req = urllib.request.Request(url, headers={"User-Agent": "rustore-openapi-builder"})
    for attempt in range(RATE_LIMIT_RETRIES + 1):
        time.sleep(delay)
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                final, text = resp.geturl(), resp.read().decode("utf-8")
        except urllib.error.HTTPError as err:
            if err.code != 429:
                raise
            final, text = "/challenge.html", ""
        if not is_rate_limited(final, text):
            if "<article" not in text:
                raise SourceError(f"{path}: на странице нет статьи справочника (сайт поменял разметку?)")
            return text
        if attempt < RATE_LIMIT_RETRIES:
            print(f"{path}: сайт ограничил частоту запросов, жду {RATE_LIMIT_WAIT:.0f} с", file=sys.stderr)
            time.sleep(RATE_LIMIT_WAIT)
    raise SourceError(f"{path}: сайт ограничил частоту запросов (страница 429), повторите позже")


def cache_name(path: str) -> str:
    rest = path[len(DOCS_ROOT):].strip("/")
    return (rest.replace("/", "__") or "index") + ".html"


def page_name(path: str) -> str:
    """Имя страницы в отчётах и в info.x-source-pages: путь после DOCS_ROOT или «index»."""
    return path[len(DOCS_ROOT):].strip("/") or "index"


def load_pages(cache: str, offline: bool, delay: float) -> dict[str, str]:
    """Обходит справочник от корня: корневая страница, затем разделы и их страницы."""
    os.makedirs(cache, exist_ok=True)
    pages: dict[str, str] = {}
    queue = [DOCS_ROOT]
    while queue:
        path = queue.pop(0)
        if path in pages:
            continue
        file = os.path.join(cache, cache_name(path))
        if offline:
            if not os.path.exists(file):
                raise SourceError(f"{path}: страницы нет в кеше {cache}")
        else:
            text = fetch(path, delay)
            with open(file, "w", encoding="utf-8") as fh:
                fh.write(text)
        with open(file, encoding="utf-8") as fh:
            pages[path] = fh.read()
        for link in doc_links(pages[path]):
            if link not in pages and link not in queue:
                queue.append(link)
    if len(pages) < 2:
        raise SourceError("в справочнике не найдено ни одной страницы раздела")
    return pages


# ---------- HTML ----------

VOID_TAGS = {"br", "img", "hr", "input", "meta", "link", "wbr", "source", "col"}


class Node:
    __slots__ = ("tag", "attrs", "children", "parent")

    def __init__(self, tag: str, attrs: dict, parent: "Node | None"):
        self.tag = tag
        self.attrs = attrs
        self.children: list["Node | str"] = []
        self.parent = parent

    def classes(self) -> set[str]:
        return set((self.attrs.get("class") or "").split())

    def iter(self):
        yield self
        for child in self.children:
            if isinstance(child, Node):
                yield from child.iter()

    def find(self, tag: str, cls: str | None = None) -> "Node | None":
        for node in self.iter():
            if node.tag == tag and (cls is None or cls in node.classes()):
                return node
        return None


class TreeBuilder(HTMLParser):
    """Простое дерево элементов; незакрытые теги закрываются по ближайшему предку."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.root = Node("#root", {}, None)
        self.current = self.root

    def handle_starttag(self, tag, attrs):
        node = Node(tag, {k: v or "" for k, v in attrs}, self.current)
        self.current.children.append(node)
        if tag not in VOID_TAGS:
            self.current = node

    def handle_startendtag(self, tag, attrs):
        self.current.children.append(Node(tag, {k: v or "" for k, v in attrs}, self.current))

    def handle_endtag(self, tag):
        node = self.current
        while node is not None and node.tag != tag:
            node = node.parent
        if node is not None and node.parent is not None:
            self.current = node.parent

    def handle_data(self, data):
        self.current.children.append(data)


def parse_html(text: str) -> Node:
    builder = TreeBuilder()
    builder.feed(text)
    builder.close()
    return builder.root


# Невидимые символы и управляющие коды: в справочнике встречаются мягкие переносы,
# пробелы нулевой ширины и даже NUL внутри слов (в заголовке «Параметры взаимодействия»).
INVISIBLE = dict.fromkeys(
    [0xAD, 0x200B, 0x200C, 0x200D, 0x2060, 0xFEFF, *(c for c in range(32) if c not in (9, 10))],
    None,
)


def text_of(node: "Node | str", code_marks: bool = True) -> str:
    """Текст узла; `code` — в обратных кавычках, `br` — перевод строки."""
    if isinstance(node, str):
        return node.translate(INVISIBLE).replace(chr(0xA0), " ")
    if node.tag in ("svg", "button", "script", "style"):
        return ""
    if node.tag == "br":
        return "\n"
    if node.tag == "li":
        return "\n• " + "".join(text_of(child, code_marks) for child in node.children) + "\n"
    inner = "".join(text_of(child, code_marks) for child in node.children)
    if node.tag == "a" and "hash-link" in node.classes():
        return ""
    if node.tag == "code" and code_marks and node.parent is not None and node.parent.tag != "pre":
        stripped = inner.strip()
        return f"`{stripped}`" if stripped else ""
    return inner


def clean(text: str) -> str:
    """Пробелы схлопываются, переводы строк сохраняются."""
    lines = [" ".join(line.split()) for line in text.split("\n")]
    return "\n".join(line for line in lines if line).strip()


def code_text(node: Node) -> str:
    """Содержимое блока кода: строки Docusaurus разделены `br` внутри `span.token-line`."""
    return text_of(node, code_marks=False).strip("\n")


def doc_links(html: str) -> list[str]:
    """Ссылки на страницы справочника: боковая панель и статья (без якорей)."""
    tree = parse_html(html)
    out = []
    for node in tree.iter():
        if node.tag != "a":
            continue
        href = node.attrs.get("href", "").split("#")[0].rstrip("/")
        if href == DOCS_ROOT or href.startswith(DOCS_ROOT + "/"):
            if href not in out:
                out.append(href)
    return out


def sidebar_labels(html: str) -> dict[str, str]:
    """Подписи ссылок боковой панели: путь страницы -> текст ссылки."""
    tree = parse_html(html)
    menu = tree.find("ul", "theme-doc-sidebar-menu")
    labels: dict[str, str] = {}
    if menu is None:
        return labels
    for node in menu.iter():
        if node.tag == "a":
            href = node.attrs.get("href", "").split("#")[0].rstrip("/")
            labels.setdefault(href, clean(text_of(node)))
    return labels


# ---------- разбор страницы ----------

BLOCK_TAGS = {"h1", "h2", "h3", "h4", "h5", "h6", "p", "table", "pre", "li"}


def blocks(article: Node) -> list[tuple]:
    """Статья как последовательность блоков: ("h", уровень, текст), ("p", текст),
    ("code", текст), ("table", заголовки, строки)."""
    out: list[tuple] = []

    def walk(node: Node) -> None:
        for child in node.children:
            if not isinstance(child, Node):
                # Текст прямо в контейнере (например, метод «GET» перед блоком кода).
                text = clean(text_of(child))
                if text:
                    out.append(("p", text))
                continue
            if child.tag in ("nav", "button", "svg", "script", "style"):
                continue
            if "theme-doc-toc-mobile" in child.classes():
                continue
            if child.tag[0] == "h" and child.tag[1:].isdigit():
                out.append(("h", int(child.tag[1:]), clean(text_of(child))))
            elif child.tag == "table":
                out.append(table_block(child))
            elif child.tag == "pre":
                out.append(("code", code_text(child)))
            elif child.tag in ("p", "li"):
                # Внутри пункта списка бывают вложенные списки и блоки кода.
                if any(isinstance(c, Node) and c.tag in ("ul", "ol", "pre", "table") for c in child.children):
                    own = "".join(text_of(c) for c in child.children if not (isinstance(c, Node) and c.tag in ("ul", "ol", "pre", "table")))
                    if clean(own):
                        out.append(("p", clean(own)))
                    walk(child)
                else:
                    text = clean(text_of(child))
                    if text:
                        out.append(("p", text))
            else:
                walk(child)

    walk(article)
    return out


def table_block(table: Node) -> tuple:
    header: list[str] = []
    rows: list[list[str]] = []
    for tr in table.iter():
        if tr.tag != "tr":
            continue
        cells = [c for c in tr.children if isinstance(c, Node) and c.tag in ("td", "th")]
        texts = [clean(text_of(c)) for c in cells]
        if all(isinstance(c, Node) and c.tag == "th" for c in cells) and not header:
            header = texts
        else:
            rows.append(texts)
    return ("table", header, rows)


def article_of(html: str) -> Node:
    tree = parse_html(html)
    article = tree.find("article")
    if article is None:
        raise SourceError("на странице нет статьи справочника")
    markdown = article.find("div", "theme-doc-markdown")
    return markdown or article


def page_title(bs: list[tuple]) -> str:
    for block in bs:
        if block[0] == "h" and block[1] == 1:
            return block[2]
    return ""


def is_deprecated(title: str, label: str) -> bool:
    return any("deprecated" in text.lower() or "устаревш" in text.lower() for text in (title, label))


INTERACTION = "параметры взаимодействия"
RESPONSE = "параметры ответа"


def squash(text: str) -> str:
    """Текст для сравнения заголовков: без пробелов (в справочнике бывает «вз аимодействия»)."""
    return re.sub(r"\s+", "", text).lower().rstrip(":")


def is_heading(block: tuple, text: str) -> bool:
    """Заголовок раздела: настоящий h2/h3 или абзац из одного жирного текста."""
    if block[0] == "h":
        return squash(block[2]).startswith(squash(text))
    return block[0] == "p" and squash(block[1]) == squash(text)


def section(bs: list[tuple], name: str) -> list[tuple]:
    """Блоки раздела `name` до следующего заголовка того же или более высокого уровня."""
    start = next((i for i, b in enumerate(bs) if is_heading(b, name)), None)
    if start is None:
        return []
    level = bs[start][1] if bs[start][0] == "h" else 2
    out = []
    for block in bs[start + 1:]:
        if block[0] == "h" and block[1] <= level:
            break
        if block[0] == "p" and (is_heading(block, INTERACTION) or is_heading(block, RESPONSE)):
            break
        out.append(block)
    return out


def endpoints(bs: list[tuple]) -> list[tuple[str, str, str, str]]:
    """Пары «метод — адрес» раздела «Параметры взаимодействия»: (метод, путь, строка
    запроса, подпись). Подпись — текст перед методом, например «Для тестовых подписок»."""
    out = []
    method = None
    caption = ""
    for block in bs:
        if block[0] == "table":
            continue
        if block[0] == "h":
            break
        text = block[1]
        tokens = sorted(
            [(m.start(), "m", m.group(1)) for m in METHOD_RE.finditer(text)]
            + [(m.start(), "u", m) for m in API_URL_RE.finditer(text)],
            key=lambda t: t[0],
        )
        if not tokens and block[0] == "p":
            caption = text
            continue
        for _, kind, value in tokens:
            if kind == "m":
                method = value
            elif method:
                path = value.group(1)
                query = (value.group(2) or "").lstrip("?")
                out.append((method, path, query, caption))
                method = None
    return out


def before_interaction(bs: list[tuple]) -> list[tuple]:
    """Блоки между заголовком страницы и разделом «Параметры взаимодействия»: на части
    страниц метод и адрес стоят над этим разделом."""
    start = next((i for i, b in enumerate(bs) if is_heading(b, INTERACTION)), None)
    if start is None:
        return []
    return [b for b in bs[:start] if not (b[0] == "h" and b[1] == 1)]


# ---------- таблицы параметров ----------

COLUMN_KEYS = [
    ("name", ("атрибут", "параметр", "название", "поле")),
    ("type", ("тип",)),
    ("required", ("обязател",)),
    ("in", ("располож",)),
    ("description", ("описание",)),
    ("example", ("пример", "значени")),
]


def columns(header: list[str]) -> dict[str, int]:
    out: dict[str, int] = {}
    for index, title in enumerate(header):
        low = title.lower().replace(" ", "")
        for key, words in COLUMN_KEYS:
            if key not in out and any(low.startswith(w) for w in words):
                out[key] = index
                break
    return out


def is_yes_no(text: str) -> bool:
    return bool(re.match(r"^(да|нет)\b", text.strip().lower()))


def table_rows(header: list[str], rows: list[list[str]]) -> list[dict]:
    cols = columns(header)
    if "name" not in cols:
        return []
    out = []
    for row in rows:
        def cell(key: str) -> str:
            index = cols.get(key)
            return row[index] if index is not None and index < len(row) else ""

        item = {key: cell(key) for key, _ in COLUMN_KEYS}
        # Обязательность бывает списком («• Нет • Да, если publishType = DELAYED»): решает
        # первый пункт, остальное — в описание.
        required = item["required"].strip()
        if required.startswith("•"):
            parts = [p.strip(" ;") for p in required.split("•") if p.strip()]
            item["required"] = parts[0] if parts else ""
            if len(parts) > 1:
                item["description"] = f"{item['description']}\nОбязательность: {'; '.join(parts)}."
        # В части таблиц ячейки «Описание» и «Обязательность» стоят не под своими заголовками.
        if not is_yes_no(item["required"]) and is_yes_no(item["description"]):
            item["required"], item["description"] = item["description"], item["required"]
        raw = item["name"]
        # В ячейке имени бывает пометка «New» на следующей строке: имя — первое значение в
        # обратных кавычках, иначе первая строка.
        quoted = re.search(r"`([^`]+)`", raw)
        name = (quoted.group(1) if quoted else raw.split("\n")[0]).replace("`", "").strip()
        if not name:
            continue
        item["raw_name"] = name
        item["name"] = re.sub(r"(\{\}|\[\])+$", "", name).strip()
        item["container"] = "array" if name.endswith("[]") else "object" if name.endswith("{}") else ""
        out.append(item)
    return out


def norm_type(text: str) -> str:
    return text.replace("`", "").strip().lower()


def map_type(type_text: str, example: str, container: str, param: bool = False) -> dict:
    kind = norm_type(type_text)
    if container == "array" or kind.startswith(("array", "massive", "list", "массив")):
        return {"type": "array"}
    if container == "object" or kind.startswith(("object", "объект")):
        return {"type": "object"}
    if kind.startswith(("bool",)):
        return {"type": "boolean"}
    if kind.startswith(("int", "long")):
        return {"type": "integer"}
    if kind.startswith(("number", "float", "double", "decimal", "числ")):
        # Справочник пишет number и для идентификаторов, и для дробных значений: параметр
        # запроса с целым примером (или без примера) считается целым, поле ответа — числом.
        if not param:
            return {"type": "number"}
        sample = example.replace("`", "").strip()
        return {"type": "integer"} if re.fullmatch(r"-?\d+", sample) or not sample or sample == "N/A" else {"type": "number"}
    if kind.startswith(("timestamp", "datetime", "date-time")):
        return {"type": "string", "format": "date-time"}
    if kind == "date":
        return {"type": "string", "format": "date"}
    if kind.startswith(("file", "binary", "multipart", "файл")):
        return {"type": "string", "format": "binary"}
    return {"type": "string"}


ENUM_ITEM_RE = re.compile(r"^\s*[•\-–]\s*`([A-Za-z0-9_\-.+]+)`\s*(?:[—–-]|[;.,]?\s*$)")
BULLET_RE = re.compile(r"^\s*[•\-–]\s*\S")


def enum_values(*cells: str) -> list[str]:
    """Значения из списка вида «• `ADMIN` — администратор» (в описании или примере): все
    пункты списка начинаются со значения в обратных кавычках, пунктов не меньше двух."""
    for cell in cells:
        bullets = [line for line in cell.split("\n") if BULLET_RE.match(line)]
        items = [ENUM_ITEM_RE.match(line) for line in bullets]
        if len(bullets) >= 2 and all(items):
            return [m.group(1) for m in items if m]
    return []


def describe(row: dict) -> str:
    text = row["description"].strip()
    if text in ("N/A", "-", "—"):
        text = ""
    example = row["example"].strip()
    # «См. пример» отсылает к примеру запроса на странице: в схеме он бесполезен.
    no_example = example in ("N/A", "-", "—") or example.lower().rstrip(".") == "см. пример"
    if example and not no_example and not enum_values(example) and len(example) <= 80:
        sample = example if example.startswith("`") else f"`{example}`"
        separator = "" if not text else " " if text.endswith((".", ":")) else ". "
        text = f"{text}{separator}Пример: {sample}.".strip() if "\n" not in example else text
    elif enum_values(example) and not enum_values(text):
        text = f"{text}\n{example}".strip()
    return text


def property_schema(row: dict, param: bool = False) -> dict:
    schema = map_type(row["type"], row["example"], row["container"], param)
    values = enum_values(row["description"], row["example"])
    if values and schema["type"] == "string":
        schema["enum"] = values
    elif values and schema["type"] == "array":
        schema["items"] = {"type": "string", "enum": values}
    description = describe(row)
    if description:
        schema["description"] = description
    return schema


def nested_tables(bs: list[tuple]) -> list[tuple[str, list[dict]]]:
    """Таблицы раздела с заголовком над ними: («body.content[]», строки)."""
    out = []
    heading = ""
    for block in bs:
        if block[0] == "h":
            heading = block[2].replace("`", "").strip()
        elif block[0] == "table":
            out.append((heading, table_rows(block[1], block[2])))
            heading = ""
    return out


def build_object(rows: list[dict], subtables: list[tuple[str, list[dict]]], param: bool = False) -> dict:
    """Схема объекта из строк таблицы; вложенные объекты и массивы — из таблиц с
    заголовком вида `имя{}`, `имя[]` или `родитель.имя[]`."""
    used: set[int] = set()

    def find_sub(name: str) -> list[dict] | None:
        for index, (heading, sub) in enumerate(subtables):
            if index in used:
                continue
            last = re.split(r"[.]", re.sub(r"(\{\}|\[\])+", "", heading))[-1].strip()
            if last == name:
                used.add(index)
                return sub
        return None

    def build(rows: list[dict]) -> dict:
        properties: dict[str, dict] = {}
        required: list[str] = []
        for row in rows:
            schema = property_schema(row, param)
            if schema["type"] in ("object", "array"):
                sub = find_sub(row["name"])
                if sub:
                    inner = build(sub)
                    if schema["type"] == "array":
                        schema["items"] = inner
                    else:
                        schema.update({k: v for k, v in inner.items() if k != "type"})
            properties.setdefault(row["name"], schema)
            if row["required"].strip().lower().startswith("да") and row["name"] not in required:
                required.append(row["name"])
        out: dict = {"type": "object", "properties": properties}
        if required:
            out["required"] = required
        return out

    return build(rows)


# ---------- сборка операции ----------


def camel_words(name: str) -> str:
    return "_".join(w for w in re.split(r"[^a-zA-Z0-9]+", name) if w).lower()


def fallback_name(page: str, method: str, sandbox: bool) -> str:
    slug = camel_words(page.rsplit("/", 1)[-1])
    name = slug if slug[:1].isalpha() else f"{method.lower()}_{slug}"
    if sandbox:
        name = f"{name}_sandbox"
    return name[:40].rstrip("_")


def access_level(method: str, path: str) -> tuple[str, str]:
    for rule_method, pattern, level, reason in ACCESS_RULES:
        if rule_method in (method, "*") and re.search(pattern, path):
            return level, reason
    return DEFAULT_ACCESS[method], ""


def tag_for(page: str, path: str) -> str:
    section_name = page.split("/", 1)[0]
    rules = SECTION_TAGS.get(section_name)
    if rules is None:
        raise SourceError(f"{page}: раздел {section_name} без набора инструментов (SECTION_TAGS)")
    for pattern, tag in rules:
        if not pattern or re.search(pattern, path):
            return tag
    return rules[-1][1]


def summary_text(title: str) -> str:
    return re.sub(r"\s*\[DEPRECATED\]\s*", " ", title, flags=re.I).strip()


def intro(bs: list[tuple]) -> str:
    """Абзацы до раздела «Параметры взаимодействия» (без подсказок о сценариях)."""
    out = []
    for block in bs:
        if block[0] == "h" and block[1] == 1:
            continue
        if is_heading(block, INTERACTION) or block[0] in ("h", "table", "code"):
            break
        text = block[1]
        # Заголовки врезок Docusaurus: сам текст врезки остаётся.
        if text.lower() in (
            "подсказка",
            "примечание",
            "важно",
            "внимание",
            "информация",
            "предупреждение",
            "к сведению",
            "important",
            "note",
            "tip",
            "info",
            "warning",
            "caution",
            "danger",
        ):
            continue
        if "если не знаете с чего начать" in text.lower():
            continue
        out.append(text)
    return "\n".join(out)


def response_info(bs: list[tuple]) -> tuple[dict | None, bool]:
    """Схема поля body конверта и признак файла в ответе."""
    resp = section(bs, RESPONSE)
    if any(b[0] == "h" and "содержание файла" in b[2].lower() for b in resp) or any(
        b[0] == "h" and "содержание файла" in b[2].lower() for b in bs
    ):
        return None, True
    tables = nested_tables(resp)
    if not tables:
        return None, False
    _, top = tables[0]
    body_row = next((r for r in top if r["name"] == "body"), None)
    if body_row is None:
        return None, False
    sub = [t for t in tables[1:]]
    first = next((i for i, (h, _) in enumerate(sub) if re.sub(r"(\{\}|\[\])+", "", h).strip() == "body"), None)
    if first is None:
        return {"description": describe(body_row)} if describe(body_row) else None, False
    _, rows = sub.pop(first)
    schema = build_object(rows, sub)
    return schema, False


def heading_location(heading: str) -> str:
    """Расположение параметров по заголовку таблицы: «Path-параметры» -> path и т. д."""
    low = squash(heading)
    for words, where in (
        (("path", "пути"), "path"),
        (("query", "строкизапроса", "запроса"), "query"),
        (("header", "заголов"), "header"),
        (("body", "телозапроса", "тело"), "body"),
    ):
        if any(word in low for word in words):
            return where
    return ""


def build_operation(page: str, url: str, bs: list[tuple], endpoint: tuple, sandbox: bool, report: list[str]) -> tuple[str, str, dict]:
    method, raw_path, query, caption = endpoint
    path = raw_path.rstrip("/") or "/"
    key = (method, path)
    title = summary_text(page_title(bs))
    op_id = OPERATION_NAMES.get(key)
    if op_id is None:
        op_id = fallback_name(page, method, sandbox)
        report.append(f"{page}: {method} {path} без имени в OPERATION_NAMES, временное имя {op_id}")
    interaction = section(bs, INTERACTION)
    tables = [b for b in interaction if b[0] == "table"]
    request_rows: list[dict] = []
    sub_tables: list[tuple[str, list[dict]]] = []
    for heading, rows in nested_tables(interaction):
        # Часть страниц делит параметры на таблицы «Path-параметры», «Query-параметры»,
        # «Заголовки (Headers)» и «Тело запроса» без колонки «Расположение».
        where = heading_location(heading)
        if where and rows and not any(r["in"] for r in rows):
            for row in rows:
                row["in"] = where
            request_rows.extend(rows)
        elif not request_rows and rows and any(r["in"] for r in rows):
            request_rows = rows
        elif request_rows:
            sub_tables.append((heading, rows))
    if not tables:
        report.append(f"{page}: нет таблицы параметров запроса")
    # Поля multipart часто описаны отдельной таблицей в разделе «Form data»; часть страниц
    # описывает тело отдельным разделом «Тело запроса» (multipart — если в нём есть файл).
    form_rows: list[dict] = []
    extra_body_rows: list[dict] = []
    for block in bs:
        if block[0] != "h":
            continue
        heading = squash(block[2]).replace("-", "")
        if heading in ("formdata", "телозапроса(formdata)"):
            for _, rows in nested_tables(section(bs, block[2])):
                form_rows.extend(rows)
        elif heading == "телозапроса":
            for _, rows in nested_tables(section(bs, block[2])):
                extra_body_rows.extend(rows)

    parameters = []
    body_rows = []
    overrides = PARAM_OVERRIDES.get(op_id, {})
    placeholders = re.findall(r"\{(\w+)\}", path)
    seen = set()
    for row in request_rows:
        location = row["in"].replace("`", "").strip().lower()
        name = row["name"]
        override = overrides.get(name, {})
        location = override.get("in", location)
        if location == "header":
            if name.lower() != "public-token":
                report.append(f"{page}: неизвестный заголовок {name} пропущен")
            continue
        if location in ("body", "form", "formdata", "form-data", "multipart"):
            body_rows.append(row)
            continue
        if location not in ("path", "query"):
            report.append(f"{page}: параметр {name} в неизвестном месте «{row['in']}», считаю query")
            location = "query"
        if location == "query" and name in placeholders:
            report.append(f"{page}: параметр {name} помечен query, но он в шаблоне пути {path}, считаю path")
            location = "path"
        if location == "path" and name not in placeholders:
            report.append(f"{page}: параметр пути {name} не найден в шаблоне {path}, считаю query")
            location = "query"
        schema = property_schema(row, param=True)
        description = schema.pop("description", name)
        if name in NAMED_PARAMS:
            named_schema, word, named_description = NAMED_PARAMS[name]
            schema = dict(named_schema)
            if word not in description.lower():
                description = named_description
        description = override.get("description", description)
        if schema["type"] in ("object",):
            schema = {"type": "string"}
        if schema["type"] == "array" and "items" not in schema:
            schema["items"] = {"type": "string"}
        schema = override.get("schema", schema)
        required = location == "path" or override.get("required", row["required"].strip().lower().startswith("да"))
        if location == "path" and not row["required"].strip().lower().startswith("да"):
            report.append(f"{page}: параметр пути {name} помечен необязательным, считаю обязательным")
        if (location, name) in seen:
            continue
        seen.add((location, name))
        parameters.append({"name": name, "in": location, "required": bool(required), "description": description, "schema": schema})
    for name in placeholders:
        if ("path", name) not in seen:
            report.append(f"{page}: параметр пути {name} не описан в таблице")
            parameters.append({"name": name, "in": "path", "required": True, "description": name, "schema": {"type": "string"}})

    op: dict = {
        "operationId": op_id,
        "tags": [TAG_NAME[tag_for(page, path)]],
        "summary": title + (" (тестовые платежи)" if sandbox else ""),
    }
    description = intro(bs)
    if caption:
        description = f"{description}\n{caption}".strip()
    if description:
        op["description"] = description
    op["externalDocs"] = {"url": url}
    level, _ = access_level(method, path)
    op["x-access-level"] = level
    if parameters:
        op["parameters"] = parameters

    multipart = MULTIPART_FIELDS.get(op_id)
    body_rows = body_rows + extra_body_rows + form_rows
    file_rows = [r for r in body_rows if map_type(r["type"], r["example"], r["container"]).get("format") == "binary"]
    if multipart is not None or file_rows or form_rows:
        schema = multipart or build_object(body_rows, [], param=True)
        op["requestBody"] = {"required": True, "content": {"multipart/form-data": {"schema": schema}}}
    elif body_rows:
        schema = build_object(body_rows, sub_tables, param=True)
        for field, field_schema in BODY_FIELD_OVERRIDES.get(op_id, {}).items():
            if field not in schema.get("properties", {}):
                report.append(f"{page}: BODY_FIELD_OVERRIDES: поля {field} нет в теле {op_id}")
                continue
            schema["properties"][field] = field_schema
        op["requestBody"] = {
            "required": bool(schema.get("required")),
            "content": {"application/json": {"schema": schema}},
        }

    body_schema, is_file = response_info(bs)
    body_schema = RESPONSE_BODY_OVERRIDES.get(op_id, body_schema)
    if is_file or op_id in BINARY_RESPONSES:
        op["responses"] = {
            "200": {"description": "Файл.", "content": {"application/octet-stream": {"schema": {"type": "string", "format": "binary"}}}},
        }
    else:
        envelope: dict = {"$ref": "#/components/schemas/Envelope"}
        schema = envelope
        if body_schema:
            schema = {"allOf": [envelope, {"type": "object", "properties": {"body": body_schema}}]}
        op["responses"] = {"200": {"description": "Ответ RuStore API в конверте.", "content": {"application/json": {"schema": schema}}}}
    op["responses"]["default"] = {"$ref": "#/components/responses/Error"}
    return method.lower(), path, op


def build(pages: dict[str, str]) -> tuple[dict, list[str]]:
    report: list[str] = []
    labels: dict[str, str] = {}
    for html in pages.values():
        for path, label in sidebar_labels(html).items():
            labels.setdefault(path, label)

    paths: dict[str, dict] = {}
    source_pages = []
    for doc_path in sorted(pages):
        name = page_name(doc_path)
        source_pages.append(name)
        bs = blocks(article_of(pages[doc_path]))
        title = page_title(bs)
        found = endpoints(section(bs, INTERACTION)) or endpoints(before_interaction(bs))
        if name.split("/", 1)[0] in SECTION_TAGS and "/" not in name and not found:
            continue  # страница раздела: только ссылки
        if name in OVERVIEW_PAGES or name.replace("index", "") in OVERVIEW_PAGES:
            for method, raw_path, _, _ in found:
                key = (method, raw_path.rstrip("/"))
                if key in EXCLUDED_OPERATIONS:
                    report.append(f"{name}: {method} {raw_path} пропущен: {EXCLUDED_OPERATIONS[key]}")
            continue
        if is_deprecated(title, labels.get(doc_path, "")):
            report.append(f"{name}: устаревший метод пропущен ({summary_text(title)})")
            continue
        if not found:
            if section(bs, INTERACTION):
                report.append(f"{name}: на странице не найден метод и адрес, страница пропущена")
            continue
        url = DOCS_HOST + doc_path
        for endpoint in found:
            method, raw_path, _, caption = endpoint
            key = (method, raw_path.rstrip("/"))
            if key in EXCLUDED_OPERATIONS:
                report.append(f"{name}: {method} {raw_path} пропущен: {EXCLUDED_OPERATIONS[key]}")
                continue
            sandbox = "/sandbox/" in raw_path
            method_l, path, op = build_operation(name, url, bs, endpoint, sandbox, report)
            item = paths.setdefault(path, {})
            if method_l in item:
                report.append(f"{name}: {method} {path} уже описан на {item[method_l]['externalDocs']['url']}, повтор пропущен")
                continue
            item[method_l] = op

    for key in OPERATION_NAMES:
        method, path = key
        if method.lower() not in paths.get(path, {}):
            report.append(f"OPERATION_NAMES: {method} {path} не найден в справочнике")

    spec = {
        "openapi": "3.1.0",
        "info": {
            "title": "RuStore Public API",
            "version": "1",
            "description": (
                "Собрано скриптом build_openapi.py из справочника RuStore API "
                f"{DOCS_HOST}{DOCS_ROOT}. Ответы RuStore API — конверт Envelope; "
                "токен доступа передаётся в заголовке Public-Token."
            ),
            "x-source-pages": source_pages,
        },
        "servers": [{"url": API_SERVER}],
        "security": [{"PublicToken": []}],
        "tags": [{"name": name, "description": description} for _, name, description in TAGS],
        "paths": dict(sorted(paths.items())),
        "components": {
            "securitySchemes": {
                "PublicToken": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "Public-Token",
                    "description": "Токен доступа (JWE) из POST /public/auth/ по ключу RuStore API. Живёт 900 секунд.",
                },
            },
            "schemas": {
                "Envelope": {
                    "type": "object",
                    "description": "Конверт ответа RuStore API.",
                    "properties": {
                        "code": {"type": "string", "description": "Код ответа: OK при успехе, иначе код ошибки."},
                        "message": {"type": ["string", "null"], "description": "Расшифровка кода ответа."},
                        "body": {"description": "Тело ответа; форма зависит от метода."},
                        "timestamp": {"type": "string", "description": "Время ответа."},
                    },
                    "required": ["code"],
                },
            },
            "responses": {
                "Error": {
                    "description": "Ошибка: HTTP 4xx или 5xx, либо конверт с code не OK.",
                    "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Envelope"}}},
                },
            },
        },
    }
    # Варианты для тестовых платежей описаны на той же странице: одинаковые замечания — один раз.
    return spec, list(dict.fromkeys(report))


# ---------- проверка расхождений со справочником (--check) ----------

# Коды выхода: изменений нет, справочник недоступен или не разобран, неверные аргументы
# (их возвращает argparse), спецификация отстала.
EXIT_OK, EXIT_ERROR, EXIT_USAGE, EXIT_DRIFT = 0, 1, 2, 3


def spec_ops(spec: dict) -> dict[tuple[str, str], dict]:
    return {(m.upper(), path): op for path, item in spec.get("paths", {}).items() for m, op in item.items()}


def op_params(op: dict) -> dict[str, dict]:
    """Параметры и поля тела верхнего уровня: «query page» -> {required, schema, description}."""
    out = {}
    for p in op.get("parameters", []):
        out[f"{p['in']} {p['name']}"] = {k: p.get(k) for k in ("required", "schema", "description")}
    for media in op.get("requestBody", {}).get("content", {}).values():
        schema = media.get("schema", {})
        required = set(schema.get("required", []))
        for name, prop in schema.get("properties", {}).items():
            prop = dict(prop)
            out[f"body {name}"] = {
                "required": name in required,
                "description": prop.pop("description", None),
                "schema": prop,
            }
    return out


def param_brief(p: dict) -> str:
    schema = p.get("schema") or {}
    kind = schema.get("type") or ""
    parts = [x for x in (kind, "обязательный" if p.get("required") else "") if x]
    return f" ({', '.join(parts)})" if parts else ""


def param_changes(old: dict, new: dict) -> list[str]:
    out = []
    for key in sorted(set(old) | set(new)):
        a, b = old.get(key), new.get(key)
        if a is None:
            out.append(f"+ {key}{param_brief(b)}")
        elif b is None:
            out.append(f"- {key}{param_brief(a)}")
        elif a != b:
            what = []
            if bool(a.get("required")) != bool(b.get("required")):
                what.append("обязательный" if b.get("required") else "необязательный")
            if a.get("schema") != b.get("schema"):
                what.append("схема")
            if a.get("description") != b.get("description"):
                what.append("описание")
            out.append(f"~ {key}: {', '.join(what)}")
    return out


OP_FIELD_NAMES = {
    "summary": "заголовок",
    "description": "описание",
    "tags": "раздел",
    "externalDocs": "страница справочника",
    "x-access-level": "уровень доступа",
    "requestBody": "тело запроса",
    "responses": "ответы",
}


def drift(old: dict, new: dict) -> list[str]:
    """Разница между спецификацией в репозитории (old) и свежей сборкой (new); пусто — изменений нет."""
    lines: list[str] = []

    def section_lines(title: str, items: list[str]) -> None:
        if items:
            lines.append(f"{title} ({len(items)}):")
            lines.extend(f"  {x}" for x in items)

    old_pages = set(old.get("info", {}).get("x-source-pages", []))
    new_pages = set(new.get("info", {}).get("x-source-pages", []))
    if old_pages:
        section_lines("Новые страницы справочника", [f"+ {x}" for x in sorted(new_pages - old_pages)])
        section_lines("Удалённые страницы справочника", [f"- {x}" for x in sorted(old_pages - new_pages)])
    else:
        lines.append("В спецификации нет списка страниц (info.x-source-pages): страницы не сравниваются.")

    old_ops, new_ops = spec_ops(old), spec_ops(new)

    def title(key: tuple[str, str], op: dict) -> str:
        return f"{key[0]} {key[1]} ({op.get('operationId')}, {op.get('x-access-level')})"

    section_lines("Новые операции", [f"+ {title(k, new_ops[k])}" for k in sorted(new_ops.keys() - old_ops.keys())])
    section_lines("Удалённые операции", [f"- {title(k, old_ops[k])}" for k in sorted(old_ops.keys() - new_ops.keys())])
    renamed, params, other = [], [], []
    for key in sorted(old_ops.keys() & new_ops.keys()):
        a, b = old_ops[key], new_ops[key]
        name = f"{key[0]} {key[1]} ({b.get('operationId')})"
        if a.get("operationId") != b.get("operationId"):
            renamed.append(f"~ {key[0]} {key[1]}: {a.get('operationId')} -> {b.get('operationId')}")
        changes = param_changes(op_params(a), op_params(b))
        if changes:
            params.append(f"~ {name}: " + "; ".join(changes))
        fields = []
        for field in sorted((set(a) | set(b)) - {"parameters", "operationId"}):
            if a.get(field) != b.get(field):
                label = OP_FIELD_NAMES.get(field, field)
                if field == "x-access-level":
                    label += f" {a.get(field)} -> {b.get(field)}"
                fields.append(label)
        if fields:
            other.append(f"~ {name}: {', '.join(fields)}")
    section_lines("Изменён operationId (меняется имя инструмента)", renamed)
    section_lines("Изменены параметры и поля тела", params)
    section_lines("Изменено другое", other)

    def rest(spec: dict) -> dict:
        info = {k: v for k, v in spec.get("info", {}).items() if k != "x-source-pages"}
        return {**{k: v for k, v in spec.items() if k != "paths"}, "info": info}

    a, b = rest(old), rest(new)
    section_lines("Изменены разделы вне операций", sorted(k for k in set(a) | set(b) if a.get(k) != b.get(k)))
    return lines


def dump(spec: dict, path: str) -> None:
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(spec, fh, ensure_ascii=False, indent=2)
        fh.write("\n")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cache", help="каталог для скачанных страниц (по умолчанию — временный); держите его вне репозитория")
    ap.add_argument("--offline", action="store_true", help="не скачивать, взять страницы из кеша")
    ap.add_argument(
        "--check",
        action="store_true",
        help=(
            "не записывать --out, а сравнить с ним свежую сборку и напечатать разницу; "
            f"код выхода {EXIT_OK} — изменений нет, {EXIT_DRIFT} — есть, {EXIT_ERROR} — справочник недоступен или не разобран"
        ),
    )
    ap.add_argument("--delay", type=float, default=FETCH_DELAY, help="пауза между запросами к сайту, секунд")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "rustore.openapi.json"))
    args = ap.parse_args()
    if args.offline and not args.cache:
        ap.error("для --offline нужен --cache")
    if args.delay < 0:
        ap.error("--delay не может быть отрицательным")
    with tempfile.TemporaryDirectory(prefix="rustore-api-docs-") as tmp:
        cache = args.cache or tmp
        try:
            pages = load_pages(cache, args.offline, args.delay)
            spec, report = build(pages)
        except (OSError, SourceError) as err:
            # urllib.error.URLError и тайм-ауты — подклассы OSError.
            print(f"ошибка загрузки справочника: {err}", file=sys.stderr)
            return EXIT_ERROR
        except Exception as err:
            # Сбой разбора (сайт поменял разметку) — ошибка, а не расхождение со справочником.
            print(f"ошибка разбора справочника: {type(err).__name__}: {err}", file=sys.stderr)
            return EXIT_ERROR
        for line in report:
            print(line, file=sys.stderr)
        if not spec_ops(spec):
            print("ошибка разбора справочника: не найдено ни одной операции", file=sys.stderr)
            return EXIT_ERROR
        if not args.check:
            dump(spec, args.out)
            return EXIT_OK
        fresh = os.path.join(cache, "rustore.openapi.json")
        dump(spec, fresh)
        try:
            with open(args.out, encoding="utf-8") as fh:
                current = json.load(fh)
        except (OSError, ValueError) as err:
            print(f"не удалось прочитать {args.out}: {err}", file=sys.stderr)
            return EXIT_ERROR
        lines = drift(current, spec)
        print(
            f"Справочник: страниц {len(pages)}, операций {len(spec_ops(spec))}; "
            f"в спецификации: страниц {len(current.get('info', {}).get('x-source-pages', []))}, "
            f"операций {len(spec_ops(current))}."
        )
        if not lines:
            print("Изменений нет: спецификация совпадает со справочником.")
            return EXIT_OK
        for line in lines:
            print(line)
        print("Спецификация отстала от справочника: пересоберите её скриптом и обновите обычным PR.")
        if args.cache:
            print(f"Свежая сборка: {fresh}")
        return EXIT_DRIFT


if __name__ == "__main__":
    sys.exit(main())
