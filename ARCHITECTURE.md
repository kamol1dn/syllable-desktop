# Syllable (working name): native Windows app

Rust port of the C# prototype in the parent folder (`../TaskbarLyrics`, plus the
prototype branches checked out under `../.claude/worktrees/`). This folder is its own
git repository and is private: nothing here goes into the public `spicy-taskbar` repo.

## Build

Rust lives on D: and may not be on PATH in an already-open shell:

    export RUSTUP_HOME='D:\rust\rustup' CARGO_HOME='D:\rust\cargo' PATH="/d/rust/cargo/bin:$PATH"
    cargo build            # debug: target/debug/syllable.exe (console shows the log)
    cargo test
    cargo build --release  # no console window

When several people or sessions build at once, give each its own
`CARGO_TARGET_DIR` (e.g. `D:\rust\target\<name>`) so builds do not wait on one lock.

## Layout

| crate | owns |
|---|---|
| `crates/core` | shared contracts: models, config, event bus, playback clock, audio slot, log |
| `crates/media` | Windows media session (SMTC): track, artwork, position |
| `crates/lyrics` | lyric providers, cache, secrets, optional Spotify sign-in |
| `crates/taskbar` | lyrics strip and active-app name, docked into the taskbar |
| `crates/audio` | WASAPI loopback + spectrum |
| `crates/wallpaper` | local feed server + WebView2 host behind the desktop icons |
| `crates/settings` | settings window and style presets |
| `crates/app` | `main`, module start-up, tray icon, message loop |

Rules:

- Module crates depend on `syllable-core` only, never on each other. They talk through
  the bus. `crates/app` is the only place that names every module.
- Each module exposes `pub fn start(ctx: Context) -> anyhow::Result<()>`. It returns
  quickly; lasting work runs on the module's own threads (a window needs its own thread
  with a message loop). Threads exit on `Event::Quit`.
- No async runtime. Blocking I/O on worker threads (`ureq` for HTTP, `tungstenite` for
  WebSockets) keeps the binary small and the code simple.
- Win32/WinRT through the `windows` crate. No `unsafe` outside FFI calls.

## Contracts in `syllable-core`

- `Context { bus, config }`: cloneable handle given to every module.
- `Bus::subscribe() -> Receiver<Event>` / `Bus::publish(Event)`. Events:
  `TrackChanged`, `TrackUpdated`, `Artwork`, `Lyrics`, `PlaybackChanged`,
  `ConfigChanged`, `OpenSettings`, `AccountsCommand`, `AccountsChanged`, `NextWallpaper`, `Quit`
  (documented in `bus.rs`).
- `position::{set, clear, now_ms, playing}`: the playback clock. `media` writes it;
  anything drawing lyrics reads `now_ms()` each frame and adds `GlobalOffsetMs`.
- `audio::{set, latest}`: the latest spectrum frame, written by `audio`.
- `ConfigStore::{read, snapshot, update}`: `update` saves and publishes `ConfigChanged`.
  The file is `%LOCALAPPDATA%\Syllable\config.json`, PascalCase keys.
- `log!("…")` appends to `%LOCALAPPDATA%\Syllable\log.txt`.
- `matching`: title/artist cleanup and scoring, shared by media and lyrics. The
  lyrics providers use the Android app's rules (`query`, `match_score`): one
  cleaned (title, artist) query per song, results that are clearly another song
  scored -1, no title-only fallback, and Spotify desktop's duration treated as
  possibly up to 40 s early. The prototype's `candidates`/`pick_best` remain.
- `accounts`: the lyric sources the user can set up; see "Accounts" below.

Changing core: additions only (a new `Event` variant, a new config field with a
default). Never rename or remove what another module may already use, and keep the
workspace compiling when you save.

## Who publishes what

| event | published by | consumed by |
|---|---|---|
| `TrackChanged`, `TrackUpdated`, `Artwork`, `PlaybackChanged` | media | lyrics, taskbar, wallpaper, audio |
| `Lyrics` | lyrics | taskbar, wallpaper |
| `ConfigChanged` | whoever calls `ConfigStore::update` (settings, tray) | every module |
| `OpenSettings` | tray | settings |
| `AccountsCommand` | settings (tray, later) | lyrics |
| `AccountsChanged` | lyrics | settings |
| `NextWallpaper` | settings ("Next wallpaper" button) | wallpaper |
| `Quit` | app | every module |

## Accounts

The optional lyric sources: a Spicy Lyrics developer API key, a Spotify web-player
sign-in, and the user's own Spotify app (Client ID with PKCE). Lyrics owns them. Settings
drives them through core alone, with no crate dependency.

- **State.** `syllable_core::accounts::current() -> Arc<AccountsState>` holds:
  - `ready`
  - `has_spicy_key`
  - `spotify_signed_in` and `spotify_user_name`
  - `spotify_client_id` and `spotify_app_connected`
  - `spotify_redirect_uri`, which the user registers in their Spotify app
  - `busy: Vec<AccountsAction>`
  - `last_result: Option<AccountsResult { action, ok, message, seq }>`

  Read it when a window opens, because the bus keeps no history. Read it again on every
  `Event::AccountsChanged`. Keys and tokens never appear in it.
- **Commands.** Publish `Event::AccountsCommand(AccountsCommand::…)` with one of:
  - `SetSpicyKey(Option<String>)`
  - `TestSpicyKey(String)`
  - `SpotifySignIn`, which opens a WebView2 window
  - `SpotifySignOut`
  - `ConnectSpotifyApp { client_id }`, which opens the default browser and waits up
    to 5 minutes for the redirect to `http://127.0.0.1:<SpotifyRedirectPort>/callback`
    (default 9013)
  - `CancelSpotifyApp`
  - `DisconnectSpotifyApp`

  Long commands show up in `busy` and end with a new `last_result` (a higher `seq`).
  `Debug` redacts keys.
- **Storage.** Secrets are kept in `%LOCALAPPDATA%\Syllable\secrets.dat`, protected
  with DPAPI for the current user. The web player's sign-in cookie lives in its own
  WebView2 profile, `%LOCALAPPDATA%\Syllable\spotify-webview`. Lyrics are cached in
  `%LOCALAPPDATA%\Syllable\cache\`.
- **Resolve order.** Lyrics re-resolves the current song whenever what is set up
  changes. The order is:
  1. the cache
  2. `spicy-api` (with a key)
  3. `spotify` (signed in)
  4. `lrclib` (always)

  `Lyrics::source` names the provider that answered.
