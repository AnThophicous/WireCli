use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures_util::StreamExt;
use reqwest::{Client, Url};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub bind_addr: SocketAddr,
    pub upstream_url: String,
}

#[derive(Clone)]
struct GatewayState {
    client: Client,
    config: Arc<GatewayConfig>,
}

pub async fn serve(config: GatewayConfig) -> Result<(), String> {
    let state = GatewayState {
        client: Client::new(),
        config: Arc::new(config),
    };
    let bind_addr = state.config.bind_addr;
    let upstream_url = state.config.upstream_url.clone();

    let app = Router::new()
        .route("/health", axum::routing::get(proxy))
        .route("/v1/models", axum::routing::get(proxy))
        .route("/v1/responses", axum::routing::post(proxy))
        .fallback(reject_all)
        .with_state(state);

    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(|e| e.to_string())?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    println!("rift gateway listening on http://{bound}");
    println!("upstream: {}", upstream_url);
    println!("accepted routes: GET /health, GET /v1/models, POST /v1/responses");

    axum::serve(listener, app)
        .with_graceful_shutdown(wait_for_shutdown())
        .await
        .map_err(|e| e.to_string())
}

async fn proxy(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    match proxy_request(state, request).await {
        Ok(response) => response,
        Err(err) => response_with_text(StatusCode::BAD_GATEWAY, err),
    }
}

async fn reject_all() -> Response {
    response_with_text(
        StatusCode::FORBIDDEN,
        "only POST /v1/responses is supported".to_string(),
    )
}

async fn proxy_request(state: GatewayState, request: Request<Body>) -> Result<Response, String> {
    let (parts, body) = request.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();

    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| e.to_string())?;

    let upstream_uri = rewrite_uri(&state.config.upstream_url, &uri)?;
    let mut builder = state.client.request(reqwest_method(&method)?, upstream_uri);
    builder = builder.headers(filtered_headers(headers));
    if !body_bytes.is_empty() {
        builder = builder.body(body_bytes);
    }

    let response = builder.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let mut out = Response::builder().status(status);

    for (name, value) in response.headers() {
        if should_skip_response_header(name) {
            continue;
        }
        out = out.header(name, value);
    }

    let stream = response.bytes_stream().map(|item| item.map(Bytes::from));
    out.body(Body::from_stream(stream))
        .map_err(|e| e.to_string())
}

fn rewrite_uri(upstream_base: &str, uri: &Uri) -> Result<Url, String> {
    let path = uri.path();
    if uri.query().is_some() {
        return Err(format!("query strings are not supported on {path}"));
    }

    let mut url = Url::parse(upstream_base).map_err(|e| e.to_string())?;
    let new_path = match path {
        "/health" => "/health".to_string(),
        "/v1/models" => rewrite_v1_path(url.path(), "models"),
        "/v1/responses" => rewrite_v1_path(url.path(), "responses"),
        other => return Err(format!("unsupported path: {other}")),
    };
    url.set_path(&new_path);
    url.set_query(None);
    Ok(url)
}

fn rewrite_v1_path(base_path: &str, leaf: &str) -> String {
    let upstream_path = base_path.trim_end_matches('/');
    if upstream_path.is_empty() || upstream_path == "/" {
        format!("/v1/{leaf}")
    } else if upstream_path.ends_with("/v1") {
        format!("{upstream_path}/{leaf}")
    } else {
        format!("{upstream_path}/v1/{leaf}")
    }
}

fn filtered_headers(mut headers: HeaderMap) -> HeaderMap {
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::TRANSFER_ENCODING);
    headers.remove(header::CONNECTION);
    headers.remove(header::TE);
    headers.remove(header::TRAILER);
    headers.remove(header::UPGRADE);
    headers.remove(header::PROXY_AUTHORIZATION);
    headers
}

fn reqwest_method(method: &Method) -> Result<reqwest::Method, String> {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(|e| e.to_string())
}

fn should_skip_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "transfer-encoding"
            | "content-length"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn response_with_text(status: StatusCode, text: String) -> Response {
    (status, text).into_response()
}

async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::rewrite_uri;
    use axum::http::Uri;

    #[test]
    fn rewrites_responses_path() {
        let uri: Uri = "/v1/responses".parse().unwrap();
        let rewritten = rewrite_uri("http://127.0.0.1:3000/v1", &uri).unwrap();
        assert_eq!(rewritten.as_str(), "http://127.0.0.1:3000/v1/responses");
    }

    #[test]
    fn rejects_non_v1_paths() {
        let uri: Uri = "/v1/chat/responses".parse().unwrap();
        assert!(rewrite_uri("http://127.0.0.1:3000/v1", &uri).is_err());
    }

    #[test]
    fn rejects_query_strings() {
        let uri: Uri = "/v1/responses?stream=true".parse().unwrap();
        assert!(rewrite_uri("http://127.0.0.1:3000/v1", &uri).is_err());
    }

    #[test]
    fn rewrites_health_path() {
        let uri: Uri = "/health".parse().unwrap();
        let rewritten = rewrite_uri("http://127.0.0.1:3000/v1", &uri).unwrap();
        assert_eq!(rewritten.as_str(), "http://127.0.0.1:3000/health");
    }
}
