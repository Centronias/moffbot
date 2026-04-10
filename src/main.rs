#![warn(
    clippy::all,
    clippy::correctness,
    clippy::suspicious,
    clippy::style,
    clippy::complexity,
    clippy::perf,
    clippy::pedantic,
    clippy::nursery
)]
#![allow(
    // Pedantic/nursery opt-outs for stylistic preferences.
    clippy::missing_docs_in_private_items,
    clippy::missing_panics_doc,
    clippy::pattern_type_mismatch,
    clippy::wildcard_enum_match_arm,
    clippy::implicit_return,
    clippy::question_mark_used,
    clippy::shadow_unrelated,
    clippy::shadow_reuse,
    clippy::shadow_same,
    clippy::too_many_lines,
    clippy::enum_glob_use
)]

use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use hmac::{Hmac, KeyInit, Mac};
use lambda_http::{
    Body, Error, Request, Response, run, service_fn, tracing,
    tracing::{debug, error},
};
use octocrab::{
    Octocrab,
    models::webhook_events::{WebhookEvent, WebhookEventType},
};
use sha2::Sha256;

use crate::pr_reviews::on_pull_request_review;

mod octo_ext;
mod pr_reviews;

static WEBHOOK_SECRET: OnceLock<Vec<u8>> = OnceLock::new();

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();

    WEBHOOK_SECRET
        .set(std::env::var("WEBHOOK_SECRET")?.into_bytes())
        .ok();

    octocrab::initialise(
        Octocrab::builder()
            .personal_token(std::env::var("GITHUB_TOKEN")?)
            .build()?,
    );

    run(service_fn(service_function)).await
}

const MOFF_ORG: &str = "moff-station";
const MOFF_REPO: &str = "moff-station-14";

async fn service_function(event: Request) -> Result<Response<Body>, Error> {
    if let Err(e) = verify_signature(&event) {
        return Ok(client_error_response(e.to_string())?);
    }

    let event = match event
        .headers()
        .get("X-GitHub-Event")
        .ok_or_else(|| anyhow::anyhow!("missing X-GitHub-Event header"))
        .and_then(|header| header.to_str().map_err(Into::into))
        .and_then(|header_value| {
            WebhookEvent::try_from_header_and_body(header_value, event.body()).map_err(Into::into)
        })
        .and_then(validate)
    {
        Ok(e) => e,
        Err(e) => return Ok(client_error_response(e.to_string())?),
    };

    let result = match event.kind {
        WebhookEventType::PullRequestReview => on_pull_request_review(event),
        other => {
            return Ok(client_error_response(format!(
                "Unexpected webhook event kind \"{other:?}\""
            ))?);
        }
    };

    let result = Box::pin(result).await.inspect_err(|e| error!("{e}"));
    let resp = match result {
        Ok(()) => Response::builder().status(200).body(Body::Empty),
        Err(e) => Response::builder()
            .status(500)
            .body(Body::Text(e.to_string())),
    }?;

    Ok(resp)
}

fn validate(event: WebhookEvent) -> Result<WebhookEvent> {
    let (org, repo) = event
        .organization
        .as_ref()
        .zip(event.repository.as_ref())
        .ok_or_else(|| anyhow!("Missing organization or repository in webhook"))?;

    // Check the source organization
    let org_name = &org.login;
    if org_name != MOFF_ORG {
        bail!("Webhook from unexpected organization \"{org_name}\"");
    }

    // Check the source repository
    if repo.name != MOFF_REPO {
        bail!("Webhook from unexpected repo \"{}\"", repo.name);
    }

    Ok(event)
}

fn client_error_response(msg: impl Into<String>) -> Result<Response<Body>> {
    let msg = msg.into();
    debug!("Client error: {}", msg.replace('\n', "\\n"));
    Ok(Response::builder().status(400).body(Body::Text(msg))?)
}

