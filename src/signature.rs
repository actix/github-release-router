use actix_web::{
    Error, HttpRequest, error,
    web::{Bytes, Data},
};
use actix_web_lab::extract::RequestSignatureScheme;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

use crate::config::Config;

pub struct GithubSignature {
    mac: Hmac<Sha256>,
    candidate: Vec<u8>,
}

impl RequestSignatureScheme for GithubSignature {
    type Signature = ();
    type Error = Error;

    async fn init(req: &HttpRequest) -> Result<Self, Error> {
        let config = req
            .app_data::<Data<Config>>()
            .ok_or_else(|| error::ErrorInternalServerError("configuration unavailable"))?;
        let source_name = req.match_info().get("source").unwrap_or_default();
        let source = config.sources.get(source_name).ok_or_else(|| {
            tracing::warn!(reason = "unknown_source", "Webhook rejected");
            error::ErrorNotFound("unknown source")
        })?;
        if req.headers().contains_key("content-encoding") {
            tracing::warn!(
                source = source_name,
                reason = "content_encoding",
                "Webhook rejected"
            );
            return Err(error::ErrorUnsupportedMediaType(
                "content encoding is not supported",
            ));
        }
        let values = req
            .headers()
            .get_all("x-hub-signature-256")
            .collect::<Vec<_>>();
        let text = if values.len() == 1 {
            values[0].to_str().ok()
        } else {
            None
        };
        let candidate = text
            .and_then(|s| s.strip_prefix("sha256="))
            .filter(|s| s.len() == 64)
            .and_then(|s| hex::decode(s).ok())
            .ok_or_else(|| {
                tracing::warn!(
                    source = source_name,
                    reason = "missing_or_malformed_signature",
                    "Webhook rejected"
                );
                error::ErrorUnauthorized("missing or malformed signature")
            })?;
        let mac = Hmac::<Sha256>::new_from_slice(source.key.as_bytes())
            .map_err(|_| error::ErrorInternalServerError("invalid signing key"))?;
        Ok(Self { mac, candidate })
    }

    async fn consume_chunk(&mut self, _: &HttpRequest, chunk: Bytes) -> Result<(), Error> {
        self.mac.update(&chunk);
        Ok(())
    }

    async fn finalize(self, req: &HttpRequest) -> Result<(), Error> {
        self.mac.verify_slice(&self.candidate).map_err(|_| {
            tracing::warn!(
                source = req.match_info().get("source").unwrap_or_default(),
                reason = "invalid_signature",
                "Webhook rejected"
            );
            error::ErrorUnauthorized("invalid signature")
        })
    }
}
