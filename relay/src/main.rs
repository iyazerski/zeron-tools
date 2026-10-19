//! zeron-relay: a tiny relay between a phone browser and the Zeron apps on
//! your Macs.
//!
//! Each Mac runs `zeron-tools connect`, which dials out to `/connect` and
//! stays connected ([`hub`]). The phone opens the web app, signs in with a
//! passkey ([`auth`]), and its calls are routed to the chosen Mac. Macs
//! report chats that finish, need an answer, or fail; the relay sends those
//! as Web Push notifications ([`push`]). State is one small JSON file
//! ([`store`]).
//!
//! It serves plain HTTP behind a TLS-terminating proxy. API calls must be
//! JSON POSTs from the app's own origin, with the session cookie
//! (`SameSite=Strict`), so other sites can't make them.

mod auth;
mod hub;
mod push;
mod store;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;
use webauthn_rs::prelude::{PublicKeyCredential, RegisterPublicKeyCredential};
use zeron_relay_proto::{CONNECT_PATH, Notice, NoticeKind};

use auth::Auth;
use hub::Hub;
use push::{Payload, Push, Subscription};
use store::{Machine, Store};

/// The app's files: plain HTML, CSS, and JS, no build step.
const ASSETS: &[(&str, &str, &[u8])] = &[
    (
        "/",
        "text/html; charset=utf-8",
        include_bytes!("../assets/index.html"),
    ),
    (
        "/app.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/app.css"),
    ),
    (
        "/app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/app.js"),
    ),
    (
        "/sw.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/sw.js"),
    ),
    (
        "/manifest.webmanifest",
        "application/manifest+json",
        include_bytes!("../assets/manifest.webmanifest"),
    ),
    (
        "/icon.svg",
        "image/svg+xml",
        include_bytes!("../assets/icon.svg"),
    ),
    (
        "/icon-192.png",
        "image/png",
        include_bytes!("../assets/icon-192.png"),
    ),
    (
        "/icon-512.png",
        "image/png",
        include_bytes!("../assets/icon-512.png"),
    ),
    (
        "/icon-maskable.png",
        "image/png",
        include_bytes!("../assets/icon-maskable.png"),
    ),
    (
        "/badge.png",
        "image/png",
        include_bytes!("../assets/badge.png"),
    ),
];

#[derive(Parser)]
#[command(version, about)]
struct Opts {
    /// The address people open, like https://zeron.example.com (passkeys
    /// are bound to its host)
    #[arg(long, env = "PUBLIC_URL")]
    public_url: Url,
    /// Where to listen
    #[arg(long, env = "BIND_ADDR", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,
    /// Where the passkeys, Macs, and push key are kept
    #[arg(long, env = "DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    /// A secret that allows creating a passkey without being signed in
    #[arg(long, env = "SETUP_TOKEN", hide_env_values = true)]
    setup_token: Option<String>,
}

#[derive(Clone)]
struct App {
    store: Arc<Store>,
    auth: Arc<Auth>,
    hub: Arc<Hub>,
    push: Arc<Push>,
    /// The public URL's origin, which every API call must come from.
    origin: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    let store = Arc::new(Store::open(&opts.data_dir)?);
    let app = App {
        auth: Arc::new(Auth::new(&opts.public_url, opts.setup_token)?),
        push: Arc::new(Push::open(&opts.data_dir)?),
        hub: Arc::default(),
        origin: opts.public_url.origin().ascii_serialization(),
        store,
    };
    if app.store.passkeys().is_empty() {
        if app.auth.has_setup_token() {
            eprintln!(
                "no passkey yet: open {}#/setup/<SETUP_TOKEN> to create one",
                opts.public_url
            );
        } else {
            eprintln!("no passkey yet and no SETUP_TOKEN: nobody can sign in");
        }
    }
    let mut router = Router::new();
    for &(path, mime, bytes) in ASSETS {
        router = router.route(
            path,
            get(move || async move {
                // Always revalidate, so a new release shows up at once.
                (
                    [
                        (header::CONTENT_TYPE, mime),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    bytes,
                )
            }),
        );
    }
    let signed_in = Router::new()
        .route("/api/machines", post(machines))
        .route("/api/machines/add", post(add_machine))
        .route("/api/machines/remove", post(remove_machine))
        .route("/api/call/{machine}/{tool}", post(call_tool))
        .route("/push/key", post(push_key))
        .route("/push/subscribe", post(push_subscribe))
        .route("/push/unsubscribe", post(push_unsubscribe))
        .route("/push/test", post(push_test))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_session));
    let router = router
        .merge(signed_in)
        .route("/healthz", get(|| async { "ok" }))
        .route("/auth/status", post(auth_status))
        .route("/auth/register/start", post(register_start))
        .route("/auth/register/finish", post(register_finish))
        .route("/auth/login/start", post(login_start))
        .route("/auth/login/finish", post(login_finish))
        .route("/auth/logout", post(logout))
        .route(CONNECT_PATH, get(connect))
        .layer(middleware::from_fn_with_state(app.clone(), check_origin))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(opts.bind)
        .await
        .with_context(|| format!("binding {}", opts.bind))?;
    eprintln!("listening on {} for {}", opts.bind, opts.public_url);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Refuse POSTs from any page but the app's own.
