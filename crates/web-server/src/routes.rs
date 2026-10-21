use crate::{PocState, WebError, requests::ListRequest};
use axum::{
    Json, Router,
    body::HttpBody as _,
    extract::{Query, Request, State, rejection::QueryRejection},
    http::{Method, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;

/// Only a port can vary; there is no configurable public host/origin allowlist.
pub fn router(state: PocState, port: u16) -> Router {
    Router::new()
        .route("/api/v1/assignments", get(assignments))
        .route("/health/live", get(live))
        .fallback(|| async { WebError::NotFound })
        .layer(middleware::from_fn_with_state(
            (state.clone(), port),
            boundary,
        ))
        .with_state(state)
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
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value == "cross-site")
    {
        return Err(WebError::CrossOrigin);
    }
    if !accepting {
        return Err(WebError::Unavailable);
    }
    if request.method() != Method::GET {
        return Err(WebError::MethodNotAllowed);
    }
    // There is no JSON/body API in Web-1. Do not parse or echo unexpected input.
    if !request.body().is_end_stream()
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
    response
}
