//! tern-spotify: a Spotify player for Tern.
//!
//! It signs in with Spotify's OAuth flow (librespot), runs Spotify's official
//! Soloist to play the audio as a Spotify Connect device on this machine,
//! and serves a small JSON API on 127.0.0.1 that the Tern plugin's Luau
//! halves drive (see `api.rs` for the routes).

mod api;
mod catalog;
mod engine;
#[cfg(target_os = "macos")]
mod nowplaying;
mod soloist;

use std::{
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use engine::{Engine, EngineConfig};

const USAGE: &str = "usage: tern-spotify serve --cache DIR --soloist DIR [--port PORT] [--name NAME]

Serves the player's JSON API on 127.0.0.1:PORT (default 47821). --cache keeps
the stored Spotify credentials. --soloist holds the official Soloist
executable (`soloist`) and your Soloist API key (`api-key`), both put there
by you, and Soloist's own data and caches. NAME is the Spotify Connect device
name (default \"Tern\").";

struct Args {
    port: u16,
    cache: PathBuf,
    soloist: PathBuf,
    name: String,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => {}
        Some("--version" | "-V") => {
            println!("tern-spotify {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        _ => return Err(USAGE.to_owned()),
    }
    let mut port = 47821;
    let mut cache = None;
    let mut soloist = None;
    let mut name = "Tern".to_owned();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value\n\n{USAGE}"));
        match flag.as_str() {
            "--port" => port = value()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--cache" => cache = Some(PathBuf::from(value()?)),
            "--soloist" => soloist = Some(PathBuf::from(value()?)),
            "--name" => name = value()?,
            _ => return Err(format!("unknown argument {flag}\n\n{USAGE}")),
        }
    }
    let cache = cache.ok_or_else(|| format!("--cache is required\n\n{USAGE}"))?;
    let soloist = soloist.ok_or_else(|| format!("--soloist is required\n\n{USAGE}"))?;
    Ok(Args { port, cache, soloist, name })
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,mdns_sd=warn"),
    )
    .format_timestamp_millis()
    .init();

    let args = match parse_args() {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some((listener, engine, catalog)) = runtime.block_on(setup(args)) else {
        return ExitCode::FAILURE;
    };

    // The main thread belongs to macOS: media keys and Control Center's
    // buttons arrive on its run loop. The daemon runs on tokio's threads.
    #[cfg(target_os = "macos")]
    nowplaying::start(engine.clone(), runtime.handle());
    let done = Arc::new(AtomicBool::new(false));
    let server = {
        let done = done.clone();
        std::thread::spawn(move || {
            runtime.block_on(serve(listener, engine, catalog));
            done.store(true, Ordering::SeqCst);
        })
    };
    #[cfg(target_os = "macos")]
    nowplaying::run_main_loop(&done);
    if server.join().is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn setup(args: Args) -> Option<(tokio::net::TcpListener, Engine, catalog::Catalog)> {
    // Bind first: a second daemon started while one runs exits here, before
    // it starts a second Soloist or touches the stored credentials.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await {
        Ok(listener) => listener,
        Err(e) => {
            log::error!("cannot listen on 127.0.0.1:{}: {e}", args.port);
            return None;
        }
    };
    let catalog = catalog::Catalog::new(args.cache.clone());
    let config = EngineConfig { cache_dir: args.cache, soloist_dir: args.soloist, device_name: args.name };
    let engine = match Engine::start(config) {
        Ok(engine) => engine,
        Err(e) => {
            log::error!("{e}");
            return None;
        }
    };
    log::info!("tern-spotify {} listening on 127.0.0.1:{}", env!("CARGO_PKG_VERSION"), args.port);
    Some((listener, engine, catalog))
}

async fn serve(listener: tokio::net::TcpListener, engine: Engine, catalog: catalog::Catalog) {
    let quit = engine.clone();
    let shutdown = async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = quit.quit_requested() => {}
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    };
    if let Err(e) = axum::serve(listener, api::router(engine.clone(), catalog))
        .with_graceful_shutdown(shutdown)
        .await
    {
        log::error!("server: {e}");
    }
    engine.shutdown().await;
}
