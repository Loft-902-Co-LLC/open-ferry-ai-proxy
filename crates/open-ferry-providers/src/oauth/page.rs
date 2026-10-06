//! The page a browser lands on at the end of a sign-in. The CLI login's
//! callback server and the main server's callback pages both answer with
//! it.
//!
//! It says how the sign-in ended: signed in, failed and why, or still
//! finishing. Then it says where to go next: back to the terminal or to the
//! dashboard.
//!
//! The page is one self-contained document, in the dashboard's colours and
//! with its mark, light or dark as the system prefers:
//! - It loads nothing and runs no script.
//! - Every piece of text that wasn't written here is HTML-escaped.
//! - Each answer forbids caching, referrers, sniffing and framing.
//! - Its content security policy allows only the page's own style, by its
//!   hash, and `data:` images.
//!
//! A page reporting a failure answers 400 Bad Request. A page reporting a
//! sign-in, or one still finishing, answers 200 OK.
//!
//! Deviations from upstream:
//! - Upstream's pages have their own design and wording: the CLI callback
//!   server's `LoginSuccessHtml`, plain `http.Error` text for a failed
//!   callback, and the main server's `oauthCallbackSuccessHTML`.
//! - Upstream's pages say the sign-in succeeded before it has: the CLI's
//!   before the code is exchanged, and the main server's whatever the
//!   callback held. This page says how the sign-in ended.
//! - Upstream's pages promise that the window closes by itself, in 10
//!   seconds (the CLI's) or 5 (the main server's), with a script that calls
//!   `window.close()`, which browsers allow only for a window a script
//!   opened. This page has no script, and says the tab can be closed.
//! - The CLI's page links to the provider's site; this page links to and
//!   loads nothing.
//! - A failure page answers 400, as upstream's CLI callback answers a failed
//!   callback; the main server's pages answer 200 whatever happened.
//! - Upstream's pages are sent without security headers.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::LazyLock;

use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderValue, StatusCode, header};
use sha2::{Digest, Sha256};

use super::CALLBACK_ERRORS;

/// Where a sign-in was started, and so where the page sends the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A `-codex-login` or `-claude-login` command.
    Terminal,
    /// The dashboard, through the management API.
    Dashboard,
}

/// How a sign-in ended, as the page reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The credential is saved.
    SignedIn,
    /// The sign-in failed.
    Failed(Failure),
    /// The sign-in hadn't ended when the page had to answer.
    Finishing,
}

/// Why a sign-in failed, in plain words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// What went wrong, in a few words.
    pub title: String,
    /// What it means, in a sentence or two.
    pub meaning: String,
    /// The reason a provider or the token endpoint gave, already redacted.
    /// The page shows at most [`MAX_DETAIL_CHARS`] characters of it.
    pub detail: Option<String>,
}

/// The most characters of a failure's detail the page shows.
pub const MAX_DETAIL_CHARS: usize = 300;

impl Failure {
    /// A failure titled `title`, with `meaning` saying what happened.
    pub fn new(title: impl Into<String>, meaning: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            meaning: meaning.into(),
            detail: None,
        }
    }

    /// The failure with `detail`, which must already be redacted, unless it
    /// is empty once trimmed.
    #[must_use]
    pub fn with_detail(mut self, detail: &str) -> Self {
        let detail = detail.trim();
        self.detail = (!detail.is_empty()).then(|| detail.to_owned());
        self
    }

    /// `provider` answered the sign-in with `error` instead of a code. The
    /// error is named only when RFC 6749 defines it, because anyone can put
    /// any text in the callback's address.
    pub fn provider_error(provider: &str, error: &str) -> Self {
        let failure = Self::new(
            format!("{provider} didn't sign you in"),
            format!(
                "{provider} sent back an error instead of a sign-in code, so the sign-in has \
                 stopped."
            ),
        );
        if CALLBACK_ERRORS.contains(&error) {
            failure.with_detail(error)
        } else {
            failure
        }
    }

    /// The sign-in got its code from `provider` but couldn't make the
    /// credential, for `reason`, which must already be redacted.
    pub fn unfinished(provider: &str, reason: &str) -> Self {
        Self::new(
            "The sign-in didn't finish",
            format!(
                "open-ferry got a sign-in code from {provider} but couldn't finish signing in, \
                 so nothing was saved."
            ),
        )
        .with_detail(reason)
    }

    /// The credential was made but couldn't be saved, for `reason`, which
    /// must already be redacted.
    pub fn unsaved(reason: &str) -> Self {
        Self::new(
            "The credential couldn't be saved",
            "open-ferry signed in but couldn't save the credential.",
        )
        .with_detail(reason)
    }

    /// The sign-in stopped before it ended, without saying why.
    pub fn stopped() -> Self {
        Self::new(
            "The sign-in stopped",
            "It stopped before it finished, so nothing was saved.",
        )
    }
}

