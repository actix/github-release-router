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
        return Err(error::ErrorBadRequest("invalid delivery ID"));
    }
    let event = header(&req, "x-github-event")?;
    let source = req
        .match_info()
        .get("source")
        .and_then(|s| config.sources.get(s))
        .ok_or_else(|| error::ErrorNotFound("unknown source"))?;
    if event == "ping" {
        return Ok(HttpResponse::Ok().body("pong"));
    }
    let repository = body
        .pointer("/repository/full_name")
        .and_then(Value::as_str)
        .ok_or_else(|| error::ErrorBadRequest("repository.full_name is required"))?;
    if !source.repositories.iter().any(|r| r == repository) {
        return Err(error::ErrorForbidden(
            "repository is not allowed for this source",
        ));
    }
    if event != "release" || body.get("action").and_then(Value::as_str) != Some("published") {
        tracing::info!(delivery, repository, result = "ignored", "incoming event");
        return Ok(HttpResponse::NoContent().finish());
    }
    let release: Release =
        serde_json::from_value(body.get("release").cloned().unwrap_or(Value::Null))
            .map_err(|_| error::ErrorBadRequest("invalid release payload"))?;
    let url = reqwest::Url::parse(&release.html_url)
        .map_err(|_| error::ErrorBadRequest("invalid release URL"))?;
    if release.id <= 0
        || release.tag_name.is_empty()
        || url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || release.html_url.len() > 2048
    {
        return Err(error::ErrorBadRequest("invalid release ID, tag or URL"));
    }
    let destinations = routing::destinations(&config, repository, &release);
    let inserted = store
        .enqueue(
            delivery,
            repository,
            release.id,
            &destinations,
            &discord::render(repository, &release),
        )
        .await
        .map_err(|_| {
            error::ErrorServiceUnavailable("cannot persist announcement; redeliver this event")
        })?;
    tracing::info!(delivery, repository, routes = ?destinations, result = if inserted {"persisted"} else {"duplicate"}, "incoming release");
    Ok(HttpResponse::Accepted().finish())
}

fn header<'a>(req: &'a HttpRequest, name: &str) -> actix_web::Result<&'a str> {
    let values = req.headers().get_all(name).collect::<Vec<_>>();
    if values.len() != 1 {
        return Err(error::ErrorBadRequest("missing or duplicate GitHub header"));
    }
    values[0]
        .to_str()
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| error::ErrorBadRequest("invalid GitHub header"))
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
                        error::ErrorPayloadTooLarge("payload exceeds 2 MiB")
                    }
                    _ => error::ErrorBadRequest("invalid JSON payload"),
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
    if worker.0.load(std::sync::atomic::Ordering::Relaxed)
        && sqlx::query("SELECT 1").execute(&store.0).await.is_ok()
    {
        HttpResponse::Ok().body("ready")
    } else {
        HttpResponse::ServiceUnavailable().body("not ready")
    }
}
