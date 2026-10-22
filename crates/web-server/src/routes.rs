use crate::{
    PocState, WebError,
    auth::{self, AuthenticatedSession},
    requests::{CompletionRequest, ListRequest},
};
use axum::{
    Json, Router,
    body::HttpBody as _,
    extract::{
        DefaultBodyLimit, Extension, Path, Query, Request, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Serialize;

/// Only a port can vary; there is no configurable public host/origin allowlist.
pub fn router(state: PocState, port: u16) -> Router {
    Router::new()
        .route(
            "/api/v1/auth/login",
            post(login).layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/v1/auth/logout",
            post(logout).layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/v1/auth/reauth",
            post(reauth).layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/v1/sync",
            post(start_sync).layer(DefaultBodyLimit::max(1024)),
        )
        .route("/api/v1/sync/status", get(sync_status))
        .route("/api/v1/sync/runs/{runId}", get(sync_run))
        .route("/api/v1/settings/status", get(settings_status))
        .route(
            "/api/v1/settings/moodle",
            put(save_moodle).delete(remove_moodle),
        )
        .route(
            "/api/v1/settings/webclass",
            put(save_webclass).delete(remove_webclass),
        )
        .route("/api/v1/session", get(session))
        .route("/api/v1/assignments", get(assignments))
        .route(
            "/api/v1/assignments/{id}/completion",
            put(completion).layer(DefaultBodyLimit::max(1024)),
        )
        .route("/health/live", get(live))
        .fallback(crate::assets::serve)
        .method_not_allowed_fallback(|| async { WebError::MethodNotAllowed })
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn_with_state(
            (state.clone(), port),
            boundary,
        ))
        .with_state(state)
}

async fn completion(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
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
        state
            .set_completion(&session.user_id, id, body.completion.into())
            .await?,
    ))
}