/// The page for `outcome` of a sign-in to `provider` (such as `Claude` or
/// `Codex`) started from `origin`, with its status and headers.
pub fn response(provider: &str, origin: Origin, outcome: &Outcome) -> Response {
    let status = match outcome {
        Outcome::Failed(_) => StatusCode::BAD_REQUEST,
        Outcome::SignedIn | Outcome::Finishing => StatusCode::OK,
    };
    let mut response = (status, html(provider, origin, outcome)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        CONTENT_SECURITY_POLICY.clone(),
    );
    response
}

/// The page's HTML.
pub fn html(provider: &str, origin: Origin, outcome: &Outcome) -> String {
    let (class, icon, title, meaning, detail) = match outcome {
        Outcome::SignedIn => (
            "ok",
            ICON_OK,
            Cow::Owned(format!("Signed in to {provider}")),
            Cow::Owned(format!("open-ferry has saved your {provider} credential.")),
            None,
        ),
        Outcome::Failed(failure) => (
            "failed",
            ICON_FAILED,
            Cow::Borrowed(failure.title.as_str()),
            Cow::Borrowed(failure.meaning.as_str()),
            failure.detail.as_deref(),
        ),
        Outcome::Finishing => (
            "finishing",
            ICON_FINISHING,
            Cow::Borrowed("The sign-in is still finishing"),
            Cow::Owned(format!(
                "open-ferry has the sign-in code from {provider} and is still finishing the \
                 sign-in."
            )),
            None,
        ),
    };
    let next = match (outcome, origin) {
        (Outcome::SignedIn | Outcome::Failed(_), Origin::Terminal) => {
            "You can close this tab and go back to the terminal."
        }
        (Outcome::SignedIn, Origin::Dashboard) => {
            "You can close this tab and go back to the dashboard."
        }
        (Outcome::Failed(_), Origin::Dashboard) => {
            "You can close this tab and start the sign-in again from the dashboard."
        }
        (Outcome::Finishing, Origin::Terminal) => {
            "You can close this tab and go back to the terminal, which shows the result."
        }
        (Outcome::Finishing, Origin::Dashboard) => {
            "You can close this tab and go back to the dashboard, which shows the result."
        }
    };
    let title = escape(&title);
    let mut page = String::with_capacity(8 * 1024);
    let _ = write!(
        page,
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"light dark\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <meta name=\"robots\" content=\"noindex\">\n\
         <title>{title} - open-ferry</title>\n\
         <link rel=\"icon\" type=\"image/svg+xml\" href=\"{icon_url}\">\n\
         <style>{STYLE}</style>\n\
         </head>\n\
         <body>\n\
         <main class=\"{class}\">\n\
         <p class=\"brand\"><span class=\"mark\" aria-hidden=\"true\">{MARK}</span>open-ferry</p>\n\
         <section class=\"card\">\n\
         <div class=\"badge\" aria-hidden=\"true\">{icon}</div>\n\
         <h1>{title}</h1>\n\
         <p class=\"meaning\">{meaning}</p>\n",
        icon_url = *MARK_URL,
        meaning = escape(&meaning),
    );
    if let Some(detail) = detail {
        let _ = writeln!(
            page,
            "<p class=\"detail\"><code>{}</code></p>",
            escape(&shorten(detail))
        );
    }
    let _ = write!(
        page,
        "<p class=\"next\">{next}</p>\n\
         </section>\n\
         </main>\n\
         </body>\n\
         </html>\n"
    );
    page
}

/// `text` cut to [`MAX_DETAIL_CHARS`] characters, with an ellipsis where it
/// was cut.
fn shorten(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(MAX_DETAIL_CHARS) {
        Some((end, _)) => Cow::Owned(format!("{}\u{2026}", text.get(..end).unwrap_or(text))),
        None => Cow::Borrowed(text),
    }
}

/// `text` safe to put in HTML text or a quoted attribute.
fn escape(text: &str) -> Cow<'_, str> {
    if !text.contains(['&', '<', '>', '"', '\'']) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// The dashboard's mark, `dashboard/public/favicon.svg`.
