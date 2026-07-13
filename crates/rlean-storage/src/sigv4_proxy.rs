//! In-process SigV4 signing forward-proxy for the Iceberg REST catalog.
//!
//! `iceberg-catalog-rest` 0.9.1 can only attach static headers or an OAuth2
//! bearer token to catalog requests; it has no per-request signing hook, and
//! `reqwest` (the client it drives) exposes no request-level middleware. AWS
//! S3 Tables / Glue REST catalogs require every request to be individually
//! signed with SigV4 (the signature covers the method, canonical path, query,
//! host and the exact request body). A single pre-built client therefore cannot
//! satisfy the catalog.
//!
//! This module runs a tiny localhost HTTP/1 server. The store points the REST
//! catalog `uri` at `http://127.0.0.1:<port>/<path-prefix>`; every catalog call
//! then lands here, where the request is rewritten to the real catalog endpoint,
//! signed with SigV4, forwarded, and the response streamed back verbatim.
//!
//! Credentials are resolved from a [`SharedCredentialsProvider`] on every
//! request (the provider caches, so this is cheap) so that temporary SSO/STS
//! credentials are refreshed rather than pinned at startup. Credential material
//! is never logged.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sigv4::http_request::{
    sign, SignableBody, SignableRequest, SigningParams, SigningSettings,
};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// A running SigV4 signing proxy. Holds the background accept-loop task and
/// exposes the local base URI the REST catalog should target. Dropping it
/// aborts the task and closes the listener, so the proxy lives exactly as long
/// as the [`crate::iceberg_store::IcebergStore`] that owns it.
pub(crate) struct SigV4Proxy {
    local_uri: String,
    handle: JoinHandle<()>,
}

impl Drop for SigV4Proxy {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl SigV4Proxy {
    /// The `http://127.0.0.1:<port><path-prefix>` base URI to hand to the REST
    /// catalog. All requests to this URI are signed and forwarded to the real
    /// catalog endpoint.
    pub(crate) fn local_uri(&self) -> &str {
        &self.local_uri
    }

    /// Start the proxy using the ambient AWS credential chain.
    ///
    /// `catalog_uri` is the real REST catalog base (e.g.
    /// `https://s3tables.us-west-2.amazonaws.com/iceberg`). `region` and
    /// `signing_name` are the SigV4 signing region and service.
    pub(crate) async fn start(
        catalog_uri: &str,
        region: &str,
        signing_name: &str,
        credentials: SharedCredentialsProvider,
    ) -> Result<Self> {
        let target = TargetEndpoint::parse(catalog_uri)?;
        let config = Arc::new(ProxyConfig {
            target,
            region: region.to_string(),
            signing_name: signing_name.to_string(),
            credentials,
        });

        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .context("failed to bind SigV4 proxy listener")?;
        let port = listener
            .local_addr()
            .context("failed to read SigV4 proxy local address")?
            .port();
        let local_uri = format!("http://127.0.0.1:{port}{}", config.target.path_prefix);

        let handle = tokio::spawn(accept_loop(listener, config));
        Ok(Self { local_uri, handle })
    }
}

/// The real catalog endpoint split into the parts the proxy needs to rebuild
/// and sign each forwarded request.
#[derive(Clone)]
struct TargetEndpoint {
    /// Scheme + authority, e.g. `https://s3tables.us-west-2.amazonaws.com`.
    origin: String,
    /// Host header value, e.g. `s3tables.us-west-2.amazonaws.com`.
    host: String,
    /// Path prefix of the catalog base, e.g. `/iceberg` (never a trailing `/`).
    path_prefix: String,
}

impl TargetEndpoint {
    fn parse(catalog_uri: &str) -> Result<Self> {
        let url = url::Url::parse(catalog_uri)
            .with_context(|| format!("invalid catalog uri '{catalog_uri}'"))?;
        let scheme = url.scheme();
        if scheme != "https" && scheme != "http" {
            return Err(anyhow!(
                "catalog uri '{catalog_uri}' must be http(s), got scheme '{scheme}'"
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("catalog uri '{catalog_uri}' has no host"))?
            .to_string();
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        let origin = format!("{scheme}://{authority}");
        // Strip a trailing slash so joining with the incoming path never
        // produces a double slash.
        let path_prefix = url.path().trim_end_matches('/').to_string();
        Ok(Self {
            origin,
            host: authority,
            path_prefix,
        })
    }
}

struct ProxyConfig {
    target: TargetEndpoint,
    region: String,
    signing_name: String,
    credentials: SharedCredentialsProvider,
}

async fn accept_loop(listener: TcpListener, config: Arc<ProxyConfig>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!("SigV4 proxy accept failed: {error}");
                continue;
            }
        };
        let config = config.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let config = config.clone();
                async move { Ok::<_, Infallible>(handle_request(req, config).await) }
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::debug!("SigV4 proxy connection closed: {error}");
            }
        });
    }
}

