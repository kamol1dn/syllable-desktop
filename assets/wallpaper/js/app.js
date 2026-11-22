// Wires everything together: the app's local feed (ws://127.0.0.1:PORT/wallpaper)
// for track, lyrics, position, settings, the image folder and the bass level, the
// now-playing card, the idle clock and the render loop.
"use strict";

// ---------------- settings ----------------
const settings = {
  background: "wallpapers",
  collection: "all",
  wallpaperblur: 0,
  dim: 35,
  kenburns: true,
  dynspeed: 100,
  audioreactive: true,
  layout: "split",
  lyricssize: 100,
  linepos: 45,
  lineblur: true,
  lyricstyle: "glide",   // preset from js/lyric-styles.js: glide | punch | mist
  clock: true,
  offset: 0,
  port: "9012",
  folder: "",
  fps: 60,
};
const DEFAULTS = Object.assign({}, settings);

// A host may flip one page between the two surfaces ({type:"surface"}), so this
// can change at runtime.
let IS_LOCK = window.WALLPAPER_SURFACE === "lockscreen";

// Values are coerced to the type of the default, so "false"/"120" become bool/number.
function setKey(key, raw) {
  if (!(key in settings) || raw === undefined || raw === null) return;
  const def = settings[key];
  if (typeof def === "boolean") settings[key] = raw === true || raw === "true" || raw === "1";
  else if (typeof def === "number") { const n = Number(raw); if (!Number.isNaN(n)) settings[key] = n; }
  else settings[key] = String(raw);
}

// The app's settings are pushed over the feed. The last
// ones received are remembered per surface, so the look holds while the app is closed
// (e.g. the lock screen before login).
const trayCacheKey = () => "lyricwp.tray." + (IS_LOCK ? "lockscreen" : "desktop");

// Order: built-in defaults < config.js < URL parameters < tray settings.
function loadConfig() {
  Object.assign(settings, DEFAULTS);
  if (IS_LOCK) { settings.layout = "lock"; settings.clock = false; }
  const cfg = window.WALLPAPER_CONFIG || {};
  for (const k of Object.keys(cfg)) setKey(k, cfg[k]);
  if (IS_LOCK && cfg.lockscreen) for (const k of Object.keys(cfg.lockscreen)) setKey(k, cfg.lockscreen[k]);
  for (const [k, v] of new URLSearchParams(location.search)) setKey(k, v);
  try {
    const tray = JSON.parse(localStorage.getItem(trayCacheKey()) || "null");
    if (tray) for (const k of Object.keys(tray)) setKey(k, tray[k]);
  } catch (e) {}
}
loadConfig();

function applySettings() {
  const root = document.documentElement.style;
  root.setProperty("--dim", settings.dim / 100);
  root.setProperty("--lyrics-scale", settings.lyricssize / 100);
  document.body.dataset.layout = settings.layout;
  document.body.classList.toggle("noclock", !settings.clock);
  Background.setOptions({
    mode: settings.background,
    collection: settings.collection,
    blur: settings.wallpaperblur,
    kenBurns: settings.kenburns,
    speed: settings.dynspeed / 100,
    audioReactive: settings.audioreactive,
  });
  LyricsView.setOptions({ blur: settings.lineblur, linePos: settings.linepos / 100, style: settings.lyricstyle });
}

// The desktop host calls this when the page's monitor is fully covered by another
// window (and again when it shows): stop drawing, freeze CSS animations and turn
// the bass feed off, so a hidden wallpaper costs nothing.
let hostPaused = false;
window.hostSetPaused = function (paused) {
  paused = !!paused;
  if (paused === hostPaused) return;
  hostPaused = paused;
  let s = document.getElementById("hostPause");
  if (paused && !s) {
    s = document.createElement("style");
    s.id = "hostPause";
    s.textContent = "*,*::before,*::after{animation-play-state:paused!important}";
    document.head.appendChild(s);
  } else if (!paused && s) s.remove();
  Bridge.requestAudio();
  if (!paused) startLoop();
};

// ---------------- playback clock ----------------
// Position arrives a few times a second; interpolate in between, and slew small
// corrections instead of snapping so the karaoke fill never visibly jitters.
const Clock = {
  base: 0, at: performance.now(), playing: false, rate: 1, has: false,
  now() {
    const el = this.playing ? (performance.now() - this.at) * this.rate : 0;
    return this.base + el;
  },
  update(ms, playing, rate) {
    const predicted = this.now();
    const err = ms - predicted;
    if (!this.has || playing !== this.playing || Math.abs(err) > 300) {
      this.base = ms;
    } else {
      this.base = predicted + err * 0.25;
    }
    this.at = performance.now();
    this.playing = playing;
    this.rate = rate || 1;
    this.has = true;
  },
};

