//! The Accounts group (Spicy Lyrics API key, Spotify sign-in, Spotify app client ID),
//! wired to the lyrics module's contract in `syllable_core::accounts`.
//!
//! Page → Rust: {"type":"account","action": A, "value": V}
//!   A = "setSpicyKey" (V = key) | "clearSpicyKey" | "testSpicyKey" (V = key)
//!     | "spotifySignIn" | "spotifySignOut"
//!     | "connectSpotifyApp" (V = client id) | "cancelSpotifyApp" | "disconnectSpotifyApp"
//!
//! Rust → page, inside every `state` message, `accounts` (see `state`). Keys and tokens
//! never reach the page: only whether they are set.

use serde_json::{json, Value};
use syllable_core::accounts::{self, AccountsAction, AccountsCommand};
use syllable_core::{Context, Event};

fn action_id(a: AccountsAction) -> &'static str {
    match a {
        AccountsAction::TestSpicyKey => "testSpicyKey",
        AccountsAction::SpotifySignIn => "spotifySignIn",
        AccountsAction::SpotifySignOut => "spotifySignOut",
        AccountsAction::ConnectSpotifyApp => "connectSpotifyApp",
    }
}

/// What the page shows in the Accounts group.
pub fn state(_ctx: &Context) -> Value {
    let s = accounts::current();
    json!({
        "ready": s.ready,
        "spicy": { "hasKey": s.has_spicy_key },
        "spotify": {
            "signedIn": s.spotify_signed_in,
            "user": s.spotify_user_name,
            "clientId": s.spotify_client_id.clone().unwrap_or_default(),
            "appConnected": s.spotify_app_connected,
            "redirectUri": s.spotify_redirect_uri,
        },
        "busy": s.busy.iter().map(|a| action_id(*a)).collect::<Vec<_>>(),
        "result": s.last_result.as_ref().map(|r| json!({
            "action": action_id(r.action), "ok": r.ok, "message": r.message, "seq": r.seq,
        })),
    })
}

/// Turn an `account` message into a command for the lyrics module. Outcomes come back
/// as `AccountsChanged` (and a new `result`), which refreshes the page.
pub fn handle(ctx: &Context, msg: &Value) -> Option<Value> {
    let value = msg["value"].as_str().unwrap_or("").trim().to_string();
    let command = match msg["action"].as_str().unwrap_or("") {
        "setSpicyKey" => AccountsCommand::SetSpicyKey((!value.is_empty()).then_some(value)),
        "clearSpicyKey" => AccountsCommand::SetSpicyKey(None),
        "testSpicyKey" if !value.is_empty() => AccountsCommand::TestSpicyKey(value),
        "spotifySignIn" => AccountsCommand::SpotifySignIn,
        "spotifySignOut" => AccountsCommand::SpotifySignOut,
        "connectSpotifyApp" if !value.is_empty() => AccountsCommand::ConnectSpotifyApp { client_id: value },
        "cancelSpotifyApp" => AccountsCommand::CancelSpotifyApp,
        "disconnectSpotifyApp" => AccountsCommand::DisconnectSpotifyApp,
        other => {
            syllable_core::log!("settings: account action {other:?} ignored");
            return Some(crate::bridge::toast("Enter a value first.", "error"));
        }
    };
    let saved = matches!(command, AccountsCommand::SetSpicyKey(_));
    let had_key = accounts::current().has_spicy_key;
    ctx.bus.publish(Event::AccountsCommand(command.clone()));
    if !accounts::current().ready {
        return Some(crate::bridge::toast("The lyrics module is still starting. Try again in a moment.", "info"));
    }
    saved.then(|| {
        let text = match command {
            AccountsCommand::SetSpicyKey(Some(_)) => "API key saved.",
            _ if had_key => "API key removed.",
            _ => "No key was stored.",
        };
        crate::bridge::toast(text, "ok")
    })
}
