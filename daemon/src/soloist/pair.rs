//! First sign-in of a Soloist instance: the normal Spotify Connect pairing a
//! phone does, with the account the daemon already signed in to.
//!
//! Soloist's ZeroConf endpoint (`getInfo`) offers an `authInfo`: the id of a
//! pending authorization and the PKCE challenge and DPoP key thumbprint it is
//! bound to (Soloist keeps the verifier and the key). Spotify's accounts
//! service exchanges the signed-in session's access token for an
//! authorization code bound to those; `addUser` hands the code to Soloist,
//! which redeems it and keeps its own session from then on.
//!
//! No credential of the daemon's is copied to Soloist, and none of Soloist's
//! is read. The code is held in memory only and never logged.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use librespot_core::Session;
use serde_json::Value;

const SERVICE: &str = "_spotify-connect._tcp.local.";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// How long DNS-SD may take to find the instance's ZeroConf endpoint.
const DISCOVERY: Duration = Duration::from_secs(15);
/// The ZeroConf protocol version `addUser` is sent as (what Soloist reports).
const ZEROCONF_VERSION: &str = "2.10.0";

/// Pairs the Soloist instance whose Connect device id is `device_id` with the
/// account `session` is signed in to. `controller` names the device that
/// paired it (shown in Spotify's device list history).
pub async fn pair(http: &reqwest::Client, session: &Session, device_id: &str, controller: &str) -> Result<(), String> {
    let base = discover(http, device_id).await?;
    // The authorization the code is bound to; no getInfo after this one until
    // `addUser`, since a new one may replace it.
    let info = get_info(http, &base).await?;
    let auth_info = info
        .get("authInfo")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("Soloist offers no bound authorization (authInfo); this build can't be paired automatically")?;
    let auth = AuthInfo::parse(&STANDARD.decode(auth_info).map_err(|e| format!("authInfo: {e}"))?)?;

    let token = session.login5().auth_token().await.map_err(|e| format!("no access token: {e}"))?;
    let code = exchange(http, session, &auth, &token.access_token).await?;

    let blob = STANDARD.encode(auth_token(&auth.id, &code));
    let username = session.username();
    let form: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("action", "addUser")
        .append_pair("version", ZEROCONF_VERSION)
        .append_pair("tokenType", "auth_token")
        .append_pair("clientKey", "")
        .append_pair("userName", &username)
        .append_pair("loginId", &username)
        .append_pair("blob", &blob)
        .append_pair("deviceName", controller)
        .finish();
    let reply: Value = http
        .post(&base)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .map_err(|e| format!("addUser: {}", e.without_url()))?
        .json()
        .await
        .map_err(|e| format!("addUser: {}", e.without_url()))?;
    let status = reply.get("status").and_then(Value::as_i64).unwrap_or(0);
    if status != 101 {
        let what = reply.get("statusString").and_then(Value::as_str).unwrap_or("refused");
        let code = reply.get("spotifyError").and_then(Value::as_i64).unwrap_or(0);
        return Err(format!("Soloist refused the sign-in: {what} (status {status}, spotifyError {code})"));
    }
    Ok(())
}

