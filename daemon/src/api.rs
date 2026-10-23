//! The JSON API on 127.0.0.1. Every command answers with the status after it
//! was sent (`Status`); a refused one answers `{"error": …}` with a 4xx/5xx.
//!
//! GET  /status                    the player's state
//! POST /login                     opens Spotify's sign-in page in the browser
//! POST /login/cancel              gives up a pending sign-in
//! POST /logout                    forgets the stored sign-in
//! POST /retry                     reconnects now after a failed connect
//! POST /toggle /play /pause /next /prev /transfer
//! POST /seek?ms=N                 position in the current track
//! POST /volume?percent=N          0–100
//! POST /shuffle?on=true|false
//! POST /repeat?mode=off|context|track
//! POST /load  {"uri": …, "track": …?}  plays a track or a context (from `track`)
//! GET  /queue                     what plays next (a listing, like /open's)
//! POST /queue?uri=…               adds a track to the queue
//! POST /quality?level=lossless|very_high  the audio quality asked for
//! GET  /search?q=…                tracks, albums, artists, playlists
//! GET  /library                   Liked Songs, saved playlists, albums, artists
//! GET  /open?uri=…                a playlist's, album's, artist's or Liked Songs' rows
//! GET  /saved?uri=…               {"saved": bool}: in Liked Songs / the library
//! POST /save?uri=…&on=true|false  adds it to or removes it from the library
//! GET  /lyrics?uri=…              {"synced", "provider", "lines": [{"ms", "text"}]}
//! POST /quit                      ends the daemon