const MARK: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 32 32\"><rect \
    width=\"32\" height=\"32\" rx=\"7\" fill=\"#0969da\"/><path d=\"M6 19h20l-3.2 5.2a2 2 0 0 \
    1-1.7.8H10.9a2 2 0 0 1-1.7-.8z\" fill=\"#fff\"/><path d=\"M15 6v11H8.5z\" fill=\"#fff\"/>\
    <path d=\"M17 9l6.5 8H17z\" fill=\"#fff\" opacity=\".8\"/></svg>";

/// The mark as a `data:` URL, for the tab's icon.
static MARK_URL: LazyLock<String> =
    LazyLock::new(|| format!("data:image/svg+xml;base64,{}", STANDARD.encode(MARK)));

/// A tick, for a sign-in that succeeded.
const ICON_OK: &str = "<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" \
    stroke-width=\"2.5\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M20 6 9 \
    17l-5-5\"/></svg>";

/// A cross, for a sign-in that failed.
const ICON_FAILED: &str = "<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" \
    stroke-width=\"2.5\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M18 6 6 \
    18\"/><path d=\"m6 6 12 12\"/></svg>";

/// A clock, for a sign-in still finishing.
const ICON_FINISHING: &str = "<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" \
    stroke-width=\"2.5\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><circle cx=\"12\" \
    cy=\"12\" r=\"9\"/><path d=\"M12 7v5l3 2\"/></svg>";

