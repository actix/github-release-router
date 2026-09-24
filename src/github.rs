use actix_web::{
    HttpRequest, HttpResponse, error,
    web::{self, Data},
};
use actix_web_lab::extract::RequestSignature;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{config::Config, discord, routing, signature::GithubSignature, store::Store};

pub const MAX_BODY: usize = 2 * 1024 * 1024;
pub type SignedBody = RequestSignature<web::Json<Value>, GithubSignature>;

#[derive(Deserialize, Serialize)]
pub struct Release {
    pub id: i64,
    pub tag_name: String,
    pub name: Option<String>,
    pub html_url: String,
    pub body: Option<String>,
    pub prerelease: bool,
}

pub async fn receive(
    req: HttpRequest,
    body: SignedBody,
    config: Data<Config>,
    store: Data<Store>,
) -> actix_web::Result<HttpResponse> {
    let (body, ()) = body.into_parts();
    let delivery = header(&req, "x-github-delivery")?;
    if delivery.len() > 128
        || !delivery
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        tracing::warn!(reason = "invalid_delivery_id", "webhook rejected");
        return Err(error::ErrorBadRequest("invalid delivery ID"));
    }
    let event = header(&req, "x-github-event")?;
    let source_name = req
        .match_info()
        .get("source")
        .ok_or_else(|| error::ErrorNotFound("unknown source"))?;
    let source = config
        .sources
        .get(source_name)
        .ok_or_else(|| error::ErrorNotFound("unknown source"))?;
    if event == "ping" {
        tracing::info!(delivery, source = source_name, "authenticated GitHub ping");
        return Ok(HttpResponse::Ok().body("pong"));
    }
    let repository = body
        .pointer("/repository/full_name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            tracing::warn!(
                delivery,
                source = source_name,
                reason = "missing_repository",
                "webhook rejected"
            );
            error::ErrorBadRequest("repository.full_name is required")
        })?;
    if !source.repositories.iter().any(|r| r == repository) {
        tracing::warn!(
            delivery,
            source = source_name,
            reason = "repository_not_allowed",
            "webhook rejected"
        );
        return Err(error::ErrorForbidden(
            "repository is not allowed for this source",
        ));
    }
    if event != "release" || body.get("action").and_then(Value::as_str) != Some("published") {
        let reason = if event == "release" {
            "release_not_published"
        } else {
            "not_release_event"
        };
        tracing::info!(
            delivery,
            source = source_name,
            repository,
            reason,
            "webhook ignored"
        );
        return Ok(HttpResponse::NoContent().finish());
    }
    let release: Release = serde_json::from_value(
        body.get("release").cloned().unwrap_or(Value::Null),
    )
    .map_err(|_| {
        tracing::warn!(
            delivery,
            source = source_name,
            repository,
            reason = "invalid_release_payload",
            "webhook rejected"
        );
        error::ErrorBadRequest("invalid release payload")
    })?;
    let url = reqwest::Url::parse(&release.html_url).map_err(|_| {
        tracing::warn!(
            delivery,
            source = source_name,
            repository,
            reason = "invalid_release_url",
            "webhook rejected"
        );
        error::ErrorBadRequest("invalid release URL")
    })?;
    if release.id <= 0
        || release.tag_name.is_empty()
        || url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || release.html_url.len() > 2048
    {
        tracing::warn!(
            delivery,
            source = source_name,
            repository,
            reason = "invalid_release_metadata",
            "webhook rejected"
        );
        return Err(error::ErrorBadRequest("invalid release ID, tag or URL"));
    }
    let destinations = routing::destinations(&config, repository, &release);
    let enqueue = store
        .enqueue(
            delivery,
            repository,
            release.id,
            &destinations,
            &discord::render(repository, &release),
        )
        .await
        .map_err(|error| {
            tracing::error!(delivery, source = source_name, repository, release_id = release.id, error = %error, "cannot persist announcement");
            error::ErrorServiceUnavailable("cannot persist announcement; redeliver this event")
        })?;
    let result = if !enqueue.delivery_inserted {
        "duplicate_delivery"
    } else if enqueue.jobs_inserted > 0 {
        "queued"
    } else if destinations.is_empty() {
        "unmatched"
    } else {
        "duplicate_release"
    };
    tracing::info!(
        delivery,
        source = source_name,
        repository,
        release_id = release.id,
        destinations = ?destinations,
        jobs_inserted = enqueue.jobs_inserted,
        result,
        "release webhook processed"
    );
    Ok(HttpResponse::Accepted().finish())
}

fn header<'a>(req: &'a HttpRequest, name: &str) -> actix_web::Result<&'a str> {
    let values = req.headers().get_all(name).collect::<Vec<_>>();
    if values.len() != 1 {
        tracing::warn!(
            header = name,
            reason = "missing_or_duplicate",
            "webhook rejected"
        );
        return Err(error::ErrorBadRequest("missing or duplicate GitHub header"));
    }
    values[0]
        .to_str()
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            tracing::warn!(header = name, reason = "invalid_value", "webhook rejected");
            error::ErrorBadRequest("invalid GitHub header")
        })
}

pub fn endpoints(cfg: &mut web::ServiceConfig) {
    cfg.app_data(
        web::JsonConfig::default()
            .limit(MAX_BODY)
            .error_handler(|err, _| {
                use actix_web::error::JsonPayloadError;
                match err {
                    JsonPayloadError::Overflow { .. }
                    | JsonPayloadError::OverflowKnownLength { .. } => {
                        tracing::warn!(reason = "payload_too_large", "webhook rejected");
                        error::ErrorPayloadTooLarge("payload exceeds 2 MiB")
                    }
                    _ => {
                        tracing::warn!(reason = "invalid_json", "webhook rejected");
                        error::ErrorBadRequest("invalid JSON payload")
                    }
                }
            }),
    )
    .route("/webhooks/github/{source}", web::post().to(receive))
    .route(
        "/healthz",
        web::get().to(|| async { HttpResponse::Ok().body("ok") }),
    )
    .route("/readyz", web::get().to(ready));
}

async fn ready(store: Data<Store>, worker: Data<crate::worker::WorkerHealth>) -> HttpResponse {
    if !worker.0.load(std::sync::atomic::Ordering::Relaxed) {
        tracing::debug!(reason = "worker_inactive", "readiness check failed");
        return HttpResponse::ServiceUnavailable().body("not ready");
    }
    if let Err(error) = sqlx::query("SELECT 1").execute(&store.0).await {
        tracing::debug!(error = %error, "readiness database check failed");
        HttpResponse::ServiceUnavailable().body("not ready")
    } else {
        HttpResponse::Ok().body("ready")
    }
}