// ---------------- state ----------------
const state = {
  track: null,        // { title, artist, album, cover, durationMs }
  trackKey: null,
  art: null,          // media-session art (data URL) from the feed
  lyricsState: "none",
};

const $ = (id) => document.getElementById(id);

function coverSource() {
  return (state.track && state.track.cover) || state.art || null;
}

function refreshCover() {
  const src = coverSource();
  const img = $("cover");
  if (src && img.dataset.src !== src) {
    img.dataset.src = src;
    const pre = new Image();
    pre.onload = () => { if (img.dataset.src === src) { img.src = src; img.classList.add("ready"); } };
    pre.src = src;
  } else if (!src) {
    img.classList.remove("ready");
    delete img.dataset.src;
  }
  if (src) Background.setCover(src);
}

function setTrack(track) {
  // First artist only: Spotify lists every artist ("ROSÉ, Bruno Mars") where
  // Windows' media info may carry just the first, and that isn't a new song.
  const key = track ? `${track.title}|${(track.artist || "").split(",")[0].trim()}`.toLowerCase() : null;
  const changed = key !== state.trackKey;
  state.track = track;
  state.trackKey = key;
  if (changed) {
    state.art = null;
    // The new song's position is a new baseline, not drift: take the next one as
    // it stands instead of slewing onto it from the previous song's clock (which
    // a transition into a track starting near the same position would do slowly).
    Clock.has = false;
    if (track) Background.songChanged();
  }
  document.body.classList.toggle("idle", !track);
  $("title").textContent = track ? track.title : "";
  $("artist").textContent = track ? track.artist : "";
  $("album").textContent = track && track.album ? track.album : "";
  refreshCover();
}

let lyricsSig = null, lyricsKey = null;
function setLyrics(st, lyrics) {
  // The app re-announces a track when its identity firms up (SMTC -> Spotify id)
  // and re-resolves it. Keep the lines on screen through that instead of
  // rebuilding them mid-song: ignore "loading" for the song already shown, and
  // identical payloads.
  if (st === "loading" && lyricsKey === state.trackKey && LyricsView.hasLyrics) return;
  const sig = st === "ok" ? JSON.stringify(lyrics) : st;
  if (sig === lyricsSig && lyricsKey === state.trackKey) return;
  lyricsSig = sig;
  lyricsKey = state.trackKey;
  state.lyricsState = st;
  LyricsView.set(st === "ok" ? lyrics : null);
  // While loading keep the split layout, so the card doesn't jump when lyrics land.
  document.body.classList.toggle("nolyrics", st === "none");
  document.body.classList.toggle("loading", st === "loading");
}

// ---------------- feed (the app) ----------------
const Bridge = (() => {
  let ws = null, timer = null, connected = false;

  function connect() {
    clearTimeout(timer);
    try {
      // The host passes its monitor name, so the app can tell the pages apart.
      const screen = new URLSearchParams(location.search).get("screen");
      ws = new WebSocket(`ws://127.0.0.1:${settings.port}/wallpaper` + (screen ? `?screen=${encodeURIComponent(screen)}` : ""));
    } catch (e) {
      timer = setTimeout(connect, 3000);
      return;
    }
    ws.onopen = () => {
      connected = true;
      document.body.classList.add("bridged");
      requestExtras();
    };
    ws.onclose = () => {
      if (connected) {
        connected = false;
        document.body.classList.remove("bridged");
        Interact.setSupported(false);
        setLyrics("none", null); // no lyrics source without the app
      }
      timer = setTimeout(connect, 3000);
    };
    ws.onerror = () => {};
    ws.onmessage = (ev) => {
      let m;
      try { m = JSON.parse(ev.data); } catch (e) { return; }
      switch (m.type) {
        case "track": setTrack(m.track); break;
        case "lyrics": setLyrics(m.state, m.lyrics); break;
        case "art": state.art = m.dataUrl; refreshCover(); break;
        case "pos": if (m.has) Clock.update(m.ms, m.playing, m.rate); break;
        case "wallpapers": gotWallpaperList(m); break;
        case "audio": Background.setAudioLevel(Math.min(1, (m.bass || 0) * 1.1)); break;
        case "settings": gotTraySettings(m); break;
        case "next-wallpaper": Background.songChanged(); break;
        case "surface": setSurface(m.surface); break;
        case "hello": Interact.setSupported(!!(m.caps && m.caps.controls)); break;
        case "tap": Interact.tap(m.x, m.y); break;
      }
    };
  }

  function send(obj) {
    if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(obj));
  }

  // The image folder listing and the bass level come from the app too.
  function requestExtras() {
    requestFolder();
    requestAudio();
  }
  function requestFolder() {
    if (settings.folder) send({ type: "list-wallpapers", dir: settings.folder });
  }
  function requestAudio() {
    send({ type: "audio", on: !!settings.audioreactive && !hostPaused });
  }

  return {
    start: connect,
    reconnect() { if (ws) { try { ws.onclose = null; ws.close(); } catch (e) {} } connected = false; connect(); },
    requestFolder,
    requestAudio,
    send,
    get connected() { return connected; },
  };
})();

