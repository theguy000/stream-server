use crate::state::AppState;
use axum::{
    Router,
    extract::{Path, Query},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use reqwest::{Client, Method};
use std::collections::HashMap;
use url::Url;

pub fn router() -> Router<AppState> {
    Router::new()
        // The original JS uses /proxy/:opts/:pathname*
        // We can use a wildcard capturing the whole path.
        .route("/{*rest}", any(proxy_handler))
}

pub async fn proxy_handler(
    Path(rest): Path<String>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    method: Method,
) -> impl IntoResponse {
    // Porting the logic from express_805.js
    // Format 1: ?d=URL (standard)
    // Format 2: /<query_params>/<path> (Core) where query_params contains d=ORIGIN&h=HEADER&r=RESPONSE_HEADER

    let mut target_url = String::new();
    let mut custom_headers = HashMap::new();
    let mut custom_response_headers = HashMap::new();
    let mut is_path_format = false;

    // Check for standard query param '?d='
    if let Some(d) = params.get("d") {
        target_url = d.clone();
        // Fallback: If rest is not empty and d is just origin, we might need to append rest?
        // But usually ?d=FULL_URL
    } else {
        is_path_format = true;
        // Handle path-based format: /proxy/d=...&h=.../path/to/file
        // Split rest by first slash to get query_segment and path
        let (query_seg, path_seg) = match rest.split_once('/') {
            Some((q, p)) => (q, p),
            None => (rest.as_str(), ""),
        };

        // Parse the query segment
        for (key, val) in url::form_urlencoded::parse(query_seg.as_bytes()) {
            match key.as_ref() {
                "d" => target_url = val.into_owned(),
                "h" => {
                    // Header format "Name:Value"
                    if let Some((name, value)) = val.split_once(':') {
                        custom_headers.insert(name.trim().to_string(), value.trim().to_string());
                    }
                }
                "r" => {
                    // Response header format "Name:Value"
                    if let Some((name, value)) = val.split_once(':') {
                        custom_response_headers
                            .insert(name.trim().to_string(), value.trim().to_string());
                    }
                }
                _ => {}
            }
        }

        // If we found 'd', construct the full URL
        if !target_url.is_empty() {
            // target_url is the origin (e.g. http://example.com)
            // path_seg is the relative path (e.g. video.mp4)
            // Join them carefully
            if !path_seg.is_empty() {
                if !target_url.ends_with('/') {
                    target_url.push('/');
                }
                target_url.push_str(path_seg);
            }
        } else {
            // Fallback: assume whole rest is the URL (legacy/simple proxy)
            target_url = rest.clone();
        }
    }

    let mut url = match Url::parse(&target_url) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid target URL").into_response(),
    };

    if is_path_format && let Some(q) = raw_query {
        url.set_query(Some(&q));
    }

    let client = Client::builder()
        .danger_accept_invalid_certs(true) // Parity with rejectUnauthorized: false
        .build()
        .unwrap();

    let mut req_builder = client.request(method, url.clone());

    // Forward standard headers
    let allowed_req_headers = [
        "accept",
        "accept-language",
        "connection",
        "transfer-encoding",
        "range",
        "if-range",
        "user-agent",
    ];

    for name in allowed_req_headers {
        if let Some(value) = headers.get(name) {
            req_builder = req_builder.header(name, value);
        }
    }

    // The response allow-list below drops `content-encoding`, so a compressed
    // body would reach the client as raw gzip it cannot decode. Ask for the
    // identity encoding instead of forwarding the client's `accept-encoding`.
    req_builder = req_builder.header("accept-encoding", "identity");

    // Apply custom headers from query params (Core format)
    for (name, value) in custom_headers {
        req_builder = req_builder.header(name, value);
    }

    let response = match req_builder.send().await {
        Ok(resp) => resp,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response(),
    };

    let status = response.status();
    let mut res_builder = Response::builder().status(status);

    let allowed_res_headers = [
        "accept-ranges",
        "content-type",
        "content-length",
        "content-range",
        "connection",
        "transfer-encoding",
        "last-modified",
        "etag",
        "server",
        "date",
    ];

    let res_headers = response.headers().clone();
    for name in allowed_res_headers {
        if let Some(value) = res_headers.get(name) {
            res_builder = res_builder.header(name, value);
        }
    }

    // Apply custom response headers (Core format)
    for (name, value) in custom_response_headers {
        res_builder = res_builder.header(name, value);
    }

    // CORS headers
    res_builder = res_builder
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "*");

    let content_type = res_headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let is_playlist = url.path().ends_with(".m3u8")
        || url.path().ends_with(".m3u")
        || content_type.contains("mpegurl");

    if is_playlist {
        // We need to rewrite the playlist.
        // For now, let's just stream it without rewriting as a first step,
        // then add rewriting if segments fail.
        let body = response.text().await.unwrap_or_default();
        let rewritten = rewrite_playlist(&body, &url);
        return res_builder
            .body(axum::body::Body::from(rewritten))
            .unwrap()
            .into_response();
    }

    let stream = response.bytes_stream();
    res_builder
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
        .into_response()
}