/// The URL of the instance's ZeroConf endpoint (`http://127.0.0.1:PORT/CPATH`),
/// found by DNS-SD (port and the TXT record's `CPath`) and confirmed by the
/// device id its `getInfo` answers.
async fn discover(http: &reqwest::Client, device_id: &str) -> Result<String, String> {
    let mdns = mdns_sd::ServiceDaemon::new().map_err(|e| format!("DNS-SD: {e}"))?;
    let events = mdns.browse(SERVICE).map_err(|e| format!("DNS-SD: {e}"))?;
    let found = tokio::time::timeout(DISCOVERY, async {
        let mut tried = std::collections::HashSet::new();
        while let Ok(event) = events.recv_async().await {
            let mdns_sd::ServiceEvent::ServiceResolved(service) = event else { continue };
            let path = service.txt_properties.get_property_val_str("CPath").unwrap_or("/");
            let slash = if path.starts_with('/') { "" } else { "/" };
            // Soloist listens on every interface: ask it on loopback.
            let base = format!("http://127.0.0.1:{}{slash}{path}", service.get_port());
            if !tried.insert(base.clone()) {
                continue;
            }
            let Ok(info) = get_info(http, &base).await else { continue };
            if info.get("deviceID").and_then(Value::as_str) == Some(device_id) {
                return Some(base);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    let _ = mdns.stop_browse(SERVICE);
    let _ = mdns.shutdown();
    found.ok_or_else(|| "Soloist's Spotify Connect endpoint did not show up on DNS-SD".to_owned())
}

async fn get_info(http: &reqwest::Client, base: &str) -> Result<Value, String> {
    http.get(format!("{base}?action=getInfo"))
        .send()
        .await
        .map_err(|e| format!("getInfo: {}", e.without_url()))?
        .json()
        .await
        .map_err(|e| format!("getInfo: {}", e.without_url()))
}

/// An authorization code for Soloist's pending authorization, issued to the
/// session's own client id with Soloist's client id as the audience.
async fn exchange(http: &reqwest::Client, session: &Session, auth: &AuthInfo, access_token: &str) -> Result<String, String> {
    let form: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &session.client_id())
        .append_pair("audience", &auth.client_id)
        .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange")
        .append_pair("requested_token_type", "urn:spotify:params:oauth:authorization_code")
        .append_pair("resource", "urn:spotify:resources:connect")
        .append_pair("scope", "streaming")
        .append_pair("subject_token_type", "urn:ietf:params:oauth:token-type:access_token")
        .append_pair("subject_token", access_token)
        .append_pair("code_challenge", &auth.pkce_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("dpop_jkt", &auth.jkt)
        .finish();
    let response = http
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .map_err(|e| format!("token exchange: {}", e.without_url()))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        // The error fields name the problem; nothing secret comes back.
        let why = body.get("error_description").or_else(|| body.get("error")).and_then(Value::as_str).unwrap_or("");
        return Err(format!("Spotify refused the token exchange ({status}) {why}").trim_end().to_owned());
    }
    body.get("access_token")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "the token exchange answered no authorization code".to_owned())
}

/// What `authInfo` holds (base64 protobuf):
/// `AuthInfo { string id = 1; string client_id = 2; Info info = 3 }`,
/// `Info { DpopBoundAuthCodeInfo dpop_bound_auth_code = 1 }`,
/// `DpopBoundAuthCodeInfo { string jkt = 1; string pkce_code_challenge = 2 }`.
#[derive(Debug, Default)]
struct AuthInfo {
    id: String,
    client_id: String,
    jkt: String,
    pkce_challenge: String,
}

impl AuthInfo {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut out = AuthInfo::default();
        for (field, value) in proto::fields(bytes)? {
            match field {
                1 => out.id = proto::string(value)?,
                2 => out.client_id = proto::string(value)?,
                3 => {
                    for (f, info) in proto::fields(value)? {
                        if f != 1 {
                            continue;
                        }
                        for (g, v) in proto::fields(info)? {
                            match g {
                                1 => out.jkt = proto::string(v)?,
                                2 => out.pkce_challenge = proto::string(v)?,
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if out.id.is_empty() || out.client_id.is_empty() || out.jkt.is_empty() || out.pkce_challenge.is_empty() {
            return Err("authInfo lacks the DPoP-bound authorization fields".to_owned());
        }
        Ok(out)
    }
}

/// `AuthToken { string auth_info_id = 1; DpopBoundAuthCode dpop_bound_auth_code = 10 }`,
/// `DpopBoundAuthCode { string value = 1 }`. The code is field 10: as field 2
/// Soloist answers `session_unknown_connect_auth_token_type`.
fn auth_token(auth_info_id: &str, code: &str) -> Vec<u8> {
    let mut inner = Vec::new();
    proto::put_bytes(&mut inner, 1, code.as_bytes());
    let mut out = Vec::new();
    proto::put_bytes(&mut out, 1, auth_info_id.as_bytes());
    proto::put_bytes(&mut out, 10, &inner);
    out
}

/// The few protobuf wire rules the two messages need.
mod proto {
    fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *bytes.get(*at).ok_or("truncated protobuf")?;
            *at += 1;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("bad protobuf varint".to_owned())
    }

    /// The length-delimited fields of a message, as (number, bytes); other
    /// wire types are skipped.
    pub fn fields(bytes: &[u8]) -> Result<Vec<(u64, &[u8])>, String> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let key = varint(bytes, &mut at)?;
            match key & 7 {
                0 => {
                    varint(bytes, &mut at)?;
                }
                1 => at += 8,
                5 => at += 4,
                2 => {
                    let len = usize::try_from(varint(bytes, &mut at)?).map_err(|e| e.to_string())?;
                    let end = at.checked_add(len).filter(|e| *e <= bytes.len()).ok_or("truncated protobuf")?;
                    out.push((key >> 3, &bytes[at..end]));
                    at = end;
                }
                t => return Err(format!("unexpected protobuf wire type {t}")),
            }
        }
        Ok(out)
    }

    pub fn string(bytes: &[u8]) -> Result<String, String> {
        String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
    }

    fn put_varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    pub fn put_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
        put_varint(out, (field << 3) | 2);
        put_varint(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_token_puts_the_code_in_field_10() {
        let bytes = auth_token("id-1", "code-xyz");
        let fields = proto::fields(&bytes).unwrap();
        assert_eq!(fields[0], (1, b"id-1".as_slice()));
        assert_eq!(fields[1].0, 10);
        assert_eq!(proto::fields(fields[1].1).unwrap(), vec![(1, b"code-xyz".as_slice())]);
    }

    #[test]
    fn auth_info_reads_the_nested_dpop_fields() {
        let mut dpop = Vec::new();
        proto::put_bytes(&mut dpop, 1, b"thumb");
        proto::put_bytes(&mut dpop, 2, b"challenge");
        let mut info = Vec::new();
        proto::put_bytes(&mut info, 1, &dpop);
        let mut msg = Vec::new();
        proto::put_bytes(&mut msg, 1, b"auth-id");
        proto::put_bytes(&mut msg, 2, b"client");
        proto::put_bytes(&mut msg, 3, &info);
        let a = AuthInfo::parse(&msg).unwrap();
        assert_eq!((a.id.as_str(), a.client_id.as_str()), ("auth-id", "client"));
        assert_eq!((a.jkt.as_str(), a.pkce_challenge.as_str()), ("thumb", "challenge"));
        assert!(AuthInfo::parse(&msg[..msg.len() - 3]).is_err());
    }
}
