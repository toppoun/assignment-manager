use crate::{
    PocState, WebError,
    requests::{CompletionRequest, ListRequest},
};
use axum::{
    Json, Router,
    body::HttpBody as _,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use serde::Serialize;

/// Only a port can vary; there is no configurable public host/origin allowlist.
pub fn router(state: PocState, port: u16) -> Router {
    Router::new()
        .route("/api/v1/assignments", get(assignments))
        .route("/api/v1/assignments/{id}/completion", put(completion))
        .route("/health/live", get(live))
        .fallback(crate::assets::serve)
        .method_not_allowed_fallback(|| async { WebError::MethodNotAllowed })
        .layer(DefaultBodyLimit::max(1024))
        .layer(middleware::from_fn_with_state(
            (state.clone(), port),
            boundary,
        ))
        .with_state(state)
}

async fn completion(
    State(state): State<PocState>,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<CompletionRequest>, JsonRejection>,
) -> Result<Json<crate::dto::CompletionResponse>, WebError> {
    let Path(id) = path.map_err(|_| WebError::InvalidRequest)?;
    if id.trim().is_empty()
        || id.len() > 256
        || id.chars().any(char::is_control)
        || id.contains(['/', '\\'])
        || id == "."
        || id == ".."
    {
        return Err(WebError::InvalidRequest);
    }
    let Json(body) = body.map_err(|error| match error.status() {
        StatusCode::PAYLOAD_TOO_LARGE => WebError::PayloadTooLarge,
        StatusCode::UNSUPPORTED_MEDIA_TYPE => WebError::UnsupportedMediaType,
        _ => WebError::InvalidRequest,
    })?;
    Ok(Json(
        state.set_completion(id, body.completion.into()).await?,
    ))
}

async fn assignments(
    State(state): State<PocState>,
    request: Result<Query<ListRequest>, QueryRejection>,
) -> Result<Json<crate::dto::AssignmentListDto>, WebError> {
    let Query(request) = request.map_err(|_| WebError::InvalidQuery)?;
    Ok(Json(state.query(request.into_query()?).await?))
}
#[derive(Serialize)]
struct Health {
    status: &'static str,
}
async fn live() -> Json<Health> {
    Json(Health { status: "alive" })
}

fn check(request: &Request, port: u16, accepting: bool) -> Result<(), WebError> {
    let headers = request.headers();
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = hosts
        .next()
        .and_then(|h| h.to_str().ok())
        .ok_or(WebError::InvalidHost)?;
    if hosts.next().is_some()
        || (host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}"))
    {
        return Err(WebError::InvalidHost);
    }
    let mut origins = headers.get_all(header::ORIGIN).iter();
    if let Some(origin) = origins.next()
        && (origin.to_str().ok() != Some(format!("http://{host}").as_str())
            || origins.next().is_some())
    {
        return Err(WebError::CrossOrigin);
    }
    let sites: Vec<_> = headers.get_all("sec-fetch-site").iter().collect();
    if sites.len() > 1 || sites.first().is_some_and(|v| **v == "cross-site") {
        return Err(WebError::CrossOrigin);
    }
    if !accepting {
        return Err(WebError::Unavailable);
    }
    let mutation = request.method() == Method::PUT
        && request.uri().path().starts_with("/api/v1/assignments/")
        && request.uri().path().ends_with("/completion");
    if request.method() != Method::GET && !mutation {
        return Err(WebError::MethodNotAllowed);
    }
    if mutation {
        // A browser must prove same origin. Non-browser smoke clients must send
        // the same Origin/custom header; missing Fetch Metadata is allowed only
        // with that proof. Never admit a same-site sibling or a simple form POST.
        if !headers.contains_key(header::ORIGIN)
            || sites.first().is_some_and(|v| **v != "same-origin")
            || headers.get_all("x-assignment-manager").iter().count() != 1
            || headers
                .get("x-assignment-manager")
                .is_none_or(|v| v != "web-2")
        {
            return Err(WebError::CrossOrigin);
        }
        let modes: Vec<_> = headers.get_all("sec-fetch-mode").iter().collect();
        if modes.len() > 1
            || modes
                .first()
                .is_some_and(|v| **v != "same-origin" && **v != "cors")
        {
            return Err(WebError::CrossOrigin);
        }
        let types: Vec<_> = headers.get_all(header::CONTENT_TYPE).iter().collect();
        if types.len() != 1
            || types[0].to_str().ok().is_none_or(|v| {
                let mut parts = v.split(';').map(str::trim);
                !parts
                    .next()
                    .is_some_and(|v| v.eq_ignore_ascii_case("application/json"))
                    || parts.any(|v| !v.eq_ignore_ascii_case("charset=utf-8"))
            })
        {
            return Err(WebError::UnsupportedMediaType);
        }
        if headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|v| v > 1024)
        {
            return Err(WebError::PayloadTooLarge);
        }
        if request.uri().query().is_some() {
            return Err(WebError::InvalidQuery);
        }
    } else if !request.body().is_end_stream()
        || headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .is_some_and(|v| v != "0")
    {
        return Err(WebError::InvalidRequest);
    }
    if let Some(query) = request.uri().query()
        && (query.len() > 2048 || !valid_percent_encoding(query.as_bytes()))
    {
        return Err(WebError::InvalidQuery);
    }
    Ok(())
}

fn valid_percent_encoding(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1..=index + 2]
                    .iter()
                    .all(u8::is_ascii_hexdigit)
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

async fn boundary(
    State((state, port)): State<(PocState, u16)>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = match check(&request, port, state.accepting()) {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    ));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        header::HeaderValue::from_static("same-origin"),
    );
    response
}
