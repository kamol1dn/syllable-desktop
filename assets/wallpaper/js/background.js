// Background: either a random image from the user's wallpaper folder (a new one on
// every song change, crossfaded) or the animated backdrop implemented in backdrop.js.
// This file owns the wallpaper slideshow and the choice between the two.
"use strict";

const Background = (() => {
  const wpRoot = document.getElementById("wp");

  const IMAGE_EXT = /\.(jpe?g|png|webp|gif|bmp|avif)$/i;

  let mode = "wallpapers";      // user choice: "wallpapers" | "dynamic"
  let active = null;            // what is actually showing (wallpapers falls back to dynamic when the folder is empty)
  let blurPx = 0;
  let kenBurns = true;
  let collection = "all";
  let speedMul = 1;
  let audioReactive = true;

  // ---------------- wallpaper pack ----------------
  const files = new Set();
  let bag = [];
  let currentPath = null;
  let currentBitmap = null;
  let loadSeq = 0;
  let failures = 0;      // consecutive images that wouldn't load
  let broken = false;    // gave up on the folder until a new listing arrives

  // Category of an image: its folder name, or the filename prefix before "_"
  // (the pack names files like "nord_a_snowy_peak.jpg").
  function category(path) {
    const parts = path.replace(/\\/g, "/").split("/");
    const name = parts[parts.length - 1].toLowerCase();
    const dir = (parts[parts.length - 2] || "").toLowerCase();
    const prefix = name.includes("_") ? name.slice(0, name.indexOf("_")) : "";
    return [dir, prefix];
  }

  function pool() {
    const all = [...files];
    if (collection === "all") return all;
    const hit = all.filter((p) => category(p).includes(collection));
    return hit.length > 0 ? hit : all;
  }

  // Shuffle-bag: every image shows once before any repeats, never twice in a row.
  function nextPath() {
    const p = pool();
    if (p.length === 0) return null;
    bag = bag.filter((x) => files.has(x) && p.includes(x));
    if (bag.length === 0) {
      bag = p.slice();
      for (let i = bag.length - 1; i > 0; i--) {
        const j = (Math.random() * (i + 1)) | 0;
        [bag[i], bag[j]] = [bag[j], bag[i]];
      }
      if (bag.length > 1 && bag[bag.length - 1] === currentPath) bag.unshift(bag.pop());
    }
    return bag.pop();
  }

  function screenSize() {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    // Guard a not-yet-laid-out window (0x0 would make zero-size canvases).
    return [Math.max(16, Math.round(innerWidth * dpr)), Math.max(16, Math.round(innerHeight * dpr)), dpr];
  }

  async function decodeToBitmap(path) {
    const img = await loadImage(fileUrl(path), false);
    const [w, h] = screenSize();
    // Crop to the screen's aspect ("cover") and resample once, off the main
    // thread where supported — camera photos are 24 MP and would otherwise be
    // rescaled by the compositor every frame of the Ken Burns drift.
    const s = Math.max(w / img.naturalWidth, h / img.naturalHeight);
    const sw = w / s, sh = h / s;
    const sx = (img.naturalWidth - sw) / 2, sy = (img.naturalHeight - sh) / 2;
    try {
      return await createImageBitmap(img, sx, sy, sw, sh, { resizeWidth: w, resizeHeight: h, resizeQuality: "high" });
    } catch (e) {
      return { img, sx, sy, sw, sh, width: w, height: h, close() {} };
    }
  }

  function paintLayer(canvas, bmp) {
    const [w, h, dpr] = screenSize();
    canvas.width = w; canvas.height = h;
    const ctx = canvas.getContext("2d");
    const b = blurPx * dpr;
    // Overdraw by the blur radius so blurred edges don't fade to transparent.
    const m = b * 2;
    if (b > 0) ctx.filter = `blur(${b}px)`;
    if (bmp.img) ctx.drawImage(bmp.img, bmp.sx, bmp.sy, bmp.sw, bmp.sh, -m, -m, w + 2 * m, h + 2 * m);
    else ctx.drawImage(bmp, -m, -m, w + 2 * m, h + 2 * m);
  }

  function showBitmap(bmp) {
    const layer = document.createElement("canvas");
    layer.className = "wpl";
    // Each image drifts toward a different corner.
    layer.style.setProperty("--kx", (Math.random() < 0.5 ? -1 : 1) * (0.6 + Math.random() * 0.8) + "%");
    layer.style.setProperty("--ky", (Math.random() < 0.5 ? -1 : 1) * (0.4 + Math.random() * 0.6) + "%");
    layer.classList.toggle("kb", kenBurns);
    paintLayer(layer, bmp);
    const old = [...wpRoot.children];
    wpRoot.appendChild(layer);
    void layer.offsetWidth; // commit opacity:0 first so the fade-in transition runs
    layer.classList.add("in");
    setTimeout(() => old.forEach((el) => el.remove()), 1600);
  }

  async function showWallpaper(path) {
    if (!path) return;
    const seq = ++loadSeq;
    try {
      const bmp = await decodeToBitmap(path);
      if (seq !== loadSeq) { bmp.close && bmp.close(); return; } // a newer song won
      if (currentBitmap && currentBitmap.close) currentBitmap.close();
      currentBitmap = bmp;
      currentPath = path;
      failures = 0;
      showBitmap(bmp);
    } catch (e) {
      console.warn("wallpaper failed to load", path, e);
      files.delete(path);
      if (seq !== loadSeq) return;
      // A few bad files are skipped; a run of failures means the folder isn't
      // reachable from this page at all — show the gradient rather than nothing.
      if (++failures >= 5 || files.size === 0) {
        broken = true;
        failures = 0;
        apply();
        return;
      }
      showWallpaper(nextPath());
    }
  }

  let audioLevel = 0;    // latest audio level (0..1), handed to the backdrop each frame

  // ---------------- mode switching ----------------
  function effectiveMode() {
    return mode === "wallpapers" && files.size > 0 && !broken ? "wallpapers" : "dynamic";
  }

  function apply() {
    const m = effectiveMode();
    if (m === active) return;
    active = m;
    document.body.dataset.bg = m;
    // Without GPU rendering (a host with acceleration off) fall back to a CSS-blurred cover.
    if (m === "dynamic" && !Backdrop.init()) document.body.classList.add("nogl");
    if (m === "wallpapers" && !currentPath) showWallpaper(nextPath());
  }

  addEventListener("resize", () => {
    Backdrop.resize();
    if (!currentBitmap) return;
    // Grown past the resolution we decoded at (monitor/DPI change): decode again.
    const [w] = screenSize();
    if (currentBitmap.width < w * 0.9 && currentPath) showWallpaper(currentPath);
    else [...wpRoot.children].forEach((c) => paintLayer(c, currentBitmap));
  });

  return {
    setOptions(o) {
      if (o.mode !== undefined) mode = o.mode;
      if (o.collection !== undefined && o.collection !== collection) { collection = o.collection; bag = []; }
      if (o.kenBurns !== undefined) {
        kenBurns = o.kenBurns;
        [...wpRoot.children].forEach((c) => c.classList.toggle("kb", kenBurns));
      }
      if (o.blur !== undefined && o.blur !== blurPx) {
        blurPx = o.blur;
        if (currentBitmap) [...wpRoot.children].forEach((c) => paintLayer(c, currentBitmap));
      }
      if (o.speed !== undefined) speedMul = o.speed;
      if (o.audioReactive !== undefined) audioReactive = o.audioReactive;
      apply();
    },
    addFiles(list) {
      for (const p of list) if (IMAGE_EXT.test(p)) files.add(p);
      broken = false;
      apply();
    },
    removeFiles(list) {
      for (const p of list) files.delete(p);
      if (files.size === 0) currentPath = null;
      apply();
    },
    /** Replace the whole list (the folder as listed by the app). */
    setFolderFiles(list) {
      files.clear();
      bag = [];
      this.addFiles(list);
    },
    /** New song: next wallpaper from the pack (wallpaper mode only). */
    songChanged() {
      if (effectiveMode() === "wallpapers") showWallpaper(nextPath());
    },
    setCover(url) { return Backdrop.setCover(url); },
    setAudioLevel(v) { audioLevel = v; },
    frame(dt) {
      if (active === "dynamic") Backdrop.frame(Math.min(dt, 0.1), speedMul, audioReactive ? audioLevel : 0);
    },
    get mode() { return active; },
  };
})();
