//! Ход долгих составных инструментов и часы ожидания.
//!
//! - [`Progress`] — получатель сообщений о ходе вызова. Сервер создаёт его, когда клиент
//!   передал `progressToken` в `_meta` запроса `tools/call`, и отправляет каждое сообщение
//!   уведомлением `notifications/progress`. Без токена получателя нет, и сообщения
//!   отбрасываются: инструмент от этого не меняется.
//! - [`Clock`] — время и сон для инструментов с ожиданием (`wait_for_moderation`). На сервере
//!   это часы `tokio`; в тестах — ручные часы ([`Clock::manual`]): сон только переводит их
//!   вперёд, поэтому ожидание длиной в минуты проходит мгновенно и без настоящих таймеров
//!   (`tokio::time::pause` не подходит: на остановленном времени HTTP-клиент тестов получает
//!   ложные таймауты, пока ждёт ответа `wiremock`).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Будущее отправки одного сообщения о ходе.
pub type ProgressFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Одно сообщение о ходе: поля `progress`, `total` и `message` уведомления MCP.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressUpdate {
    /// Сколько сделано; растёт с каждым сообщением.
    pub progress: f64,
    /// Сколько всего, если известно.
    pub total: Option<f64>,
    /// Текст для пользователя (английский).
    pub message: String,
}

/// Получатель сообщений о ходе вызова. Дёшево клонируется.
#[derive(Clone)]
pub struct Progress(Arc<dyn Fn(ProgressUpdate) -> ProgressFuture + Send + Sync>);

impl Progress {
    /// Получатель, который передаёт каждое сообщение в `send`. Ошибки отправки `send`
    /// обрабатывает сам: ход вызова — подсказка клиенту, а не часть результата.
    pub fn new(send: impl Fn(ProgressUpdate) -> ProgressFuture + Send + Sync + 'static) -> Self {
        Self(Arc::new(send))
    }

    /// Отправляет сообщение.
    pub async fn report(&self, update: ProgressUpdate) {
        (self.0)(update).await;
    }
}

impl fmt::Debug for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Progress")
    }
}

/// Часы ожидания: прошедшее время и сон.
#[derive(Debug, Clone)]
pub struct Clock(ClockKind);

#[derive(Debug, Clone)]
enum ClockKind {
    /// Настоящее время: отсчёт от момента создания, сон — `tokio::time::sleep`.
    System(Instant),
    /// Ручные часы для тестов: сон переводит их вперёд без ожидания.
    Manual(Arc<Mutex<Duration>>),
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}

impl Clock {
    /// Настоящие часы.
    pub fn system() -> Self {
        Self(ClockKind::System(Instant::now()))
    }

    /// Ручные часы с нуля: [`Clock::sleep`] только прибавляет время. Клоны делят показания.
    pub fn manual() -> Self {
        Self(ClockKind::Manual(Arc::new(Mutex::new(Duration::ZERO))))
    }

    /// Время от точки отсчёта часов; разность двух показаний — прошедшее время.
    pub fn now(&self) -> Duration {
        match &self.0 {
            ClockKind::System(origin) => origin.elapsed(),
            ClockKind::Manual(now) => *now.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }

    /// Ждёт `duration`.
    pub async fn sleep(&self, duration: Duration) {
        match &self.0 {
            ClockKind::System(_) => tokio::time::sleep(duration).await,
            ClockKind::Manual(now) => {
                let mut now = now.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                *now += duration;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manual_clock_advances_on_sleep() {
        let clock = Clock::manual();
        let shared = clock.clone();
        assert_eq!(clock.now(), Duration::ZERO);
        clock.sleep(Duration::from_secs(90)).await;
        assert_eq!(shared.now(), Duration::from_secs(90));
    }

    #[tokio::test]
    async fn progress_forwards_updates() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let progress = Progress::new(move |update| {
            sink.lock().unwrap().push(update);
            Box::pin(async {})
        });
        progress
            .report(ProgressUpdate {
                progress: 1.0,
                total: Some(2.0),
                message: "half".into(),
            })
            .await;
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(format!("{progress:?}"), "Progress");
    }
}
