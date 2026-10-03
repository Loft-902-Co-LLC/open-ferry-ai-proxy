//! Which redirects the providers' HTTP clients follow.
//!
//! Our requests carry credentials where a redirect would pass them on: in
//! headers that aren't `Authorization` (`x-goog-api-key`, `x-api-key`),
//! which a client copies to any host, and in bodies (a service account's
//! signed assertion), which a 307 or 308 sends again. So a redirect is
//! followed only within the origin, the scheme, host and port, of the first
//! request; one elsewhere is answered as it came, an error with its 3xx
//! status.
//!
//! Deviations from upstream: the whole module. Upstream follows a redirect
//! to any origin, as Go's client does, which drops only `Authorization`,
//! `Www-Authenticate` and cookies on the way to another domain.

use reqwest::redirect::{Attempt, Policy};

/// How many redirects stop a request, as in Go's client: the tenth isn't
/// followed.
const MAX_REDIRECTS: usize = 10;

/// Follows a redirect within the first request's origin, up to
/// [`MAX_REDIRECTS`].
pub(crate) fn policy() -> Policy {
    Policy::custom(decide)
}

fn decide(attempt: Attempt<'_>) -> reqwest::redirect::Action {
    // `previous` holds the first request and each redirect followed.
    let previous = attempt.previous();
    if previous.len() >= MAX_REDIRECTS {
        return attempt.error(format!("stopped after {MAX_REDIRECTS} redirects"));
    }
    let same_origin = previous
        .first()
        .is_some_and(|first| first.origin() == attempt.url().origin());
    if same_origin {
        attempt.follow()
    } else {
        tracing::warn!(
            "not following a redirect to another origin, {}",
            origin_text(attempt.url())
        );
        attempt.stop()
    }
}

/// A URL's origin, for logs: its query or path could hold a secret.
fn origin_text(url: &url::Url) -> String {
    url.origin().ascii_serialization()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode, Uri, header};
    use axum::response::IntoResponse as _;

    use super::*;

    /// What a server saw: each path, `x-api-key` and body.
    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

    /// A server on 127.0.0.1 that answers a path `/to/<url>` with a 307 to
    /// that URL, `/loop` with a 307 to itself, and others with 200.
    async fn server() -> (String, Seen) {
        let seen: Seen = Arc::default();
        let recorder = Arc::clone(&seen);
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: String| {
            let recorder = Arc::clone(&recorder);
            async move {
                let key = headers
                    .get("x-api-key")
                    .map(|value| value.to_str().unwrap().to_owned())
                    .unwrap_or_default();
                let path = uri.path().to_owned();
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((path.clone(), key, body));
                let location = match path.strip_prefix("/to/") {
                    Some(target) => target.to_owned(),
                    None if path == "/loop" => "/loop".to_owned(),
                    None => return (StatusCode::OK, "done").into_response(),
                };
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, location)],
                    "moved",
                )
                    .into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, seen)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(policy())
            .build()
            .unwrap()
    }

    fn seen(seen: &Seen) -> Vec<(String, String, String)> {
        seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    async fn post(url: &str) -> reqwest::Result<reqwest::Response> {
        client()
            .post(url)
            .header("x-api-key", "the-secret-key")
            .body("assertion=signed")
            .send()
            .await
    }

    /// Whether `client` follows a redirect to another origin.
    pub(crate) async fn crosses_origins(client: &reqwest::Client) -> bool {
        let (a, _) = server().await;
        let (b, b_seen) = server().await;
        let response = client.get(format!("{a}/to/{b}/x")).send().await.unwrap();
        assert_eq!(response.status() == 200, !seen(&b_seen).is_empty());
        !seen(&b_seen).is_empty()
    }

    #[tokio::test]
    async fn follows_a_redirect_within_the_origin() {
        let (a, a_seen) = server().await;
        let response = post(&format!("{a}/to/{a}/next")).await.unwrap();
        assert_eq!(response.status(), 200);
        let seen = seen(&a_seen);
        assert_eq!(seen.len(), 2);
        let next = (
            "/next".into(),
            "the-secret-key".into(),
            "assertion=signed".into(),
        );
        assert_eq!(seen[1], next);
    }

    #[tokio::test]
    async fn answers_a_redirect_to_another_origin_as_it_came() {
        let (a, a_seen) = server().await;
        let (b, b_seen) = server().await;
        let response = post(&format!("{a}/to/{b}/elsewhere")).await.unwrap();
        assert_eq!(response.status(), 307);
        assert_eq!(response.text().await.unwrap(), "moved");
        assert_eq!(seen(&a_seen).len(), 1);
        assert!(seen(&b_seen).is_empty());

        // Another port, or another scheme, is another origin.
        let other_scheme = a.replacen("http://", "https://", 1);
        let response = post(&format!("{a}/to/{other_scheme}/x")).await.unwrap();
        assert_eq!(response.status(), 307);
        assert_eq!(seen(&a_seen).len(), 2);
    }

    #[tokio::test]
    async fn stops_at_the_tenth_redirect() {
        let (a, a_seen) = server().await;
        let error = post(&format!("{a}/loop")).await.unwrap_err();
        assert!(error.is_redirect());
        assert!(
            format!("{:?}", error).contains("stopped after 10 redirects"),
            "{error:?}"
        );
        assert_eq!(seen(&a_seen).len(), 10);
    }
}