fn rewrite_playlist(body: &str, base_url: &Url) -> String {
    let mut rewritten = String::new();
    for line in body.lines() {
        if line.is_empty() {
            rewritten.push('\n');
            continue;
        }
        if line.starts_with("#") {
            // Handle URI="url" in tags like #EXT-X-MEDIA
            if let Some(start) = line.find("URI=\"") {
                let rest = &line[start + 5..];
                if let Some(end) = rest.find("\"") {
                    let uri = &rest[..end];
                    let absolute_uri = if uri.contains("://") {
                        uri.to_string()
                    } else {
                        base_url
                            .join(uri)
                            .map(|u: Url| u.to_string())
                            .unwrap_or_else(|_| uri.to_string())
                    };
                    let proxy_uri = format!("/proxy/?d={}", urlencoding::encode(&absolute_uri));
                    rewritten.push_str(&line[..start + 5]);
                    rewritten.push_str(&proxy_uri);
                    rewritten.push_str(&rest[end..]);
                    rewritten.push('\n');
                    continue;
                }
            }
            rewritten.push_str(line);
            rewritten.push('\n');
        } else {
            // It's a URL
            let absolute_uri = if line.contains("://") {
                line.to_string()
            } else {
                base_url
                    .join(line)
                    .map(|u: Url| u.to_string())
                    .unwrap_or_else(|_| line.to_string())
            };
            let proxy_uri = format!("/proxy/?d={}", urlencoding::encode(&absolute_uri));
            rewritten.push_str(&proxy_uri);
            rewritten.push('\n');
        }
    }
    rewritten
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    use tokio::net::TcpListener;

    const PAGE: &str = "<!doctype html><html>hello</html>";

    async fn upstream_page(headers: HeaderMap) -> Response {
        let gzip = headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("gzip"));
        if !gzip {
            return ([(header::CONTENT_TYPE, "text/html")], PAGE).into_response();
        }
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(PAGE.as_bytes()).unwrap();
        (
            [
                (header::CONTENT_TYPE, "text/html"),
                (header::CONTENT_ENCODING, "gzip"),
            ],
            encoder.finish().unwrap(),
        )
            .into_response()
    }

    async fn serve(app: Router) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    /// A client that accepts gzip must get a body it can read: either plain, or
    /// compressed with a matching `Content-Encoding` header.
    #[tokio::test]
    async fn body_is_readable_when_the_client_accepts_gzip() {
        let upstream = serve(Router::new().route("/page.html", get(upstream_page))).await;
        let proxy = serve(Router::new().nest(
            "/proxy",
            Router::new().route("/{*rest}", any(proxy_handler)),
        ))
        .await;
        let url = format!(
            "http://127.0.0.1:{proxy}/proxy/d=http%3A%2F%2F127.0.0.1%3A{upstream}/page.html"
        );

        let response = reqwest::Client::new()
            .get(url)
            .header("accept-encoding", "gzip, deflate, br")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let encoding = response.headers().get(header::CONTENT_ENCODING).cloned();
        let body = response.bytes().await.unwrap();

        assert!(
            encoding.is_some() || body.as_ref() == PAGE.as_bytes(),
            "compressed body without a Content-Encoding header ({} bytes, starts {:02x?})",
            body.len(),
            &body[..body.len().min(2)]
        );
    }
}
