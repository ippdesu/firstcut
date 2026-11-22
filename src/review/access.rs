//! Review URLs carry a launch key in their fragment so that it is not sent
//! with an ordinary page request. The exchanged bearer token is stored by
//! browser origin; unlike cookies this separates different localhost ports.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use subtle::ConstantTimeEq;

pub(super) const HEALTH_MARKER: &str = "firstcut-review";
const MAX_FAILED_SIGN_INS: usize = 5;
const SIGN_IN_WINDOW: Duration = Duration::from_secs(60);

pub(super) struct Access {
    authority: String,
    origin: String,
    key: String,
    session: String,
    managed: bool,
    alias_authority: Option<String>,
    alias_origin: Option<String>,
    failed_sign_ins: Mutex<VecDeque<Instant>>,
}

impl Access {
    pub fn new(
        bind: IpAddr,
        port: u16,
        public_url: Option<&str>,
        configured_key: Option<&str>,
    ) -> Result<Self> {
        anyhow::ensure!(
            bind.is_loopback() || configured_key.is_some(),
            "非回环监听必须设置 FIRSTCUT_ACCESS_TOKEN，并使用 HTTPS 代理或只向宿主机本机发布端口"
        );
        let default_host = match bind {
            IpAddr::V6(address) if address.is_loopback() => "[::1]",
            _ => "127.0.0.1",
        };
        let default_url = if port == 80 {
            format!("http://{default_host}")
        } else {
            format!("http://{default_host}:{port}")
        };
        let url: axum::http::Uri = public_url
            .unwrap_or(&default_url)
            .parse()
            .context("无效的复核服务外部 URL")?;
        let scheme = url
            .scheme_str()
            .context("复核 URL 必须以 http:// 或 https:// 开头")?;
        let authority = url.authority().context("复核 URL 缺少主机名")?;
        anyhow::ensure!(
            (url.path().is_empty() || url.path() == "/")
                && url.query().is_none()
                && !authority.as_str().contains('@')
                && matches!(scheme, "http" | "https"),
            "复核 URL 必须只包含协议、主机名和端口"
        );
        let is_local = ["127.0.0.1", "localhost", "[::1]"]
            .iter()
            .any(|host| authority.host().eq_ignore_ascii_case(host));
        anyhow::ensure!(
            is_local || scheme == "https",
            "非本机的复核服务 URL 必须使用 HTTPS"
        );
        let mut authority = authority.as_str().to_ascii_lowercase();
        let implicit_port = if scheme == "https" { ":443" } else { ":80" };
        if authority.ends_with(implicit_port) {
            authority.truncate(authority.len() - implicit_port.len());
        }
        let key = match configured_key {
            Some(key) => {
                anyhow::ensure!(
                    (32..=128).contains(&key.len())
                        && key
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                    "FIRSTCUT_ACCESS_TOKEN 需要 32–128 个随机字符，只允许字母、数字、- 和 _"
                );
                key.into()
            }
            None => random_key()?,
        };
        let alias_authority = if public_url.is_none() && default_host == "127.0.0.1" {
            Some(if port == 80 {
                "localhost".into()
            } else {
                format!("localhost:{port}")
            })
        } else {
            None
        };
        let alias_origin = alias_authority
            .as_ref()
            .map(|host| format!("{scheme}://{host}"));
        Ok(Self {
            origin: format!("{scheme}://{authority}"),
            authority,
            key,
            session: random_key()?,
            managed: configured_key.is_some(),
            alias_authority,
            alias_origin,
            failed_sign_ins: Mutex::new(VecDeque::new()),
        })
    }

    pub fn url(&self) -> String {
        format!("{}/#access={}", self.origin, self.key)
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn is_managed(&self) -> bool {
        self.managed
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        one_header(headers, header::AUTHORIZATION)
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|value| equal_secret(value, &self.session))
    }