async fn handle_request(req: Request<Incoming>, config: Arc<ProxyConfig>) -> Response<Full<Bytes>> {
    match forward_signed(req, &config).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!("SigV4 proxy request failed: {error:#}");
            let mut response = Response::new(Full::new(Bytes::from(format!(
                "sigv4 proxy error: {error}"
            ))));
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            response
        }
    }
}

async fn forward_signed(
    req: Request<Incoming>,
    config: &ProxyConfig,
) -> Result<Response<Full<Bytes>>> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    // The incoming request-target path already includes the catalog path prefix
    // (the REST client builds it by appending to the base uri we handed it), so
    // forward the path-and-query verbatim to the real origin.
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let target_url = format!("{}{}", config.target.origin, path_and_query);

    // Buffer the body: SigV4 for S3 Tables signs the payload hash, so we must
    // have the full bytes before signing and forwarding.
    let body_bytes = body
        .collect()
        .await
        .context("failed to read proxied request body")?
        .to_bytes();

    let credentials = config
        .credentials
        .provide_credentials()
        .await
        .context("failed to resolve AWS credentials for SigV4 signing")?;
    let identity: Identity = credentials.into();

    let signing_settings = SigningSettings::default();
    let signing_params: SigningParams = v4::SigningParams::builder()
        .identity(&identity)
        .region(&config.region)
        .name(&config.signing_name)
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()
        .context("failed to build SigV4 signing params")?
        .into();

    // Sign over the request as the real origin will see it: the true host
    // header plus every forwarded header. The signer emits an `Authorization`
    // header (and `x-amz-security-token` when the identity carries a session
    // token) which we then apply to the outgoing request.
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    header_pairs.push(("host".to_string(), config.target.host.clone()));
    for (name, value) in parts.headers.iter() {
        let name = name.as_str();
        // The host is replaced with the real origin's; hop-by-hop and length
        // headers are recomputed by the forwarding client.
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            header_pairs.push((name.to_string(), value.to_string()));
        }
    }

    let signable_headers = header_pairs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()));
    let signable = SignableRequest::new(
        method.as_str(),
        &target_url,
        signable_headers,
        SignableBody::Bytes(&body_bytes),
    )
    .context("failed to build signable request")?;

    let (instructions, _signature) = sign(signable, &signing_params)
        .context("SigV4 signing failed")?
        .into_parts();

    // Rebuild the outgoing request with the forwarded headers, then let the
    // signer stamp the `Authorization` / `x-amz-*` headers onto it.
    let mut outgoing = http::Request::builder()
        .method(method.clone())
        .uri(&target_url);
    for (name, value) in &header_pairs {
        outgoing = outgoing.header(name, value);
    }
    let mut outgoing = outgoing
        .body(())
        .context("failed to build outgoing request skeleton")?;
    instructions.apply_to_request_http1x(&mut outgoing);

    let client = reqwest::Client::new();
    let mut builder = client.request(method, &target_url);
    for (name, value) in outgoing.headers().iter() {
        builder = builder.header(name, value);
    }
    let response = builder
        .body(body_bytes.to_vec())
        .send()
        .await
        .with_context(|| format!("failed to forward signed request to {target_url}"))?;

    let status = response.status();
    let mut response_headers = response.headers().clone();
    let response_body = response
        .bytes()
        .await
        .context("failed to read forwarded response body")?;

    // Strip transfer-framing headers: we re-emit the body as a single fixed
    // buffer, so a carried-over chunked/length header would corrupt the reply.
    response_headers.remove(http::header::TRANSFER_ENCODING);
    response_headers.remove(http::header::CONTENT_LENGTH);

    let mut out = Response::new(Full::new(response_body));
    *out.status_mut() = status;
    *out.headers_mut() = response_headers;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::Credentials;
    use std::sync::Mutex;

    /// Start a tiny upstream HTTP server that records the first request it sees
    /// (method, path, headers, body) and always replies `200 OK "upstream-ok"`.
    /// Returns its base URI and a handle to the captured request.
    async fn spawn_echo_upstream() -> (String, Arc<Mutex<Option<CapturedRequest>>>, JoinHandle<()>)
    {
        let captured: Arc<Mutex<Option<CapturedRequest>>> = Arc::new(Mutex::new(None));
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let base = format!("http://{}/catalog", listener.local_addr().unwrap());
        let captured_for_task = captured.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let captured = captured_for_task.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req: Request<Incoming>| {
                        let captured = captured.clone();
                        async move {
                            let (parts, body) = req.into_parts();
                            let body = body.collect().await.unwrap().to_bytes();
                            let headers = parts
                                .headers
                                .iter()
                                .map(|(name, value)| {
                                    (
                                        name.as_str().to_string(),
                                        value.to_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect();
                            *captured.lock().unwrap() = Some(CapturedRequest {
                                method: parts.method.to_string(),
                                path: parts
                                    .uri
                                    .path_and_query()
                                    .map(|pq| pq.as_str().to_string())
                                    .unwrap_or_default(),
                                headers,
                                body: body.to_vec(),
                            });
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(
                                "upstream-ok",
                            ))))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        (base, captured, handle)
    }

    struct CapturedRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// The proxy signs and forwards a request to a local mock upstream without
    /// touching AWS: static test credentials are injected via
    /// [`SharedCredentialsProvider`]. Asserts the upstream sees the SigV4
    /// `Authorization` header, the session-token header, the real host, the
    /// prefixed path, and the intact body.
    #[tokio::test]
    async fn signs_and_forwards_with_static_credentials() {
        let (upstream_uri, captured, _upstream) = spawn_echo_upstream().await;

        let creds = Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("session-token-xyz".to_string()),
            None,
            "static-test-credentials",
        );
        let provider = SharedCredentialsProvider::new(creds);

        let proxy = SigV4Proxy::start(&upstream_uri, "us-west-2", "s3tables", provider)
            .await
            .unwrap();

        let body = br#"{"namespace":"lean"}"#;
        let response = reqwest::Client::new()
            .post(format!("{}/v1/namespaces", proxy.local_uri()))
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "upstream-ok");

        let captured = captured.lock().unwrap();
        let captured = captured.as_ref().expect("upstream saw a request");
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.path, "/catalog/v1/namespaces");
        assert_eq!(captured.body, body);

        let auth = captured
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str())
            .expect("Authorization header present");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 "),
            "expected SigV4 Authorization, got '{auth}'"
        );
        assert!(
            auth.contains("us-west-2/s3tables/aws4_request"),
            "signing scope must carry region+service, got '{auth}'"
        );

        let host = captured
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.as_str())
            .expect("host header present");
        assert!(
            !host.starts_with("127.0.0.1") || upstream_uri.contains(host),
            "host header must be the real upstream host, got '{host}'"
        );

        assert!(
            captured.headers.iter().any(|(name, value)| name
                .eq_ignore_ascii_case("x-amz-security-token")
                && value == "session-token-xyz"),
            "session token must be forwarded as x-amz-security-token"
        );
    }
}