async fn check_origin(State(app): State<App>, request: Request, next: Next) -> Response {
    if request.method() == Method::POST {
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|o| o.to_str().ok());
        // Browsers always send Origin on POST; a missing one isn't a browser.
        if origin != Some(app.origin.as_str()) {
            return failure(StatusCode::FORBIDDEN, "wrong origin");
        }
    }
    next.run(request).await
}

async fn require_session(State(app): State<App>, request: Request, next: Next) -> Response {
    match auth::session_cookie(request.headers()) {
        Some(token) if app.store.session_valid(token) => next.run(request).await,
        _ => failure(StatusCode::UNAUTHORIZED, "sign in first"),
    }
}

fn signed_in(app: &App, headers: &HeaderMap) -> bool {
    auth::session_cookie(headers).is_some_and(|t| app.store.session_valid(t))
}

async fn auth_status(State(app): State<App>, headers: HeaderMap) -> Response {
    Json(json!({
        "signedIn": signed_in(&app, &headers),
        "hasPasskey": !app.store.passkeys().is_empty(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct RegisterStart {
    setup: Option<String>,
}

async fn register_start(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<RegisterStart>,
) -> Response {
    if !signed_in(&app, &headers) && !app.auth.setup_allowed(body.setup.as_deref()) {
        return failure(StatusCode::FORBIDDEN, "this setup link isn't valid");
    }
    respond(app.auth.start_register(&app.store))
}

#[derive(Deserialize)]
struct Finish<T> {
    flow: String,
    credential: T,
}

async fn register_finish(
    State(app): State<App>,
    Json(body): Json<Finish<RegisterPublicKeyCredential>>,
) -> Response {
    // The flow itself proves setup was allowed when it started.
    let result = app
        .auth
        .finish_register(&app.store, &body.flow, &body.credential)
        .and_then(|()| app.store.new_session());
    signed_in_response(result)
}

async fn login_start(State(app): State<App>, Json(_): Json<Value>) -> Response {
    respond(app.auth.start_login(&app.store))
}

async fn login_finish(
    State(app): State<App>,
    Json(body): Json<Finish<PublicKeyCredential>>,
) -> Response {
    let result = app
        .auth
        .finish_login(&app.store, &body.flow, &body.credential)
        .and_then(|()| app.store.new_session());
    signed_in_response(result)
}

/// Set the session cookie on success.
fn signed_in_response(session: Result<String>) -> Response {
    match session {
        Ok(token) => (
            [(header::SET_COOKIE, auth::set_cookie(&token))],
            Json(json!({ "ok": true })),
        )
            .into_response(),
        Err(e) => failure(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Some(token) = auth::session_cookie(&headers)
        && let Err(e) = app.store.end_session(token)
    {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }
    (
        [(header::SET_COOKIE, auth::clear_cookie())],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

/// The Macs, with whether each is connected.
async fn machines(State(app): State<App>, Json(_): Json<Value>) -> Response {
    let list: Vec<Value> = app
        .store
        .machines()
        .into_iter()
        .map(|m| {
            let mut value = json!(m);
            value["status"] = json!(app.hub.status(&m.id));
            value
        })
        .collect();
    Json(json!({ "machines": list })).into_response()
}

#[derive(Deserialize)]
struct AddMachine {
    name: String,
}

/// Add a Mac and return the command that connects it (its token is shown once).
async fn add_machine(State(app): State<App>, Json(body): Json<AddMachine>) -> Response {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return failure(StatusCode::BAD_REQUEST, "give the Mac a short name");
    }
    match app.store.add_machine(name) {
        Ok((machine, token)) => Json(json!({
            "machine": machine,
            "command": format!("zeron-tools connect --install --relay {} --token {token}", app.origin),
        }))
        .into_response(),
        Err(e) => failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct MachineId {
    id: String,
}

async fn remove_machine(State(app): State<App>, Json(body): Json<MachineId>) -> Response {
    let result = app.store.remove_machine(&body.id);
    app.hub.drop_machine(&body.id);
    done(result)
}

/// Run a Zeron tool on one Mac.
async fn call_tool(
    State(app): State<App>,
    Path((machine, tool)): Path<(String, String)>,
    Json(args): Json<Value>,
) -> Response {
    match app.hub.call(&machine, tool, args).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(StatusCode::BAD_REQUEST, error),
    }
}

/// A Mac's link: `zeron-tools connect` with `Authorization: Bearer <token>`.
async fn connect(State(app): State<App>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(machine) = token.and_then(|t| app.store.machine_for_token(t)) else {
        return failure(StatusCode::UNAUTHORIZED, "unknown Mac token");
    };
    let (hub, push, store) = (app.hub.clone(), app.push.clone(), app.store.clone());
    ws.on_upgrade(move |socket| {
        hub.serve(machine, socket, move |machine, notice| {
            let payload = payload(machine, notice, store.machines().len() > 1);
            let push = push.clone();
            tokio::spawn(async move { push.broadcast(&payload).await });
        })
    })
}

/// The notification for a Mac's notice. With several Macs, the body says which.
fn payload(machine: &Machine, notice: Notice, several: bool) -> Payload {
    let body = if several {
        format!("{} · {}", machine.name, notice.body)
    } else {
        notice.body
    };
    let kind = match notice.kind {
        NoticeKind::Finished => "finished",
        NoticeKind::Input => "input",
        NoticeKind::Failed => "failed",
    };
    Payload {
        title: notice.title,
        body,
        kind: kind.into(),
        machine: Some(machine.id.clone()),
        chat: Some(notice.chat),
    }
}

/// The VAPID public key the app subscribes with.
async fn push_key(State(app): State<App>, Json(_): Json<Value>) -> Response {
    Json(json!({ "publicKey": app.push.public_key() })).into_response()
}

async fn push_subscribe(State(app): State<App>, Json(sub): Json<Subscription>) -> Response {
    done(app.push.subscribe(sub).await)
}

#[derive(Deserialize)]
struct Endpoint {
    endpoint: String,
}

async fn push_unsubscribe(State(app): State<App>, Json(e): Json<Endpoint>) -> Response {
    done(app.push.unsubscribe(&e.endpoint).await)
}

/// Send a test notification to the calling browser.
async fn push_test(State(app): State<App>, Json(e): Json<Endpoint>) -> Response {
    let payload = Payload {
        title: "Notifications work".into(),
        body: "You'll hear from your Macs when a chat finishes or needs you.".into(),
        kind: "test".into(),
        machine: None,
        chat: None,
    };
    done(app.push.send_to(&e.endpoint, &payload).await)
}

fn respond<T: serde::Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

fn done(result: Result<()>) -> Response {
    respond(result.map(|()| json!({ "ok": true })))
}

fn failure(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(json!({ "error": error.into() }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_name_the_mac_only_when_there_are_several() {
        let mac = Machine {
            id: "m".into(),
            name: "Work Mac".into(),
        };
        let notice = Notice {
            kind: NoticeKind::Input,
            chat: "c".into(),
            title: "Deploy".into(),
            body: "Needs your answer: where?".into(),
        };
        let one = payload(&mac, notice.clone(), false);
        assert_eq!(one.body, "Needs your answer: where?");
        assert_eq!(
            (
                one.kind.as_str(),
                one.machine.as_deref(),
                one.chat.as_deref()
            ),
            ("input", Some("m"), Some("c"))
        );
        let several = payload(&mac, notice, true);
        assert_eq!(several.body, "Work Mac · Needs your answer: where?");
    }
}
