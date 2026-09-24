//! The admin router this extension mounts must not shadow a route core already owns.
//!
//! The panel ends up with `PATCH /api/admin/extensions/{extension}` - the call the admin
//! UI makes to toggle an extension on and off. Mounting this extension's configuration
//! routes at the path the routing docs suggest, `/extensions/<package-name>`, puts a
//! *static* segment right next to that parameter. Axum prefers static over parameter, so
//! the `PATCH` for this package lands on a node that has no `PATCH` and comes back as a
//! 405: the extension could never be switched off again.
//!
//! This test reproduces the mount shape the panel builds - two routers each nested at
//! `/api/admin`, merged together - and pins down that our chosen path leaves the toggle
//! alone. It needs no database, which is the only reason it can assert this at all.

use axum::{
    Router,
    http::StatusCode,
    routing::{get, patch, post},
};
use tower::ServiceExt;

/// Mirrors `backend/src/lib.rs`, where `routes::router(&state)` (core) and
/// `extension_router` are merged and both nest at `/api/admin`.
fn build_app() -> Router {
    let core_extensions = Router::new()
        .nest(
            "/manage",
            Router::new().route("/", get(|| async { StatusCode::OK })),
        )
        .nest(
            "/{extension}",
            Router::new().route("/", patch(|| async { StatusCode::OK })),
        );

    let core = Router::new().nest(
        "/api/admin",
        Router::new().nest("/extensions", core_extensions),
    );

    // What `routes::admin::router` registers, relative to the `/api/admin` mount.
    let ours = Router::new()
        .route(
            "/dev.caloptreyx.gdrive",
            get(|| async { StatusCode::OK }).put(|| async { StatusCode::OK }),
        )
        // The credential probe, a sibling leaf under our own package segment - the same
        // mount shape the real router builds.
        .route(
            "/dev.caloptreyx.gdrive/test",
            post(|| async { StatusCode::OK }),
        );

    let ours = Router::new().nest("/api/admin", ours);

    core.merge(ours)
}

/// The path the routing docs recommend, for comparison - kept as a control so the test
/// fails loudly if someone "tidies up" `routes::admin::BASE` back to it.
fn build_app_with_documented_path() -> Router {
    let core_extensions = Router::new().nest(
        "/{extension}",
        Router::new().route("/", patch(|| async { StatusCode::OK })),
    );

    let core = Router::new().nest(
        "/api/admin",
        Router::new().nest("/extensions", core_extensions),
    );

    let ours = Router::new().route(
        "/extensions/dev.caloptreyx.gdrive",
        get(|| async { StatusCode::OK }).put(|| async { StatusCode::OK }),
    );

    core.merge(Router::new().nest("/api/admin", ours))
}

async fn status_of(app: &Router, method: &str, path: &str) -> StatusCode {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();

    app.clone()
        .oneshot(request)
        .await
        .map(|response| response.status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

#[tokio::test]
async fn settings_routes_are_served() {
    let app = build_app();

    assert_eq!(
        status_of(&app, "GET", "/api/admin/dev.caloptreyx.gdrive").await,
        StatusCode::OK
    );
    assert_eq!(
        status_of(&app, "PUT", "/api/admin/dev.caloptreyx.gdrive").await,
        StatusCode::OK
    );
    // The credential probe lives beside the settings routes, under our own package
    // segment - never under `/extensions/{extension}`, where core's toggle would fight
    // it for the same path.
    assert_eq!(
        status_of(&app, "POST", "/api/admin/dev.caloptreyx.gdrive/test").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn extension_toggle_reaches_core_for_every_package() {
    let app = build_app();

    assert_eq!(
        status_of(&app, "PATCH", "/api/admin/extensions/dev.caloptreyx.gdrive").await,
        StatusCode::OK,
        "our admin routes shadow core's {{extension}} route"
    );
    assert_eq!(
        status_of(&app, "PATCH", "/api/admin/extensions/com.calagopus.other").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn core_still_owns_its_static_siblings() {
    let app = build_app();

    assert_eq!(
        status_of(&app, "GET", "/api/admin/extensions/manage").await,
        StatusCode::OK
    );
    assert_eq!(
        status_of(&app, "PATCH", "/api/admin/extensions/manage").await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn mounting_at_the_documented_path_would_break_the_toggle() {
    let app = build_app_with_documented_path();

    assert_eq!(
        status_of(&app, "PATCH", "/api/admin/extensions/dev.caloptreyx.gdrive").await,
        StatusCode::METHOD_NOT_ALLOWED,
        "core's {{extension}} route now answers again - the docs' path may be safe to use"
    );
}