async fn assignments(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    request: Result<Query<ListRequest>, QueryRejection>,
) -> Result<Json<crate::dto::AssignmentListDto>, WebError> {
    let Query(request) = request.map_err(|_| WebError::InvalidQuery)?;
    Ok(Json(
        state.query(&session.user_id, request.into_query()?).await?,
    ))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    login: String,
    password: SecretInput,
}
fn json_error(error: JsonRejection) -> WebError {
    match error.status() {
        StatusCode::PAYLOAD_TOO_LARGE => WebError::PayloadTooLarge,
        StatusCode::UNSUPPORTED_MEDIA_TYPE => WebError::UnsupportedMediaType,
        _ => WebError::InvalidRequest,
    }
}
async fn login(
    State(state): State<PocState>,
    headers: header::HeaderMap,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, WebError> {
    let Json(input) = body.map_err(json_error)?;
    let (token, session) = state
        .auth()
        .login(input.login, input.password.0, auth::read_cookie(&headers)?)
        .await?;
    let mut response = Json(session).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth::cookie(&token)
            .parse()
            .map_err(|_| WebError::Internal)?,
    );
    Ok(response)
}
async fn session(
    Extension(session): Extension<AuthenticatedSession>,
) -> Json<AuthenticatedSession> {
    Json(session)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyBody {}
async fn logout(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    body: Result<Json<EmptyBody>, JsonRejection>,
) -> Result<Response, WebError> {
    let _ = body.map_err(json_error)?;
    state.auth().logout(session).await?;
    let mut response = Json(serde_json::json!({"authenticated": false})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth::clear_cookie()
            .parse()
            .map_err(|_| WebError::Internal)?,
    );
    Ok(response)
}
// Secret inputs have no Debug/Serialize and are zeroized even when deserialization
// subsequently rejects an unknown/duplicate field. Raw JSON/HTTP buffers are not
// covered by this ownership guarantee.
struct SecretInput(zeroize::Zeroizing<String>);
impl<'de> serde::Deserialize<'de> for SecretInput {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        <String as serde::Deserialize>::deserialize(d).map(|s| Self(s.into()))
    }
}
impl SecretInput {
    fn into_secret(mut self) -> assignment_manager_application::credentials::SecretText {
        assignment_manager_application::credentials::SecretText::new(std::mem::take(&mut *self.0))
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReauthRequest {
    password: SecretInput,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MoodleRequest {
    calendar_url: SecretInput,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebclassRequest {
    university_id: SecretInput,
    password: SecretInput,
}
async fn reauth(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    body: Result<Json<ReauthRequest>, JsonRejection>,
) -> Result<StatusCode, WebError> {
    let Json(input) = body.map_err(json_error)?;
    state
        .auth()
        .reauthenticate(session, input.password.0)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn settings_status(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    request: Request,
) -> Result<Json<crate::dto::SettingsStatusDto>, WebError> {
    if request.uri().query().is_some() {
        return Err(WebError::InvalidQuery);
    }
    Ok(Json(state.settings_status(&session.user_id).await?))
}
async fn save_moodle(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    body: Result<Json<MoodleRequest>, JsonRejection>,
) -> Result<StatusCode, WebError> {
    let Json(input) = body.map_err(json_error)?;
    if input.calendar_url.0.len() > crate::vault::URL_LIMIT {
        return Err(WebError::InvalidRequest);
    }
    state
        .save_moodle(&session.user_id, input.calendar_url.into_secret())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn save_webclass(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    body: Result<Json<WebclassRequest>, JsonRejection>,
) -> Result<StatusCode, WebError> {
    let Json(input) = body.map_err(json_error)?;
    if input.university_id.0.len() > crate::vault::ID_LIMIT
        || input.password.0.len() > crate::vault::PASSWORD_LIMIT
    {
        return Err(WebError::InvalidRequest);
    }
    use assignment_manager_application::credentials::WebClassCredentials;
    state
        .save_webclass(
            &session.user_id,
            WebClassCredentials {
                university_id: input.university_id.into_secret(),
                password: input.password.into_secret(),
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn remove_moodle(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    body: Result<Json<EmptyBody>, JsonRejection>,
) -> Result<StatusCode, WebError> {
    let _ = body.map_err(json_error)?;
    state.remove_moodle(&session.user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn remove_webclass(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    body: Result<Json<EmptyBody>, JsonRejection>,
) -> Result<StatusCode, WebError> {
    let _ = body.map_err(json_error)?;
    state.remove_webclass(&session.user_id).await?;
    Ok(StatusCode::NO_CONTENT)
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
    if request.uri().path().starts_with("/api/")
        && sites.first().is_some_and(|v| {
            **v != "same-origin" && !(request.method() == Method::GET && **v == "none")
        })
    {
        return Err(WebError::CrossOrigin);
    }
    if !accepting {
        return Err(WebError::Unavailable);
    }
    let mutation = (request.method() == Method::POST
        && matches!(
            request.uri().path(),
            "/api/v1/auth/login" | "/api/v1/auth/logout" | "/api/v1/auth/reauth" | "/api/v1/sync"
        ))
        || (request.method() == Method::PUT
            && request.uri().path().starts_with("/api/v1/assignments/")
            && request.uri().path().ends_with("/completion"));
    let settings_mutation = matches!(request.method(), &Method::PUT | &Method::DELETE)
        && matches!(
            request.uri().path(),
            "/api/v1/settings/moodle" | "/api/v1/settings/webclass"
        );
    let mutation = mutation || settings_mutation;
    let body_limit = if request.uri().path().starts_with("/api/v1/settings/") {
        4096
    } else {
        1024
    };
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
                .is_none_or(|v| v != "web-3a")
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
            .is_some_and(|v| v > body_limit)
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
    mut request: Request,
    next: Next,
) -> Response {
    let response = match check(&request, port, state.accepting()) {
        Ok(()) => {
            let path = request.uri().path();
            let protected = path == "/api/v1/session"
                || path == "/api/v1/auth/logout"
                || path == "/api/v1/auth/reauth"
                || path == "/api/v1/sync"
                || path.starts_with("/api/v1/sync/")
                || path.starts_with("/api/v1/settings/")
                || path == "/api/v1/assignments"
                || (path.starts_with("/api/v1/assignments/") && path.ends_with("/completion"));
            let authorized = if protected {
                match auth::read_cookie(request.headers()) {
                    Ok(token) => state.auth().resolve(token).await,
                    Err(error) => Err(error),
                }
                .and_then(|session| {
                    state.check_owner(&session.user_id)?;
                    if request.method() != Method::GET {
                        let values: Vec<_> =
                            request.headers().get_all("x-csrf-token").iter().collect();
                        if values.len() != 1 {
                            return Err(WebError::InvalidCsrf);
                        }
                        session
                            .check_csrf(values[0].to_str().map_err(|_| WebError::InvalidCsrf)?)?;
                    }
                    Ok(session)
                })
            } else {
                // No protected extension is needed for static/login routes.
                return finish(next.run(request).await);
            };
            match authorized {
                Ok(session) => {
                    if request.uri().path().starts_with("/api/v1/settings/")
                        && request.method() != Method::GET
                        && let Err(error) = state.auth().require_recent(session.clone()).await
                    {
                        return finish(error.into_response());
                    }
                    request.extensions_mut().insert(session);
                    next.run(request).await
                }
                Err(error) => error.into_response(),
            }
        }
        Err(error) => error.into_response(),
    };
    finish(response)
}
fn finish(mut response: Response) -> Response {
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

async fn start_sync(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    headers: header::HeaderMap,
    body: Result<Json<EmptyBody>, JsonRejection>,
) -> Result<(StatusCode, Json<crate::web_sync::AcceptedDto>), WebError> {
    let _ = body.map_err(json_error)?;
    let values: Vec<_> = headers.get_all("idempotency-key").iter().collect();
    if values.len() != 1 {
        return Err(WebError::InvalidRequest);
    }
    let key = values[0].to_str().map_err(|_| WebError::InvalidRequest)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(state.sync().start(&session.user_id, key)?),
    ))
}
async fn sync_status(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    request: Request,
) -> Result<Json<crate::web_sync::StatusDto>, WebError> {
    if request.uri().query().is_some() {
        return Err(WebError::InvalidQuery);
    }
    Ok(Json(state.sync().status(&session.user_id)?))
}
async fn sync_run(
    State(state): State<PocState>,
    Extension(session): Extension<AuthenticatedSession>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Json<crate::web_sync::RunDto>, WebError> {
    if request.uri().query().is_some() {
        return Err(WebError::InvalidQuery);
    }
    let Path(id) = path.map_err(|_| WebError::NotFound)?;
    Ok(Json(state.sync().run(&session.user_id, &id)?))
}