use std::{collections::HashMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

use crate::{catalog::Catalog, engine::Engine, soloist::Quality};

#[derive(Clone)]
struct App {
    engine: Engine,
    catalog: Arc<Catalog>,
}

type Params = Query<HashMap<String, String>>;

fn refused(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

fn answer(app: &App, result: Result<(), String>) -> Response {
    match result {
        Ok(()) => Json(app.engine.status()).into_response(),
        Err(e) => refused(StatusCode::CONFLICT, e),
    }
}

fn param<T: std::str::FromStr>(params: &HashMap<String, String>, name: &str) -> Result<T, String> {
    let raw = params.get(name).ok_or_else(|| format!("missing ?{name}="))?;
    raw.parse().map_err(|_| format!("bad ?{name}={raw}"))
}

async fn status(State(app): State<App>) -> Response {
    Json(app.engine.status()).into_response()
}

macro_rules! command {
    ($name:ident, $call:ident) => {
        async fn $name(State(app): State<App>) -> Response {
            let result = app.engine.$call();
            answer(&app, result)
        }
    };
    ($name:ident, $call:ident, async) => {
        async fn $name(State(app): State<App>) -> Response {
            let result = app.engine.$call().await;
            answer(&app, result)
        }
    };
}

command!(login, login);
command!(toggle, toggle, async);
command!(play, play, async);
command!(pause, pause);
command!(next, next);
command!(prev, prev);
command!(transfer, transfer, async);

async fn cancel_login(State(app): State<App>) -> Response {
    app.engine.cancel_login();
    answer(&app, Ok(()))
}

async fn logout(State(app): State<App>) -> Response {
    app.engine.logout().await;
    answer(&app, Ok(()))
}

async fn retry(State(app): State<App>) -> Response {
    app.engine.retry();
    answer(&app, Ok(()))
}

async fn quit(State(app): State<App>) -> Response {
    app.engine.quit();
    answer(&app, Ok(()))
}

async fn seek(State(app): State<App>, Query(p): Params) -> Response {
    match param::<u32>(&p, "ms") {
        Ok(ms) => answer(&app, app.engine.seek(ms)),
        Err(e) => refused(StatusCode::BAD_REQUEST, e),
    }
}

async fn volume(State(app): State<App>, Query(p): Params) -> Response {
    match param::<u8>(&p, "percent") {
        Ok(v) if v <= 100 => answer(&app, app.engine.volume(v)),
        Ok(v) => refused(StatusCode::BAD_REQUEST, format!("volume {v} is over 100")),
        Err(e) => refused(StatusCode::BAD_REQUEST, e),
    }
}

async fn shuffle(State(app): State<App>, Query(p): Params) -> Response {
    match param::<bool>(&p, "on") {
        Ok(on) => answer(&app, app.engine.shuffle(on)),
        Err(e) => refused(StatusCode::BAD_REQUEST, e),
    }
}

async fn repeat(State(app): State<App>, Query(p): Params) -> Response {
    match param::<String>(&p, "mode") {
        Ok(mode) => answer(&app, app.engine.repeat(&mode)),
        Err(e) => refused(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct Load {
    uri: String,
    track: Option<String>,
}

async fn load(State(app): State<App>, Json(body): Json<Load>) -> Response {
    let result = app.engine.load(&body.uri, body.track.as_deref()).await;
    answer(&app, result)
}

async fn queue(State(app): State<App>) -> Response {
    match app.engine.queue().await {
        Ok(listing) => Json(listing).into_response(),
        Err(e) => refused(StatusCode::CONFLICT, e),
    }
}

async fn enqueue(State(app): State<App>, Query(p): Params) -> Response {
    match param::<String>(&p, "uri") {
        Ok(uri) => answer(&app, app.engine.enqueue(&uri)),
        Err(e) => refused(StatusCode::BAD_REQUEST, e),
    }
}

async fn quality(State(app): State<App>, Query(p): Params) -> Response {
    let level = p.get("level").map(String::as_str).unwrap_or_default();
    match Quality::parse(level) {
        Some(q) => answer(&app, app.engine.set_quality(q)),
        None => refused(StatusCode::BAD_REQUEST, format!("bad ?level={level} (lossless, very_high)")),
    }
}

/// Answers a catalog call made with the signed-in session.
async fn listing<F, Fut>(app: &App, call: F) -> Response
where
    F: FnOnce(Arc<Catalog>, librespot_core::Session) -> Fut,
    Fut: Future<Output = Result<crate::catalog::Listing, String>>,
{
    let session = match app.engine.session() {
        Ok(s) => s,
        Err(e) => return refused(StatusCode::CONFLICT, e),
    };
    match call(app.catalog.clone(), session).await {
        Ok(listing) => Json(listing).into_response(),
        Err(e) => refused(StatusCode::BAD_GATEWAY, e),
    }
}

async fn search(State(app): State<App>, Query(p): Params) -> Response {
    let q = p.get("q").cloned().unwrap_or_default();
    listing(&app, |c, s| async move { c.search(&s, &q).await }).await
}

async fn library(State(app): State<App>) -> Response {
    listing(&app, |c, s| async move { c.library(&s).await }).await
}

async fn open(State(app): State<App>, Query(p): Params) -> Response {
    let Some(uri) = p.get("uri").cloned() else {
        return refused(StatusCode::BAD_REQUEST, "missing ?uri=");
    };
    listing(&app, |c, s| async move { c.open(&s, &uri).await }).await
}

async fn saved(State(app): State<App>, Query(p): Params) -> Response {
    let Some(uri) = p.get("uri").cloned() else {
        return refused(StatusCode::BAD_REQUEST, "missing ?uri=");
    };
    let session = match app.engine.session() {
        Ok(s) => s,
        Err(e) => return refused(StatusCode::CONFLICT, e),
    };
    match app.catalog.saved(&session, std::slice::from_ref(&uri)).await {
        Ok(found) => Json(json!({ "uri": uri, "saved": found[0] })).into_response(),
        Err(e) => refused(StatusCode::BAD_GATEWAY, e),
    }
}

async fn save(State(app): State<App>, Query(p): Params) -> Response {
    let (uri, on) = match (param::<String>(&p, "uri"), param::<bool>(&p, "on")) {
        (Ok(uri), Ok(on)) => (uri, on),
        (Err(e), _) | (_, Err(e)) => return refused(StatusCode::BAD_REQUEST, e),
    };
    let session = match app.engine.session() {
        Ok(s) => s,
        Err(e) => return refused(StatusCode::CONFLICT, e),
    };
    match app.catalog.set_saved(&session, &uri, on).await {
        Ok(()) => Json(json!({ "uri": uri, "saved": on })).into_response(),
        Err(e) => refused(StatusCode::BAD_GATEWAY, e),
    }
}

async fn lyrics(State(app): State<App>, Query(p): Params) -> Response {
    let Some(uri) = p.get("uri").cloned() else {
        return refused(StatusCode::BAD_REQUEST, "missing ?uri=");
    };
    let session = match app.engine.session() {
        Ok(s) => s,
        Err(e) => return refused(StatusCode::CONFLICT, e),
    };
    match crate::catalog::lyrics(&session, &uri).await {
        Ok(l) => Json(l).into_response(),
        Err(e) => refused(StatusCode::BAD_GATEWAY, e),
    }
}

pub fn router(engine: Engine, catalog: Catalog) -> Router {
    let app = App { engine, catalog: Arc::new(catalog) };
    Router::new()
        .route("/status", get(status))
        .route("/login", post(login))
        .route("/login/cancel", post(cancel_login))
        .route("/logout", post(logout))
        .route("/retry", post(retry))
        .route("/toggle", post(toggle))
        .route("/play", post(play))
        .route("/pause", post(pause))
        .route("/next", post(next))
        .route("/prev", post(prev))
        .route("/transfer", post(transfer))
        .route("/seek", post(seek))
        .route("/volume", post(volume))
        .route("/shuffle", post(shuffle))
        .route("/repeat", post(repeat))
        .route("/load", post(load))
        .route("/queue", get(queue).post(enqueue))
        .route("/quality", post(quality))
        .route("/search", get(search))
        .route("/library", get(library))
        .route("/open", get(open))
        .route("/saved", get(saved))
        .route("/save", post(save))
        .route("/lyrics", get(lyrics))
        .route("/quit", post(quit))
        .with_state(app)
}