    fn permits(&self, request: &Request) -> bool {
        let headers = request.headers();
        let Some(host) = one_header(headers, header::HOST) else {
            return false;
        };
        let origin = if host.eq_ignore_ascii_case(&self.authority) {
            self.origin.as_str()
        } else if self
            .alias_authority
            .as_deref()
            .is_some_and(|alias| host.eq_ignore_ascii_case(alias))
        {
            self.alias_origin.as_deref().unwrap_or_default()
        } else {
            return false;
        };
        // Same-site may still be another port or service.
        if let Some(site) = headers.get("sec-fetch-site") {
            // Public top-level navigation (for example clicking the README
            // link) can still load the sign-in page. Framing is denied by CSP.
            let public_navigation = request.uri().path() == "/"
                && request.method() == Method::GET
                && headers
                    .get("sec-fetch-mode")
                    .is_some_and(|mode| mode == "navigate")
                && headers
                    .get("sec-fetch-dest")
                    .is_some_and(|dest| dest == "document");
            if !public_navigation && !matches!(site.to_str(), Ok("same-origin" | "none")) {
                return false;
            }
        }
        let method = request.method();
        if !matches!(*method, Method::GET | Method::HEAD)
            && (method != Method::POST
                || one_header(headers, header::ORIGIN) != Some(origin)
                || !one_header(headers, header::CONTENT_TYPE)
                    .and_then(|v| v.split(';').next())
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json")))
        {
            return false;
        }
        if headers.contains_key(header::ORIGIN)
            && one_header(headers, header::ORIGIN) != Some(origin)
        {
            return false;
        }
        true
    }
}

fn random_key() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| anyhow::anyhow!("无法生成复核访问凭据: {err}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn equal_secret(presented: &str, expected: &str) -> bool {
    bool::from(presented.as_bytes().ct_eq(expected.as_bytes()))
}

fn one_header(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    let all = headers.get_all(name);
    let mut values = all.iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

pub(super) async fn guard(
    State(access): State<Arc<Access>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let public = match *request.method() {
        Method::GET | Method::HEAD => matches!(path, "/" | "/healthz" | "/review.js"),
        Method::POST => path == "/api/session",
        _ => false,
    };
    let mut response = if !access.permits(&request) {
        (StatusCode::FORBIDDEN, "请求来源与复核服务地址不匹配").into_response()
    } else if !public && !access.authorized(request.headers()) {
        (
            StatusCode::UNAUTHORIZED,
            "请使用启动器或终端给出的复核链接登录",
        )
            .into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; img-src blob:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"
    ));
    response
}

#[derive(Deserialize)]
pub(super) struct SignIn {
    key: String,
}

pub(super) async fn sign_in(
    State(access): State<Arc<Access>>,
    Json(login): Json<SignIn>,
) -> Response {
    let now = Instant::now();
    {
        let mut failures = access
            .failed_sign_ins
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while failures
            .front()
            .is_some_and(|t| now.duration_since(*t) >= SIGN_IN_WINDOW)
        {
            failures.pop_front();
        }
        if failures.len() >= MAX_FAILED_SIGN_INS {
            let retry_after = SIGN_IN_WINDOW
                .saturating_sub(now.duration_since(failures[0]))
                .as_secs()
                .max(1);
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry_after.to_string())],
                "登录失败次数过多，请稍后再试",
            )
                .into_response();
        }
        if !equal_secret(&login.key, &access.key) {
            failures.push_back(now);
            return (
                StatusCode::UNAUTHORIZED,
                "复核访问凭据无效，请使用当前服务的链接",
            )
                .into_response();
        }
        failures.clear();
    }
    Json(serde_json::json!({"session": &access.session})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_listener_requires_an_explicit_key() {
        let any = "0.0.0.0".parse().unwrap();
        assert!(Access::new(any, 8787, None, None).is_err());
        let key = random_key().unwrap();
        assert!(Access::new(any, 8787, Some("http://photos.example.test"), Some(&key)).is_err());
        assert!(Access::new(any, 8787, Some("https://photos.example.test"), Some(&key)).is_ok());
    }

    #[tokio::test]
    async fn sign_in_throttles_repeated_failures() {
        let key = random_key().unwrap();
        let access = Arc::new(
            Access::new("127.0.0.1".parse().unwrap(), 8787, None, Some(&key)).unwrap(),
        );
        for _ in 0..MAX_FAILED_SIGN_INS {
            let resp = sign_in(
                State(Arc::clone(&access)),
                Json(SignIn {
                    key: "wrong-key".into(),
                }),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        let limited = sign_in(
            State(Arc::clone(&access)),
            Json(SignIn { key: key.clone() }),
        )
        .await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers().contains_key(header::RETRY_AFTER));
    }
}
