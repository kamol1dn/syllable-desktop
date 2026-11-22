// The animated album-art backdrop ("dynamic" background).
//
// Technique: the cover is never drawn. It is reduced to a handful of colours
// (Palette, below), and a fragment shader paints a field of those colours: five
// colour sources wander on slow looping paths, every pixel takes a distance-weighted
// blend of them, and the plane they live on is bent by two layers of slow waves so
// the borders between colours keep folding. With "reacts to bass" on, the music's
// low end makes the field breathe (sources swell, the picture lifts slightly) and
// pushes the motion along. A song change cross-fades the colours, it does not cut.
//
// The field has no fine detail, so it is rendered small and stretched by CSS, which
// keeps it cheap on integrated GPUs. Without WebGL, #dynCss shows the same colours
// as layered CSS gradients (style.css turns it slowly).
//
// Also publishes the cover's colours for the rest of the page:
//   --accent        strongest colour of the cover, "r g b"
//   --accent-text   the same hue made light enough to read as text
"use strict";

// ---------------- colours of a cover ----------------
const Palette = (() => {
  const N = 5;
  const NEUTRAL = [[0.16, 0.14, 0.30], [0.07, 0.09, 0.20], [0.26, 0.15, 0.33], [0.05, 0.13, 0.22], [0.12, 0.08, 0.17]];

  const lum = (c) => 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
  const chroma = (c) => Math.max(c[0], c[1], c[2]) - Math.min(c[0], c[1], c[2]);
  const dist2 = (a, b) => (a[0] - b[0]) ** 2 + (a[1] - b[1]) ** 2 + (a[2] - b[2]) ** 2;

  // Pull a colour's lightness into [lo, hi] without changing its hue.
  function fitLum(c, lo, hi) {
    const l = lum(c);
    if (l < lo) { const t = (lo - l) / Math.max(1e-4, 1 - l); return c.map((v) => v + (1 - v) * t); }
    if (l > hi) { const s = hi / l; return c.map((v) => v * s); }
    return c.slice();
  }
  // Move a colour away from its own grey (k > 1 = more vivid).
  function vivid(c, k) {
    const l = lum(c);
    return c.map((v) => Math.min(1, Math.max(0, l + (v - l) * k)));
  }

  // Cluster the pixels of a small copy of the image into N colours.
  // Returns null when the pixels cannot be read (cross-origin image without CORS).
  function fromImage(img) {
    const S = 28;
    const cv = document.createElement("canvas");
    cv.width = cv.height = S;
    const cx = cv.getContext("2d", { willReadFrequently: true });
    let data;
    try {
      cx.drawImage(img, 0, 0, S, S);
      data = cx.getImageData(0, 0, S, S).data;
    } catch (e) { return null; }
    const px = [];
    for (let i = 0; i < data.length; i += 4) {
      if (data[i + 3] < 128) continue;
      px.push([data[i] / 255, data[i + 1] / 255, data[i + 2] / 255]);
    }
    if (px.length < N) return null;

    // Seeds: start from the most colourful pixel, then keep taking the pixel
    // furthest from every seed so far. Deterministic, so a cover always gives the same look.
    let first = 0;
    for (let i = 1; i < px.length; i++) if (chroma(px[i]) > chroma(px[first])) first = i;
    const centres = [px[first].slice()];
    const near = px.map((p) => dist2(p, centres[0]));
    while (centres.length < N) {
      let far = 0;
      for (let i = 1; i < px.length; i++) if (near[i] > near[far]) far = i;
      centres.push(px[far].slice());
      for (let i = 0; i < px.length; i++) near[i] = Math.min(near[i], dist2(px[i], centres[centres.length - 1]));
    }
    const count = new Array(N).fill(0);
    for (let pass = 0; pass < 7; pass++) {
      const sum = centres.map(() => [0, 0, 0]);
      count.fill(0);
      for (const p of px) {
        let b = 0, bd = Infinity;
        for (let k = 0; k < N; k++) { const d = dist2(p, centres[k]); if (d < bd) { bd = d; b = k; } }
        sum[b][0] += p[0]; sum[b][1] += p[1]; sum[b][2] += p[2]; count[b]++;
      }
      for (let k = 0; k < N; k++) if (count[k]) centres[k] = sum[k].map((v) => v / count[k]);
    }

    // Biggest areas first; the field gives earlier colours more room.
    const order = centres.map((c, k) => k).sort((a, b) => count[b] - count[a]);
    const colours = order.map((k) => centres[k]);
    const share = order.map((k) => count[k] / px.length);

    // Accent: colourful and not a speck. Falls back to the main colour for grey covers.
    let ai = 0, best = -1;
    colours.forEach((c, i) => { const s = chroma(c) * (0.35 + Math.sqrt(share[i])); if (s > best) { best = s; ai = i; } });
    const accent = colours[ai];

    // The backdrop sits behind white text: keep it out of the very bright and the
    // pitch black, and give washed-out covers a little more colour.
    const field = colours.map((c) => fitLum(vivid(c, 1.18), 0.05, 0.52));
    return { field, share, accent: fitLum(vivid(accent, 1.25), 0.2, 0.62), accentText: fitLum(vivid(accent, 1.15), 0.68, 0.86) };
  }

  const neutral = () => ({ field: NEUTRAL.map((c) => c.slice()), share: [0.3, 0.25, 0.2, 0.15, 0.1], accent: [0.55, 0.5, 0.9], accentText: [0.86, 0.84, 1] });

  return { N, fromImage, neutral };
})();

