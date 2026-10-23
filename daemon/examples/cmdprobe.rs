// Throwaway: does a connect-state player command reach Soloist?
use librespot_core::{Session, SessionConfig, cache::Cache};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (cache_dir, to, from, ctx, track) = (&args[1], &args[2], &args[3], &args[4], &args[5]);
    let cache = Cache::new(Some(cache_dir), None, None, None).unwrap();
    let creds = cache.credentials().expect("credentials");
    let session = Session::new(SessionConfig::default(), None);
    session.connect(creds, false).await.expect("connect");
    let body = serde_json::json!({
        "command": {
            "endpoint": "play",
            "context": { "uri": ctx, "url": format!("context://{ctx}"), "metadata": {} },
            "play_origin": { "feature_identifier": "tern" },
            "options": {
                "license": "premium",
                "skip_to": { "track_uri": track },
                "player_options_override": { "shuffling_context": false, "repeating_context": false, "repeating_track": false },
            },
        }
    })
    .to_string();
    let from = if from == "self" { session.device_id().to_owned() } else { from.clone() };
    let endpoint = format!("/connect-state/v1/player/command/from/{from}/to/{to}");
    let r = session
        .spclient()
        .request_as_json(&reqwest::Method::POST, &endpoint, None, Some(&body))
        .await;
    match r {
        Ok(b) => println!("ok {}", String::from_utf8_lossy(&b)),
        Err(e) => println!("err {e}"),
    }
}
