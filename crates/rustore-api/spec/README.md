# OpenAPI для RuStore Public API

Официальной OpenAPI-спецификации у RuStore API нет. `rustore.openapi.json` собран скриптом `build_openapi.py` из HTML-страниц [справочника RuStore API](https://www.rustore.ru/help/work-with-rustore-api). Список страниц не задан заранее: скрипт обходит корневую страницу справочника, страницы разделов и ссылки из боковой панели.

## Пересборка

```bash
python3 crates/rustore-api/spec/build_openapi.py --cache /tmp/rustore-api-docs
```

- `--cache` — каталог для скачанных страниц (без него — временный каталог, который удаляется после сборки); держите его вне репозитория;
- `--offline` — собрать из уже скачанных страниц, без сети (нужен `--cache`);
- `--out` — куда записать результат (по умолчанию `rustore.openapi.json` рядом со скриптом);
- `--delay` — пауза между запросами к сайту в секундах (по умолчанию 1);
- `--check` — не записывать `--out`, а сравнить с ним свежую сборку (см. ниже).

Нужен только Python 3.10+, сторонних пакетов нет. Обращений к RuStore API и ключа нет: скачиваются только публичные страницы справочника.

Сайт справочника ограничивает частоту запросов: примерно после двух десятков страниц за несколько минут вместо страницы приходит заглушка «429» («У вас большие запросы!») примерно на десять минут. Скрипт скачивает страницы по одной, при заглушке ждёт и повторяет запрос, поэтому полная сборка занимает до получаса. Если заглушка не уходит после нескольких попыток, скрипт завершается с кодом `1`. Для повторных сборок удобно один раз скачать страницы в `--cache`, а дальше собирать с `--offline`.

В stderr скрипт печатает отчёт: какие страницы и методы пропущены и почему, какие параметры справочник описывает неверно (параметр пути, помеченный необязательным или параметром запроса) и каким операциям не задано имя.

## Не отстала ли спецификация от справочника

```bash
python3 -I crates/rustore-api/spec/build_openapi.py --check
```

Скрипт скачивает страницы справочника, собирает спецификацию во временный каталог и печатает разницу с `rustore.openapi.json`:

- новые и удалённые страницы справочника (список страниц, по которым собрана спецификация, хранится в `info.x-source-pages`, в него входят и обзорные, и устаревшие страницы);
- новые и удалённые операции (метод, путь, `operationId`, уровень доступа) и смена `operationId` — она меняет имя инструмента;
- добавленные, удалённые и изменённые параметры пути и запроса и поля тела верхнего уровня;
- операции, у которых изменилось остальное (описание, тело, ответы, уровень доступа), и разделы вне операций (`components`, `tags`).

Если разницы нет, скрипт пишет «Изменений нет». Код выхода: `0` — изменений нет, `3` — спецификация отстала, `1` — справочник недоступен или не разобрался (например, сайт так и не снял ограничение частоты запросов или поменял разметку), `2` — неверные аргументы. С `--cache <каталог>` свежая сборка остаётся в `<каталог>/rustore.openapi.json`, её можно сравнить с файлом в репозитории построчно.

В CI проекта та же команда запускается отдельной ручной проверкой и по расписанию раз в неделю; при расхождении задача завершается с ошибкой, разница — в логе.

Чтобы обновить спецификацию, пересоберите её (команда в разделе «Пересборка»), просмотрите diff `rustore.openapi.json` и снапшоты операций (`INSTA_UPDATE=always cargo test -p rustore-api` или `cargo insta review`), проверьте имена и уровни доступа новых операций (таблицы `OPERATION_NAMES` и `ACCESS_RULES` в скрипте) и отправьте обычным PR.

## Что внутри

Спецификация OpenAPI 3.1: 49 операций со 59 страниц справочника (список — `info.x-source-pages`). Сервер — `https://public-api-m.rustore.ru`, токен доступа — схема безопасности `PublicToken` (заголовок `Public-Token`); генератор операций этот заголовок не делает аргументом инструмента: его добавляет исполнитель.

Кроме них в спецификации 4 операции Push API RuStore (набор `push`) — отдельного сервиса со своими хостами и учётными данными; они описаны в скрипте вручную, см. [Push API](#push-api).

- Каждая операция: `operationId` в snake_case не длиннее 40 символов (это имя MCP-инструмента), один тег — раздел справочника (набор инструментов), `summary` — заголовок страницы, `description` — вводный текст страницы, `externalDocs.url` — страница справочника, `x-access-level` — уровень доступа.
- Параметры пути и строки запроса — из таблицы «Параметры взаимодействия»; тело JSON — из строк с расположением `body` и вложенных таблиц; загрузка файлов — `multipart/form-data` с полем `file` (`format: binary`).
- Ответы — общий конверт `Envelope` (`code`, `message`, `body`, `timestamp`); схема поля `body` — из таблиц «Параметры ответа». Выгрузка отзывов в CSV (`export_reviews_csv`) отвечает файлом (`application/octet-stream`). Ошибки — `components.responses.Error` с тем же конвертом.
- Варианты методов для тестовых платежей (`/public/sandbox/...`) описаны на тех же страницах, что и основные, и стали отдельными операциями с суффиксом `_sandbox` и тем же уровнем доступа.

Не вошли:

- `POST /public/auth/` — выпуск токена доступа: его вызывает исполнитель по ключу профиля, агенту токен не нужен и не должен попадать в ответы;
- устаревшие методы (пометка «deprecated» в заголовке страницы или в боковой панели): данные платежа по токену и по идентификатору, данные и статус подписки, подтверждение получения подписки, данные подписки V2 и V3. У них есть замены: `get_payment`, `get_subscription`, `get_subscription_v5`, `acknowledge_subscription`;
- справочные страницы без методов: список категорий и поисковых тегов приложения, обзор и описание процессов.

Наборы инструментов (slug для `--toolsets`, по умолчанию — `apps`, `releases`, `reviews`):

| Набор | Раздел справочника | Операций |
|---|---|---|
| `apps` | Приложения | 1 |
| `releases` | Версии и публикация | 11 |
| `listing` | Иконка и скриншоты | 5 |
| `reviews` | Отзывы | 8 |
| `purchases` | Платежи и покупки | 11 |
| `subscriptions` | Подписки | 8 |
| `catalog` | Продуктовый каталог | 2 |
| `access` | Доступы | 3 |
| `push` | Push-уведомления | 4 |

Операции:

| Имя | Метод и путь | Уровень | Набор |
|---|---|---|---|
| `list_apps` | `GET /public/v1/application` | read | apps |
| `archive_version` | `POST /public/v1/application/{packageName}/version/{versionId}/archive` | destructive | releases |
| `create_draft_version` | `POST /public/v1/application/{packageName}/version` | write | releases |
| `delete_draft_version` | `DELETE /public/v1/application/{packageName}/version/{versionId}` | write | releases |
| `get_version` | `GET /public/v2/application/{packageName}/version/{versionId}` | read | releases |
| `list_versions` | `GET /public/v1/application/{packageName}/version` | read | releases |
| `publish_version` | `POST /public/v1/application/{packageName}/version/{versionId}/publish` | destructive | releases |
| `submit_draft_for_moderation` | `POST /public/v1/application/{packageName}/version/{versionId}/commit` | destructive | releases |
| `update_draft_version` | `PATCH /public/v2/application/{packageName}/version/{versionId}` | write | releases |
| `update_publish_settings` | `POST /public/v1/application/{packageName}/version/{versionId}/publish-settings` | destructive | releases |
| `upload_aab` | `POST /public/v1/application/{packageName}/version/{versionId}/aab` | write | releases |
| `upload_apk` | `POST /public/v1/application/{packageName}/version/{versionId}/apk` | write | releases |
| `delete_screenshot` | `DELETE /public/v2/application/{packageName}/version/{versionId}/image/screenshot/{id}` | write | listing |
| `list_screenshots` | `GET /public/v2/application/{packageName}/version/{versionId}/image/screenshot` | read | listing |
| `upload_icon` | `POST /public/v1/application/{packageName}/version/{versionId}/image/icon` | write | listing |
| `upload_screenshot` | `POST /public/v1/application/{packageName}/version/{versionId}/image/screenshot/{orientation}/{ordinal}` | write | listing |
| `upload_screenshot_v2` | `POST /public/v2/application/{packageName}/version/{versionId}/image/screenshot/{orientation}/{ordinal}/{type}` | write | listing |
| `delete_review_answer` | `DELETE /public/v1/application/{packageName}/feedback/{feedbackId}` | destructive | reviews |
| `export_reviews_csv` | `GET /public/v1/application/{packageName}/comment/export` | read | reviews |
| `get_app_rating` | `GET /public/v1/application/{packageName}/comment/statistic` | read | reviews |
| `get_review_answer_status` | `GET /public/v1/application/{packageName}/feedback/{feedbackId}` | read | reviews |
| `get_review_answer_statuses` | `GET /public/v1/application/{packageName}/feedback` | read | reviews |
| `list_reviews` | `GET /public/v1/application/{packageName}/comment` | read | reviews |
| `reply_to_review` | `POST /public/v1/application/{packageName}/feedback` | destructive | reviews |
| `update_review_answer` | `POST /public/v1/application/{packageName}/feedback/{feedbackId}` | destructive | reviews |
| `cancel_purchase` | `PUT /public/applications/{appId}/purchases/{purchaseId}:cancel` | destructive | purchases |
| `cancel_purchase_sandbox` | `PUT /public/sandbox/applications/{appId}/purchases/{purchaseId}:cancel` | destructive | purchases |
| `confirm_purchase` | `PUT /public/applications/{appId}/purchases/{purchaseId}:confirm` | destructive | purchases |
| `confirm_purchase_sandbox` | `PUT /public/sandbox/applications/{appId}/purchases/{purchaseId}:confirm` | destructive | purchases |
| `get_payment` | `GET /public/v2/invoices/{invoiceId}` | read | purchases |
| `get_payment_sandbox` | `GET /public/sandbox/v2/invoices/{invoiceId}` | read | purchases |
| `list_payments_by_date` | `GET /public/applications/{appId}/invoices` | read | purchases |
| `list_user_purchases` | `GET /public/applications/{appId}/purchases` | read | purchases |
| `list_user_purchases_sandbox` | `GET /public/sandbox/applications/{appId}/purchases` | read | purchases |
| `refund_payment` | `PATCH /public/applications/{appId}/invoices/{invoiceId}` | destructive | purchases |
| `refund_payment_sandbox` | `PATCH /public/sandbox/applications/{appId}/invoices/{invoiceId}` | destructive | purchases |
| `acknowledge_subscription` | `POST /public/v2/subscription/{packageName}/{subscriptionId}/{purchaseId}:acknowledge` | destructive | subscriptions |
| `acknowledge_subscription_sandbox` | `POST /public/sandbox/v2/subscription/{packageName}/{subscriptionId}/{purchaseId}:acknowledge` | destructive | subscriptions |
| `cancel_subscription` | `PATCH /public/v1/applications/{appId}/subscriptions/{purchaseId}:cancel` | destructive | subscriptions |
| `cancel_subscription_sandbox` | `PATCH /public/sandbox/v1/applications/{appId}/subscriptions/{purchaseId}:cancel` | destructive | subscriptions |
| `get_subscription` | `GET /public/v4/subscription/{packageName}/{subscriptionId}/{purchaseId}` | read | subscriptions |
| `get_subscription_sandbox` | `GET /public/sandbox/v4/subscription/{packageName}/{subscriptionId}/{purchaseId}` | read | subscriptions |
| `get_subscription_v5` | `GET /public/v5/subscription/{packageName}/{subscriptionId}/{purchaseId}` | read | subscriptions |
| `get_subscription_v5_sandbox` | `GET /public/sandbox/v5/subscription/{packageName}/{subscriptionId}/{purchaseId}` | read | subscriptions |
| `list_catalog_products` | `GET /public/applications/{appId}/catalog/products` | read | catalog |
| `list_catalog_subscriptions` | `GET /public/applications/{appId}/catalog/subscriptions` | read | catalog |
| `grant_app_access` | `PUT /public/v1/application/{packageName}/developer/{devVkId}/role/{roleName}` | destructive | access |
| `list_app_access` | `GET /public/v1/application/{packageName}/developer` | read | access |
| `revoke_app_access` | `DELETE /public/v1/application/{packageName}/developer/{devVkId}/role/{roleName}` | destructive | access |
| `send_push_notification` | `POST /messages:send` (хост push) | destructive | push |
| `send_push_to_topic` | `POST /topics/{topic}/publish` (хост топиков) | destructive | push |
| `subscribe_to_push_topic` | `POST /topics/{topic}/batch_subscribe` (хост топиков) | write | push |
| `unsubscribe_from_push_topic` | `POST /topics/{topic}/batch_unsubscribe` (хост топиков) | write | push |

## Уровни доступа

`x-access-level` у каждой операции — уровень доступа для MCP-сервера. Правила — таблица `ACCESS_RULES` в скрипте (метод, шаблон пути, уровень и причина); без правила уровень берётся по методу: `GET` — `read`, `DELETE` — `destructive`, остальные — `write`.

Кроме уровня по методу:

- `destructive` — действия с последствиями для пользователей RuStore и денег: ответы на отзывы (создание, изменение, удаление — их видят пользователи), отправка черновика на модерацию (после одобрения версия может опубликоваться сама), ручная публикация, архивирование версии, изменение настроек публикации (тем же вызовом меняется процент частичной публикации, только в большую сторону), возврат средств, подтверждение и отмена покупок, подтверждение и отмена подписок, выдача и отзыв доступа к приложению;
- `write` вместо `destructive` у `DELETE` — удаление черновика версии и скриншота черновика: опубликованную версию они не задевают;
- `read` — все `GET`. Методов чтения через `POST` в справочнике нет.

Тестовые варианты (`_sandbox`) получают уровень основного метода: шаблон правила ищется в любом месте пути, поэтому действует и на пути с `/sandbox`.

## Расхождения справочника

Справочник местами противоречит сам себе. Ниже — то, что замечено при сборке; спецификация следует таблицам справочника или, где таблица явно неверна, исключению в скрипте. Каждый пункт — открытый вопрос для проверки на реальном API (живых проверок в CI нет).

- «Пакетное управление ролями разработчиков» описывает три метода по одному пути `/public/v1/developer/roles` (GET, PUT, DELETE), каждый в своём разделе h2 со своими «Параметрами взаимодействия». Скрипт разбирает такие разделы как отдельные страницы (`method_sections`), заголовок операции — «<страница>: <раздел>». Список `appId` у GET — одна строка через запятую, как в примере справочника. Ответы этих методов — без конверта: `{"results": [...], "nextPageToken": ...}`.
- В «Получение списка платежей за дату» статусы `invoiceStatuses` перечислены заглавными (`CONFIRMED`), но API принимает только строчные, как в примере (`created`): на заглавные отвечает HTTP 400 «Incorrect parameter invoiceStatuses». Несколько статусов передаются повтором параметра. Проверено на реальном API; в спецификации — строчные значения (исключение в `PARAM_OVERRIDES`).
- Параметр пути `feedbackId` в «Получение статуса ответа на отзыв» помечен необязательным; в спецификации он обязательный, как любой параметр пути.
- `invoiceId` в «Получение данных платежа по идентификатору (v2)» помечен как параметр запроса (`query`), хотя стоит в шаблоне пути `/public/v2/invoices/{invoiceId}`; в спецификации — параметр пути.
- В «Получение списка приложений» параметры `pagination`, `continuationToken`, `pageSize`, `appName`, `appPackage`, `orderFields`, `sortDirections` помечены `path`, но в шаблоне пути их нет; в спецификации — параметры запроса. `pageSize` описан как string, хотя это число от 1 до 1000.
- `packageName` в загрузке иконки и скриншотов (v1) описан типом number и как «Идентификатор приложения»; в спецификации — строка, имя пакета, как в остальных методах.
- Создание и редактирование черновика: таблица ответа описывает `content{}` с полем `versionId`, а пример ответа — число прямо в `body` (`"body": 243242`). В спецификации `body` — целое число, идентификатор версии.
- Создание и редактирование черновика: `categories` в таблице — string с примером `"health", "news"`, в ответе «Получение детальной информации по версии» — массив строк. В спецификации тела запроса — массив строк, не больше двух.
- Создание и редактирование черновика: `developerContacts` в таблице — array («массив полей»), а описание и пример запроса — объект с полями `email`, `website`, `vkCommunity`, `maxMessenger`. В спецификации — объект.
- «Получение статуса версий приложений»: параметр запроса называется `filterTestingType`, таблица ответа тоже пишет `filterTestingType`, а пример ответа — `testingType` (к тому же значение `alpha` без кавычек, это не JSON). В «Получение детальной информации по версии» поле ответа — `testingType`. В значениях `filterTestingType` справочник пишет `RELEАSE` с кириллической «А»; в спецификации — `RELEASE`.
- Метод выпуска токена записан с завершающим `/` (`/public/auth/`), остальные методы — без него. Как API отвечает на путь без `/` и на другие методы с `/`, справочник не говорит.
- «Удаление скриншота» и «Загрузка скриншотов v2» указывают только путь, без хоста `https://public-api-m.rustore.ru`; в спецификации хост общий.
- «Загрузка APK-файла»: в примечании параметр назван `IsMainApk`, в таблице — `isMainApk` (в спецификации — второе). Тип поля `file` в таблицах загрузки — `multipart`, в «Загрузка скриншотов v2» — `binary`.
- «Изменение настроек публикации» одним методом меняет и тип и дату публикации, и процент частичной публикации уже опубликованной версии. Справочник не разделяет эти действия, поэтому у всего метода уровень `destructive`.
- «Отзыв доступа»: в списке ролей `roleName` есть роли владельца (`OWNER`, `NON_RESIDENT_OWNER`, `INDIVIDUAL_OWNER`); можно ли отозвать доступ владельца, справочник не говорит.
- В таблице продуктового каталога колонки «Описание» и «Обязательность» стоят в другом порядке, чем в заголовке, в таблицах ответа части страниц — тоже; скрипт определяет обязательность по значению «Да» или «Нет».
- Поле `body{}` в таблицах ответа часто имеет тип `object`, а в примере ответа — массив (списки отзывов, статусы ответов на отзывы).
- Опечатки разметки: пробелы внутри слов («Пар аметры ответа», «Ток ен»), невидимые символы (NUL, мягкий перенос, неразрывный пробел) в заголовках и ячейках, пометка «New» и «(Новый атрибут)» в ячейке имени параметра. Скрипт сравнивает заголовки без пробелов и вырезает невидимые символы; имена параметров берутся из значения в обратных кавычках.

## Ограничения

- Схемы ответов построены из таблиц «Параметры ответа» и нужны агенту как справка: генератор операций и исполнитель их не проверяют. Справочник часто пишет у поля `body` тип `object`, а в примере ответа — массив.
- Тип `number` справочника у параметров с целым примером (идентификаторы, номера страниц) записан как `integer`, у полей ответа — как `number`.
- Значения перечислений (`enum`) взяты из списков вида «`ADMIN` — администратор» в таблицах; если справочник перечисляет не все значения, API может принять и другие.

## Push API

Push API RuStore — не часть RuStore Public API: у него свои хосты, свой токен и свой формат ошибок, а страницы — в разделе SDK, а не в справочнике RuStore API. Таблиц «Параметры взаимодействия» на них нет, поэтому разбор страниц к нему не применяется: операции, схемы и уровни описаны в скрипте вручную (`PUSH_PATHS`, `PUSH_SCHEMAS`) и добавляются к собранной спецификации при каждой сборке, в том числе `--offline` (страницы push в кэше для сборки не нужны).

Источники, проверены 2026-11-17 (страницы операций — `info.x-push-pages`):

- [Отправка push-уведомлений](https://www.rustore.ru/help/sdk/push-notifications/send-push-notifications): `POST https://vkpns.rustore.ru/v1/projects/{project_id}/messages:send`, заголовок `Authorization: Bearer <сервисный токен>`, тело `{"message": {"token", "data", "notification", "android"}}`, успех — `200` и `{}`, ошибки — `{"error": {"code", "message", "status"}}` (`INVALID_ARGUMENT`, `INTERNAL`, `TOO_MANY_REQUESTS`, `PERMISSION_DENIED`, `NOT_FOUND`), «Максимальный объем сообщения 4096 байт».
- [Push-уведомления по топикам](https://www.rustore.ru/help/sdk/push-notifications/topic-notifications): `POST https://vkpns-topics.rustore.ru/v1/projects/{project_id}/topics/{topic}/publish`, `.../batch_subscribe` и `.../batch_unsubscribe` (тело `{"push_tokens": [...]}`); частичный успех — `200` со списком `errors` по push-токенам, ошибки — `{"code", "status", "message"}` (например `2000 BAD_REQUEST`, `1001 FORBIDDEN`).
- [Тестирование push-уведомлений](https://www.rustore.ru/help/sdk/push-notifications/test-push-notifications): идентификатор проекта и сервисный токен — в консоли («Приложение → Push-уведомления → Проекты»), у проекта до пяти сервисных токенов.

В спецификации у операций push свои `servers` с переменной `project_id` (исполнитель подставляет проект из профиля, агенту это не аргумент) и схема безопасности `PushServiceToken` (`http bearer`); генератор относит операцию к сервису по адресу сервера. Пути — относительно проекта: `/messages:send`, `/topics/{topic}/...`.

`--check` дополнительно скачивает (или с `--offline` берёт из кэша, файлы `push__*.html`) страницы push и проверяет, что на них остались ключевые факты: хосты, пути, лимит 4096 байт, статусы ошибок. Пропавший факт — расхождение (код `3`); страница, которой нет в кэше при `--offline`, пропускается с пометкой в stderr.

Расхождения и допущения:

- Таблица параметров публикации в топик перечисляет поля сообщения на верхнем уровне тела, а пример оборачивает их в `message`; спецификация следует примеру.
- Лимит 4096 байт назван только для отправки на устройство; сервер применяет его и к публикации в топик.
- `validate_only` (проверка без отправки), как у похожих API, справочник не описывает: безопасного запроса для проверки токена нет.
- Переезд на домены с суффиксом `-m` касается RuStore Public API (`public-api-m.rustore.ru`), а не хостов push.
- Универсальный Push API (`vkpns-universal.rustore.ru`, токен в теле запроса) не описан: токен в теле плохо сочетается с вырезанием секретов, а отправку покрывает основной API.