// ---------------- the field ----------------
const Backdrop = (() => {
  const canvas = document.getElementById("dyn");
  const cssFallback = document.getElementById("dynCss");
  const N = Palette.N;

  const VERT = `
attribute vec2 aPos;
varying vec2 vUv;
void main() { vUv = aPos * 0.5 + 0.5; gl_Position = vec4(aPos, 0.0, 1.0); }`;

  const FRAG = `
precision mediump float;
varying vec2 vUv;
uniform float uAspect;
uniform vec4 uOrbit[${N}]; // the four angles of each source's path, wrapped to one turn
uniform vec4 uWave;      // the four angles of the bending waves, wrapped likewise
uniform float uPulse;    // 0..1 smoothed bass
uniform vec3 uColor[${N}];
uniform float uWeight[${N}];

// Where a source is: a loop made of two circular motions per axis with unrelated
// periods, so it never visibly repeats. The angles are advanced on the CPU.
vec2 source(vec4 o) {
  return vec2(0.58 * sin(o.x) + 0.24 * sin(o.y), 0.46 * cos(o.z) + 0.22 * cos(o.w));
}

float grain(vec2 p) { return fract(sin(dot(p, vec2(41.31, 289.17))) * 2731.77); }

void main() {
  vec2 p = (vUv - 0.5) * vec2(uAspect, 1.0);

  // bend the plane: a broad wave, then a finer one riding on the result
  vec2 q = p;
  q += (0.20 + 0.05 * uPulse) * vec2(sin(q.y * 1.55 + uWave.x), cos(q.x * 1.25 - uWave.y));
  q += 0.085 * vec2(sin(q.y * 3.4 - uWave.z), cos(q.x * 2.9 + uWave.w));

  vec3 col = vec3(0.0);
  float total = 0.0;
  for (int i = 0; i < ${N}; i++) {
    vec2 c = source(uOrbit[i]) * vec2(uAspect * 0.62, 1.0);
    float d = length(q - c);
    // influence falls off steeply, so each colour owns a region with soft borders;
    // the bass lets every region reach a little further
    float w = uWeight[i] / (pow(d, 3.1) + 0.022 - 0.008 * uPulse);
    col += uColor[i] * w;
    total += w;
  }
  col /= total;

  // gentle lift toward the middle on the beat, a little darker at the rim always
  float rim = dot(p, p);
  col *= 1.0 + uPulse * 0.22 * exp(-rim * 2.2);
  col *= 1.0 - 0.16 * smoothstep(0.25, 1.1, rim);

  // richer, never milky
  float l = dot(col, vec3(0.2126, 0.7152, 0.0722));
  col = mix(vec3(l), col, 1.12);

  col += (grain(gl_FragCoord.xy) - 0.5) / 255.0 * 1.5; // hides banding in the smooth areas
  gl_FragColor = vec4(clamp(col, 0.0, 1.0), 1.0);
}`;

  let gl = null, loc = null, tried = false, usable = false;
  // Every moving part is an angle that turns at its own rate. They are advanced
  // here and wrapped to one turn, so the motion stays smooth however long the
  // wallpaper has been running (a single ever-growing time value would lose
  // precision on the GPU after some days).
  const TURN = Math.PI * 2;
  const rates = [], angles = new Float32Array(N * 4 + 4);
  for (let i = 0; i < N; i++) rates.push(0.21 + 0.047 * i, 0.22 + 0.053 * i, 0.174 + 0.039 * i, 0.13 + 0.031 * i);
  rates.push(0.37, 0.29, 0.23, 0.31);
  for (let i = 0; i < angles.length; i++) angles[i] = Math.random() * TURN; // every start looks different
  let pulse = 0;
  let coverUrl = null, coverTicket = 0;

  // colours on screen, and where they are heading
  const start = Palette.neutral();
  const shown = { field: start.field.map((c) => c.slice()), weight: weightsFrom(start.share) };
  let goal = { field: start.field, weight: weightsFrom(start.share) };
  const flat = new Float32Array(N * 3), flatW = new Float32Array(N);

  // Area share -> influence. Compressed, so a cover that is 80 % one colour still
  // shows its other colours.
  function weightsFrom(share) { return share.map((s) => 0.8 + 0.6 * Math.sqrt(Math.max(0, s))); }

  function compile(type, src) {
    const sh = gl.createShader(type);
    gl.shaderSource(sh, src);
    gl.compileShader(sh);
    if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(sh) || "shader");
    return sh;
  }

  function setup() {
    // ?nogl in the URL forces the CSS stand-in, to check how it looks on a machine that has WebGL.
    if (new URLSearchParams(location.search).has("nogl")) throw new Error("webgl switched off");
    gl = canvas.getContext("webgl", { alpha: false, antialias: false, depth: false, stencil: false, powerPreference: "low-power", preserveDrawingBuffer: false });
    if (!gl) throw new Error("no webgl");
    const prog = gl.createProgram();
    gl.attachShader(prog, compile(gl.VERTEX_SHADER, VERT));
    gl.attachShader(prog, compile(gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog) || "link");
    gl.useProgram(prog);
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW); // one triangle covering the view
    const aPos = gl.getAttribLocation(prog, "aPos");
    gl.enableVertexAttribArray(aPos);
    gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, 0, 0);
    loc = {
      aspect: gl.getUniformLocation(prog, "uAspect"),
      orbit: gl.getUniformLocation(prog, "uOrbit[0]"),
      wave: gl.getUniformLocation(prog, "uWave"),
      pulse: gl.getUniformLocation(prog, "uPulse"),
      color: gl.getUniformLocation(prog, "uColor[0]"),
      weight: gl.getUniformLocation(prog, "uWeight[0]"),
    };
    resize();
  }

  function init() {
    if (tried) return usable;
    tried = true;
    try { setup(); usable = true; } catch (e) { gl = null; usable = false; }
    if (usable) {
      canvas.addEventListener("webglcontextlost", (e) => { e.preventDefault(); gl = null; });
      canvas.addEventListener("webglcontextrestored", () => { try { setup(); } catch (e) { gl = null; } });
    }
    paintFallback();
    return usable;
  }

  // A few hundred pixels across is plenty for a field this smooth.
  function resize() {
    if (!gl) return;
    const W = Math.max(1, window.innerWidth), H = Math.max(1, window.innerHeight);
    const k = Math.min(1, 520 / Math.max(W, H));
    const w = Math.max(2, Math.round(W * k)), h = Math.max(2, Math.round(H * k));
    if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h; }
    gl.viewport(0, 0, w, h);
    gl.uniform1f(loc.aspect, w / h);
  }

  const css = (c, a) => `rgb(${Math.round(c[0] * 255)} ${Math.round(c[1] * 255)} ${Math.round(c[2] * 255)}${a === undefined ? "" : " / " + a})`;

  // No WebGL: the same colours as stacked soft gradients on #dynCss.
  function paintFallback() {
    if (usable || !cssFallback) return;
    const f = goal.field;
    cssFallback.style.backgroundColor = css(f[0]);
    cssFallback.style.backgroundImage = [
      `radial-gradient(60% 55% at 28% 30%, ${css(f[1])}, transparent 70%)`,
      `radial-gradient(55% 60% at 74% 34%, ${css(f[2])}, transparent 70%)`,
      `radial-gradient(60% 55% at 66% 76%, ${css(f[3])}, transparent 70%)`,
      `radial-gradient(50% 50% at 24% 74%, ${css(f[4])}, transparent 70%)`,
    ].join(",");
  }

  function usePalette(p) {
    goal = { field: p.field, weight: weightsFrom(p.share) };
    const root = document.documentElement.style;
    const triple = (c) => c.map((v) => Math.round(v * 255)).join(" ");
    root.setProperty("--accent", triple(p.accent));
    root.setProperty("--accent-text", triple(p.accentText));
    paintFallback();
  }

  // Same URL again is free; when covers arrive quickly the newest one wins. A cover
  // whose pixels cannot be read leaves the current colours in place.
  function setCover(url) {
    if (!url || url === coverUrl) return;
    coverUrl = url;
    const ticket = ++coverTicket;
    return loadImage(url, true).then((img) => {
      if (ticket !== coverTicket) return;
      const p = Palette.fromImage(img);
      if (p) usePalette(p);
    }).catch(() => {});
  }

  function frame(dt, speed, level) {
    // bass envelope: up fast, down slowly, so a kick reads as one breath not a flicker
    const lv = Math.min(1, Math.max(0, level || 0));
    pulse += (lv - pulse) * (1 - Math.exp(-dt / (lv > pulse ? 0.045 : 0.34)));
    const step = dt * (speed || 1) * (0.55 + 0.9 * pulse);
    for (let i = 0; i < angles.length; i++) angles[i] = (angles[i] + step * rates[i]) % TURN;

    // colours drift toward the new cover's
    const a = 1 - Math.exp(-dt / 0.9);
    for (let i = 0; i < N; i++) {
      const s = shown.field[i], g = goal.field[i];
      s[0] += (g[0] - s[0]) * a; s[1] += (g[1] - s[1]) * a; s[2] += (g[2] - s[2]) * a;
      shown.weight[i] += (goal.weight[i] - shown.weight[i]) * a;
      flat[i * 3] = s[0]; flat[i * 3 + 1] = s[1]; flat[i * 3 + 2] = s[2];
      flatW[i] = shown.weight[i];
    }
    if (!gl || document.hidden) return;
    gl.uniform4fv(loc.orbit, angles.subarray(0, N * 4));
    gl.uniform4fv(loc.wave, angles.subarray(N * 4));
    gl.uniform1f(loc.pulse, pulse);
    gl.uniform3fv(loc.color, flat);
    gl.uniform1fv(loc.weight, flatW);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }

  return { init, resize, setCover, frame, get ready() { return usable; } };
})();
