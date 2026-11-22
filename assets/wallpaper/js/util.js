// Small helpers shared by the page scripts.
"use strict";

const clamp = (v, lo, hi) => (v < lo ? lo : v > hi ? hi : v);

// Load an image. onload rather than img.decode(): decode() stalls while the page is
// hidden, and onload fires either way.
function loadImage(url, cors) {
  return new Promise((resolve, reject) => {
    const img = new Image();
    if (cors && /^https?:/i.test(url)) img.crossOrigin = "anonymous";
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error("image failed: " + url));
    img.src = url;
  });
}

// file:///-URL for a Windows path from the app's folder listing (C:\dir\a b#1.jpg), or a POSIX
// path (/home/me/a b#1.jpg).
function fileUrl(path) {
  if (/^(https?|file|data):/i.test(path)) return path;
  const parts = path.replace(/\\/g, "/").replace(/^\/+/, "").split("/");
  return "file:///" + parts.map((s, i) => (i === 0 && /^[a-z]:$/i.test(s) ? s : encodeURIComponent(s))).join("/");
}
