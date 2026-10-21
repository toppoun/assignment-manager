//! Compile-time allowlist only: no filesystem serving or path resolution.
use crate::WebError;
use axum::{
    http::{Uri, header},
    response::{IntoResponse, Response},
};

pub(crate) async fn serve(uri: Uri) -> Response {
    if uri.path() == "/favicon.ico" {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }
    let (content_type, content): (&str, &[u8]) = match uri.path() {
        "/" => (
            "text/html; charset=utf-8",
            include_bytes!("../../../frontend/web.html"),
        ),
        "/styles.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../../../frontend/styles.css"),
        ),
        "/web_app.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../../frontend/web_app.js"),
        ),
        "/http_api.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../../frontend/http_api.js"),
        ),
        "/list_view.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../../frontend/list_view.js"),
        ),
        "/list_tasks.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../../frontend/list_tasks.js"),
        ),
        "/presentation.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../../frontend/presentation.js"),
        ),
        _ => return WebError::NotFound.into_response(),
    };
    ([(header::CONTENT_TYPE, content_type)], content).into_response()
}
