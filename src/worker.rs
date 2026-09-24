use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use reqwest::{Client, multipart};
use tokio::sync::watch;

use crate::{config::Config, discord::Message, store::Store};

#[derive(Clone)]
pub struct WorkerHealth(pub Arc<AtomicBool>);

pub enum Outcome {
    Sent,
    Retry {
        delay: i64,
        reason: &'static str,
        rate_limited: bool,
    },
    Failed(&'static str),
}

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("github-release-router/0.1")
        .build()?)
}

fn backoff(attempt: i64) -> i64 {
    let base = (1_i64 << attempt.clamp(1, 10)).min(900);
    base + rand::random_range(0..=(base / 4))
}

pub async fn send(client: &Client, url: &str, message: &Message, attempt: i64) -> Outcome {
    let mut request = client.post(url).query(&[("wait", "true")]);
    if let Some(notes) = &message.notes {
        let part = multipart::Part::text(notes.clone()).file_name("release-notes.md");
        request = request.multipart(
            multipart::Form::new()
                .text("payload_json", message.payload.to_string())
                .part("files[0]", part),
        );
    } else {
        request = request.json(&message.payload);
    }
    // Never expose reqwest errors: their Display representation includes the webhook token.
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => {
            return Outcome::Retry {
                delay: backoff(attempt),
                reason: "network error",
                rate_limited: false,
            };
        }
    };
    let status = response.status();
    if status.as_u16() == 429 {
        let header_delay = response
            .headers()
            .get("retry-after")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.parse::<f64>().ok());
        let body = response.json::<serde_json::Value>().await.ok();
        let body_delay = body
            .as_ref()
            .and_then(|b| b.get("retry_after"))
            .and_then(|s| s.as_f64());
        let delay = body_delay
            .into_iter()
            .chain(header_delay)
            .filter(|d| d.is_finite() && *d >= 0.0)
            .fold(1.0_f64, f64::max);
        return Outcome::Retry {
            delay: delay.ceil().min(i32::MAX as f64) as i64 + 1,
            reason: "rate limited",
            rate_limited: true,
        };
    }
    if status.is_server_error() || status.as_u16() == 408 {
        return Outcome::Retry {
            delay: backoff(attempt),
            reason: "Discord transient error",
            rate_limited: false,
        };
    }
    if status.is_success() {
        if response
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_owned))
            .is_some()
        {
            return Outcome::Sent;
        }
        return Outcome::Retry {
            delay: backoff(attempt),
            reason: "missing message confirmation",
            rate_limited: false,
        };
    }
    Outcome::Failed("Discord permanent HTTP error")
}

pub async fn run(
    store: Store,
    config: Arc<Config>,
    mut shutdown: watch::Receiver<bool>,
    health: WorkerHealth,
) -> Result<()> {
    let result = run_inner(store, config, &mut shutdown, &health).await;
    health.0.store(false, Ordering::Relaxed);
    result
}

async fn run_inner(
    store: Store,
    config: Arc<Config>,
    shutdown: &mut watch::Receiver<bool>,
    health: &WorkerHealth,
) -> Result<()> {
    let client = client()?;
    store.recover().await?;
    store.maintain().await?;
    health.0.store(true, Ordering::Relaxed);
    let mut last_maintenance = tokio::time::Instant::now();
    while !*shutdown.borrow() {
        if last_maintenance.elapsed() >= Duration::from_secs(3600) {
            store.maintain().await?;
            last_maintenance = tokio::time::Instant::now();
        }
        if let Some(job) = store.claim().await? {
            let outcome = match config.destinations.get(&job.destination) {
                Some(destination) => {
                    send(&client, &destination.url, &job.message, job.attempts).await
                }
                None => Outcome::Failed("destination removed from configuration"),
            };
            let (state, delay, reason, limited) = match outcome {
                Outcome::Sent => ("sent", 0, "confirmed", false),
                Outcome::Failed(reason) => ("failed", 0, reason, false),
                Outcome::Retry {
                    delay,
                    reason,
                    rate_limited,
                } if job.attempts < 12 => ("pending", delay, reason, rate_limited),
                Outcome::Retry {
                    delay,
                    rate_limited,
                    ..
                } => ("failed", delay, "retry budget exhausted", rate_limited),
            };
            store.finish(job.id, state, delay, reason, limited).await?;
            tracing::info!(
                delivery = job.delivery,
                repository = job.repository,
                routes = job.routes,
                destination = job.destination,
                attempt = job.attempts,
                result = state,
                reason,
                "delivery completed"
            );
        } else {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                _ = shutdown.changed() => break,
            }
        }
    }
    Ok(())
}