// ---------------- tray settings ----------------
let lastTray = null;
function gotTraySettings(m) {
  lastTray = m;
  if (m.interactive !== undefined) Interact.setWanted(m.interactive);
  const mine = Object.assign({}, IS_LOCK ? m.lockscreen : m.desktop, { folder: m.folder || "" });
  const prevFolder = settings.folder, prevAudio = settings.audioreactive;
  for (const k of Object.keys(mine)) setKey(k, mine[k]);
  try { localStorage.setItem(trayCacheKey(), JSON.stringify(mine)); } catch (e) {}
  applySettings();
  if (settings.folder !== prevFolder) Bridge.requestFolder();
  if (settings.audioreactive !== prevAudio) Bridge.requestAudio();
}

// Desktop <-> lock screen, live: rebuild the settings for the other surface (its tray
// values if the app sent them, else its cached ones) without reloading the page.
function setSurface(surface) {
  const lock = surface === "lockscreen";
  if (lock === IS_LOCK) return;
  IS_LOCK = lock;
  window.WALLPAPER_SURFACE = surface;
  document.body.dataset.surface = surface;
  const prevFolder = settings.folder, prevAudio = settings.audioreactive;
  loadConfig();
  if (lastTray) gotTraySettings(lastTray);
  else applySettings();
  if (settings.folder !== prevFolder) Bridge.requestFolder();
  if (settings.audioreactive !== prevAudio) Bridge.requestAudio();
}

// ---------------- wallpaper folder via the feed ----------------
// Remember the last listing so wallpapers still rotate while the app is closed.
const LIST_CACHE = "lyricwp.folderList";
function gotWallpaperList(m) {
  if (m.dir !== settings.folder) return; // stale answer for a previous folder
  if (m.error) console.warn("wallpaper folder:", m.dir, m.error);
  Background.setFolderFiles(m.files || []);
  if (m.files && m.files.length) {
    try { localStorage.setItem(LIST_CACHE, JSON.stringify({ dir: m.dir, files: m.files })); } catch (e) {}
  }
}
(function restoreListing() {
  if (!settings.folder) return;
  try {
    const c = JSON.parse(localStorage.getItem(LIST_CACHE) || "null");
    if (c && c.dir === settings.folder && c.files && c.files.length) Background.setFolderFiles(c.files);
  } catch (e) {}
})();

// ---------------- card + clock ----------------
const fmt = (ms) => {
  const s = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
};

let lastClockText = "";
function updateChrome(t) {
  const dur = state.track ? state.track.durationMs : 0;
  if (dur > 0) {
    $("progressFill").style.transform = `scaleX(${clamp(t / dur, 0, 1)})`;
    const e = fmt(t), r = "-" + fmt(dur - t);
    if ($("elapsed").textContent !== e) $("elapsed").textContent = e;
    if ($("remaining").textContent !== r) $("remaining").textContent = r;
  }
  document.body.classList.toggle("nodur", !(dur > 0));
  document.body.classList.toggle("paused", !Clock.playing);

  const d = new Date();
  const txt = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (txt !== lastClockText) {
    lastClockText = txt;
    $("clockTime").textContent = txt;
    $("clockDate").textContent = d.toLocaleDateString([], { weekday: "long", month: "long", day: "numeric" });
  }
}

// ---------------- loop ----------------
let maxFps = settings.fps || 60, lastFrame = 0, lastDraw = 0, looping = false;
function startLoop() {
  if (looping) return;
  looping = true;
  requestAnimationFrame(loop);
}
function loop(now) {
  if (hostPaused) { looping = false; return; }
  requestAnimationFrame(loop);
  // FPS cap (the fps setting).
  if (now - lastDraw < 1000 / maxFps - 1) return;
  const dt = Math.min(0.1, (now - (lastFrame || now)) / 1000);
  lastFrame = now;
  lastDraw = now;
  const t = Clock.now() + Number(settings.offset || 0);
  Background.frame(dt);
  LyricsView.frame(t, dt);
  updateChrome(t);
}

// ---------------- boot ----------------
applySettings();
setTrack(null);
setLyrics("none", null);
if (new URLSearchParams(location.search).has("demo")) Demo.start();
else Bridge.start();
startLoop();