/// The page's style: the dashboard's colour tokens (`dashboard/src/index.css`)
/// and the layout of its sign-in card.
const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --of-canvas: #f6f8fa;
  --of-surface: #ffffff;
  --of-raised: #eff2f5;
  --of-fg: #1f2328;
  --of-muted: #59636e;
  --of-line: #d1d9e0;
  --of-ok: #1a7f37;
  --of-ok-soft: #dafbe1;
  --of-warn: #9a6700;
  --of-warn-soft: #fff8c5;
  --of-danger: #d1242f;
  --of-danger-soft: #ffebe9;
}
@media (prefers-color-scheme: dark) {
  :root {
    --of-canvas: #0d1117;
    --of-surface: #151b23;
    --of-raised: #212830;
    --of-fg: #f0f6fc;
    --of-muted: #9198a1;
    --of-line: #3d444d;
    --of-ok: #3fb950;
    --of-ok-soft: #10261b;
    --of-warn: #d29922;
    --of-warn-soft: #2a2213;
    --of-danger: #f85149;
    --of-danger-soft: #2c1619;
  }
}
* { box-sizing: border-box; }
html {
  background: var(--of-canvas);
  color: var(--of-fg);
  font-family: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial,
    "Noto Sans", sans-serif;
  font-size: 14px;
  line-height: 1.5;
  -webkit-font-smoothing: antialiased;
}
body { margin: 0; }
main {
  display: flex;
  flex-direction: column;
  justify-content: center;
  gap: 20px;
  width: 100%;
  max-width: 28rem;
  min-height: 100vh;
  margin: 0 auto;
  padding: 40px 16px;
}
p { margin: 0; }
.brand {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  font-weight: 600;
}
.mark svg { display: block; width: 28px; height: 28px; }
.card {
  padding: 24px 20px;
  border: 1px solid var(--of-line);
  border-radius: 8px;
  background: var(--of-surface);
  box-shadow: 0 1px 3px 0 rgb(0 0 0 / 0.1), 0 1px 2px -1px rgb(0 0 0 / 0.1);
}
.badge {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 40px;
  height: 40px;
  margin-bottom: 12px;
  border-radius: 50%;
}
.badge svg { width: 22px; height: 22px; }
.ok .badge { background: var(--of-ok-soft); color: var(--of-ok); }
.failed .badge { background: var(--of-danger-soft); color: var(--of-danger); }
.finishing .badge { background: var(--of-warn-soft); color: var(--of-warn); }
h1 { margin: 0 0 4px; font-size: 18px; line-height: 28px; font-weight: 600; }
.meaning { color: var(--of-muted); }
.detail {
  margin-top: 16px;
  padding: 8px 12px;
  border: 1px solid var(--of-line);
  border-radius: 6px;
  background: var(--of-raised);
  font-size: 12px;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.detail code {
  font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono",
    monospace;
}
.next {
  margin-top: 16px;
  padding-top: 16px;
  border-top: 1px solid var(--of-line);
}
"#;

/// The answers' content security policy: nothing but the page's own style,
/// by its hash, and `data:` images.
static CONTENT_SECURITY_POLICY: LazyLock<HeaderValue> = LazyLock::new(|| {
    let hash = STANDARD.encode(Sha256::digest(STYLE.as_bytes()));
    let policy = format!(
        "default-src 'none'; style-src 'sha256-{hash}'; img-src data:; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'"
    );
    // The policy is ASCII; were it refused, the stricter policy without the
    // style would still keep everything else out.
    HeaderValue::from_str(&policy)
        .unwrap_or_else(|_| HeaderValue::from_static("default-src 'none'"))
});

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    /// The workspace's root.
    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// The `--of-*` tokens of the CSS `block`, by name.
    fn tokens(block: &str) -> BTreeMap<String, String> {
        block
            .lines()
            .filter_map(|line| line.trim().strip_prefix("--of-"))
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| {
                let value = value.trim().trim_end_matches(';').trim();
                (name.trim().to_owned(), value.to_owned())
            })
            .collect()
    }

    /// The light and dark tokens of `css`: those of its first `:root`
    /// block, and of the one in its dark-scheme media query.
    fn themes(css: &str) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        let (light, dark) = css
            .split_once("@media (prefers-color-scheme: dark)")
            .unwrap();
        let block = |css: &str| {
            let start = css.find(":root {").unwrap();
            let end = start + css[start..].find('}').unwrap();
            tokens(&css[start..end])
        };
        (block(light), block(dark))
    }

    fn failure() -> Failure {
        Failure::new("It <broke>", "Because \"of\" 'this' & that.")
            .with_detail("<script>alert(1)</script>")
    }

    fn header<'a>(response: &'a Response, name: &str) -> &'a str {
        response.headers()[name].to_str().unwrap()
    }

    // Not upstream's: the style's hash is the one the policy allows, and the
    // policy allows nothing else but `data:` images.
    #[test]
    fn the_policy_allows_the_style_by_its_hash() {
        let page = html("Codex", Origin::Terminal, &Outcome::SignedIn);
        let start = page.find("<style>").unwrap() + "<style>".len();
        let end = page.find("</style>").unwrap();
        let hash = STANDARD.encode(Sha256::digest(&page.as_bytes()[start..end]));
        let response = response("Codex", Origin::Terminal, &Outcome::SignedIn);
        assert_eq!(
            header(&response, "content-security-policy"),
            format!(
                "default-src 'none'; style-src 'sha256-{hash}'; img-src data:; \
                 base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
            )
        );
        assert!(!header(&response, "content-security-policy").contains("unsafe"));
        assert_eq!(page.matches("<style>").count(), 1);
        assert!(!page.contains(" style="), "{page}");
    }

    // Not upstream's: every answer has its headers, and only a failure
    // answers 400.
    #[test]
    fn answers_have_their_headers_and_status() {
        for (outcome, status) in [
            (Outcome::SignedIn, 200),
            (Outcome::Finishing, 200),
            (Outcome::Failed(failure()), 400),
        ] {
            for origin in [Origin::Terminal, Origin::Dashboard] {
                let response = response("Claude", origin, &outcome);
                assert_eq!(response.status().as_u16(), status, "{outcome:?}");
                for (name, value) in [
                    ("content-type", "text/html; charset=utf-8"),
                    ("cache-control", "no-store"),
                    ("referrer-policy", "no-referrer"),
                    ("x-content-type-options", "nosniff"),
                    ("x-frame-options", "DENY"),
                ] {
                    assert_eq!(header(&response, name), value, "{outcome:?}");
                }
            }
        }
    }

    // Not upstream's: the page loads nothing, runs nothing and promises
    // nothing about closing itself.
    #[test]
    fn pages_are_self_contained() {
        for outcome in [
            Outcome::SignedIn,
            Outcome::Finishing,
            Outcome::Failed(failure()),
        ] {
            for origin in [Origin::Terminal, Origin::Dashboard] {
                let page = html("Codex", origin, &outcome).to_ascii_lowercase();
                assert!(page.contains("you can close this tab"), "{page}");
                // The SVG namespace is a name, not a request.
                let page = page.replace("xmlns=\"http://www.w3.org/2000/svg\"", "");
                for refused in [
                    "<script",
                    "window.close",
                    "seconds",
                    "http:",
                    "https:",
                    "url(",
                    "@import",
                    "@font-face",
                    " src=",
                ] {
                    assert!(!page.contains(refused), "{refused}: {page}");
                }
            }
        }
    }

    // Not upstream's: what each page says, and where it sends the user.
    #[test]
    fn pages_say_how_the_sign_in_ended() {
        let page = html("Claude", Origin::Terminal, &Outcome::SignedIn);
        assert!(page.contains("<h1>Signed in to Claude</h1>"), "{page}");
        assert!(page.contains("<title>Signed in to Claude - open-ferry</title>"));
        assert!(page.contains("You can close this tab and go back to the terminal."));
        let page = html("Codex", Origin::Dashboard, &Outcome::SignedIn);
        assert!(page.contains("<h1>Signed in to Codex</h1>"), "{page}");
        assert!(page.contains("You can close this tab and go back to the dashboard."));

        let page = html("Codex", Origin::Terminal, &Outcome::Finishing);
        assert!(page.contains("<h1>The sign-in is still finishing</h1>"));
        assert!(page.contains("go back to the terminal, which shows the result."));
        let page = html("Codex", Origin::Dashboard, &Outcome::Finishing);
        assert!(page.contains("go back to the dashboard, which shows the result."));

        let failed = Outcome::Failed(Failure::unfinished("Codex", "token exchange failed"));
        let page = html("Codex", Origin::Dashboard, &failed);
        assert!(
            page.contains("<h1>The sign-in didn&#39;t finish</h1>"),
            "{page}"
        );
        assert!(
            page.contains("<code>token exchange failed</code>"),
            "{page}"
        );
        assert!(
            page.contains("You can close this tab and start the sign-in again from the dashboard.")
        );
        let page = html("Codex", Origin::Terminal, &failed);
        assert!(page.contains("You can close this tab and go back to the terminal."));
        assert!(!page.contains("dashboard"), "{page}");
    }

    // Not upstream's: every piece of text that wasn't written here is
    // escaped, and a long detail is cut short.
    #[test]
    fn dynamic_text_is_escaped() {
        let page = html("<b>", Origin::Dashboard, &Outcome::Failed(failure()));
        assert!(page.contains("<h1>It &lt;broke&gt;</h1>"), "{page}");
        assert!(page.contains("<title>It &lt;broke&gt; - open-ferry</title>"));
        assert!(page.contains("Because &quot;of&quot; &#39;this&#39; &amp; that."));
        assert!(page.contains("<code>&lt;script&gt;alert(1)&lt;/script&gt;</code>"));
        let page = html("<b>", Origin::Dashboard, &Outcome::SignedIn);
        assert!(page.contains("Signed in to &lt;b&gt;"), "{page}");
        assert!(!page.contains("<b>"), "{page}");

        let long = "\u{e9}".repeat(MAX_DETAIL_CHARS + 5);
        let failed = Outcome::Failed(Failure::new("t", "m").with_detail(&long));
        let page = html("Codex", Origin::Terminal, &failed);
        let shown = format!("{}\u{2026}", "\u{e9}".repeat(MAX_DETAIL_CHARS));
        assert!(page.contains(&format!("<code>{shown}</code>")), "{page}");
        assert_eq!(Failure::new("t", "m").with_detail("  ").detail, None);
    }

    // Not upstream's: a provider's error is named only when RFC 6749
    // defines it.
    #[test]
    fn provider_errors_are_named_when_standard() {
        let failure = Failure::provider_error("Codex", "access_denied");
        assert_eq!(failure.title, "Codex didn't sign you in");
        assert_eq!(failure.detail.as_deref(), Some("access_denied"));
        let failure = Failure::provider_error("Claude", "Call +1 555 0100 now");
        assert_eq!(failure.detail, None);
    }

    // Not upstream's: the page carries the dashboard's mark and colours.
    #[test]
    fn the_page_matches_the_dashboard() {
        let favicon = std::fs::read_to_string(root().join("dashboard/public/favicon.svg")).unwrap();
        assert_eq!(MARK, favicon.trim_end());
        let page = html("Codex", Origin::Terminal, &Outcome::SignedIn);
        assert!(page.contains(MARK));
        let url = format!(
            "data:image/svg+xml;base64,{}",
            STANDARD.encode(favicon.trim_end())
        );
        assert!(page.contains(&format!("href=\"{url}\"")), "{page}");

        let css = std::fs::read_to_string(root().join("dashboard/src/index.css")).unwrap();
        let (light, dark) = themes(&css);
        let (our_light, our_dark) = themes(STYLE);
        assert!(!our_light.is_empty());
        assert_eq!(
            our_light.keys().collect::<Vec<_>>(),
            our_dark.keys().collect::<Vec<_>>()
        );
        for (name, value) in &our_light {
            assert_eq!(light.get(name), Some(value), "light --of-{name}");
        }
        for (name, value) in &our_dark {
            assert_eq!(dark.get(name), Some(value), "dark --of-{name}");
        }
        // Every colour comes from a token.
        let rules = STYLE.split_once("* {").unwrap().1;
        assert!(!rules.contains('#'), "{rules}");
    }
}