fn verify_signature(event: &Request) -> Result<()> {
    event
        .headers()
        .get("X-Hub-Signature-256")
        .ok_or_else(|| anyhow!("missing X-Hub-Signature-256 header"))
        .and_then(|v| v.to_str().map_err(Into::into))
        .and_then(|sig| {
            let body_bytes = match event.body() {
                Body::Text(s) => s.as_bytes(),
                Body::Binary(b) => b.as_slice(),
                _ => &[][..],
            };

            let hex_sig = sig
                .strip_prefix("sha256=")
                .ok_or_else(|| anyhow!("X-Hub-Signature-256 has unexpected format"))?;
            let sig_bytes = hex::decode(hex_sig)
                .map_err(|_| anyhow!("X-Hub-Signature-256 contains invalid hex"))?;

            let mut mac = Hmac::<Sha256>::new_from_slice(
                WEBHOOK_SECRET
                    .get()
                    .expect("WEBHOOK_SECRET not initialized"),
            )
            .map_err(|_| anyhow!("invalid HMAC key length"))?;
            mac.update(body_bytes);
            mac.verify_slice(&sig_bytes)
                .map_err(|_| anyhow!("webhook signature mismatch"))
        })
}

#[cfg(test)]
mod tests {
    use hmac::{Hmac, KeyInit, Mac};
    use http::Request;
    use lambda_http::Body;
    use sha2::Sha256;

    use super::{WEBHOOK_SECRET, validate, verify_signature};

    fn test_secret() -> &'static [u8] {
        WEBHOOK_SECRET.get_or_init(|| b"test-secret".to_vec())
    }

    fn make_sig(secret: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn valid_signature_passes() {
        let sig = make_sig(test_secret(), b"{}");
        let req = Request::builder()
            .header("X-Hub-Signature-256", sig)
            .body(Body::Text("{}".to_string()))
            .unwrap();
        assert!(verify_signature(&req).is_ok());
    }

    #[test]
    fn wrong_secret_fails() {
        let sig = make_sig(b"wrong-secret", b"{}");
        let req = Request::builder()
            .header("X-Hub-Signature-256", sig)
            .body(Body::Text("{}".to_string()))
            .unwrap();
        assert!(verify_signature(&req).is_err());
    }

    #[test]
    fn body_mismatch_fails() {
        let sig = make_sig(test_secret(), b"signed-body");
        let req = Request::builder()
            .header("X-Hub-Signature-256", sig)
            .body(Body::Text("different-body".to_string()))
            .unwrap();
        assert!(verify_signature(&req).is_err());
    }

    #[test]
    fn missing_header_fails() {
        let req = Request::builder().body(Body::Empty).unwrap();
        assert!(verify_signature(&req).is_err());
    }

    #[test]
    fn missing_prefix_fails() {
        let sig = make_sig(test_secret(), b"{}");
        let bare_hex = sig.strip_prefix("sha256=").unwrap().to_string();
        let req = Request::builder()
            .header("X-Hub-Signature-256", bare_hex)
            .body(Body::Text("{}".to_string()))
            .unwrap();
        assert!(verify_signature(&req).is_err());
    }

    #[test]
    fn invalid_hex_fails() {
        let req = Request::builder()
            .header("X-Hub-Signature-256", "sha256=zzzzzzzz")
            .body(Body::Empty)
            .unwrap();
        assert!(verify_signature(&req).is_err());
    }

    #[test]
    fn validate_passes_for_test_payload() {
        use octocrab::models::webhook_events::WebhookEvent;
        const PAYLOAD: &str = include_str!("../tests/test-payload.json");
        let event = WebhookEvent::try_from_header_and_body("pull_request_review", PAYLOAD).unwrap();
        assert!(validate(event).is_ok());
    }

    #[test]
    fn validate_rejects_wrong_org() {
        use octocrab::models::webhook_events::WebhookEvent;
        const PAYLOAD: &str = include_str!("../tests/test-payload-wrong-org.json");
        let event = WebhookEvent::try_from_header_and_body("pull_request_review", PAYLOAD).unwrap();
        assert!(validate(event).is_err());
    }
}
