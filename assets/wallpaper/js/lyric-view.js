// The lyric engine: turns a lyrics payload into rows and animates them against the
// playback position. The look and feel come from a preset in lyric-styles.js and from
// lyrics.css; this file holds no styling numbers of its own.
//
// How it works
//  - Every animated value is a pure function of the song position. Nothing is
//    integrated frame to frame except the scroll, so seeking, pausing and looping
//    need no special cases: the frame after a seek simply draws the new moment.
//  - A frame only touches the rows that are being sung (a handful of elements) and
//    writes a style only when its value changed. Geometry is read in one pass when
//    the size or the text changes (measure()), never while animating.
//  - Rows keep the class names the page layouts and the click handling rely on:
//      .row.lyric[data-start]  one lyric line (plus .opp for the second singer)
//        .line                 the main text      .line.lx-sub   a background vocal
//      .row.lx-gap > .dotGroup instrumental break indicator
//    with .act while a row is being sung and .sung afterwards. Everything below
//    .line is private to this renderer (.lx-w word, .lx-u timed unit, .lx-c letter).
"use strict";

const LyricsView = (() => {
  const host = document.getElementById("lyrics");
  const content = document.getElementById("lyricsContent");
  host.classList.add("lx");

  const opts = { blur: true, linePos: 0.45 };
  let style = null;      // preset from LyricStyles
  let moves = true;      // the preset moves ordinary units at all
  let source = null;     // payload on screen
  let rows = [];         // gap and lyric rows, in time order
  const live = new Set(); // rows carrying inline animation state

  let started = -1;      // last row whose start has passed
  let focus = -1;        // row the list is centred on
  let visLo = 0, visHi = -1; // rows currently allowed to paint
  let openGap = null;    // gap row that is unfolded
  let lastT = 0;

  let dirty = true;      // geometry is stale
  const geo = { hostH: 0, gapH: 0, span: 12 };
  let y = 0, yLag = 0, yDrawn = NaN, snap = true;

  // ---------------- curves ----------------
  const unit01 = (v) => (v < 0 ? 0 : v > 1 ? 1 : v);
  const ease = (x) => x * x * (3 - 2 * x);                 // slow in, slow out
  const settle = (x) => 1 - (1 - x) * (1 - x) * (1 - x);   // fast in, slow out
  const overshoot = (x) => settle(x) + 1.6 * x * (1 - x) * (1 - x); // passes 1, comes back
  const CURVES = { soft: settle, snap: overshoot };

  // ---------------- text ----------------
  const RTL = /[֐-ࣿיִ-﷿ﹰ-ﻼ]/;
  // Scripts whose letters stand alone, so a word can be split into letter boxes
  // without breaking joins or ligatures.
  const SPLITTABLE = /^[\p{Script=Latin}\p{Script=Cyrillic}\p{Script=Greek}\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Hangul}\p{N}\p{P}\s]+$/u;
  const CJK = /[぀-ヿ㐀-鿿豈-﫿가-힯]/;

  // Untimed text -> words. CJK has no spaces, so each character is its own word and
  // long lines can still wrap.
  function splitWords(text) {
    const out = [];
    for (const chunk of text.trim().split(/\s+/)) {
      if (!chunk) continue;
      if (!CJK.test(chunk)) { out.push({ text: chunk, space: true }); continue; }
      const parts = chunk.match(/[぀-ヿ㐀-鿿豈-﫿가-힯][、。，．！？!?,.」』）)]*|[^぀-ヿ㐀-鿿豈-﫿가-힯]+/g) || [chunk];
      parts.forEach((p, i) => out.push({ text: p, space: i === parts.length - 1 }));
    }
    return out;
  }

  // Timed syllables -> words of units. A syllable flagged PartOfWord continues into
  // the next one.
  function wordsFromSylls(sylls) {
    const words = [];
    let word = [];
    for (const s of sylls) {
      const text = String(s.Text || "").replace(/\s+$/, "");
      if (text) word.push({ start: s.Start, end: Math.max(s.End, s.Start + 1), text, synth: false });
      if (!s.PartOfWord && word.length) { words.push({ units: word, space: true }); word = []; }
    }
    if (word.length) words.push({ units: word, space: true });
    return words;
  }

  // No word timing: sweep across the line evenly, by length, so it fills as it is
  // sung instead of lighting all at once. A line-synced line usually runs until the
  // next one starts, through any instrumental after it, so the sweep is held to a singing pace
  // (`msPerChar`) rather than stretched over a long instrumental tail.
  const SWEEP_MS_PER_CHAR = 170;
  function wordsEven(text, start, end, msPerChar) {
    const parts = splitWords(text);
    const chars = parts.reduce((n, p) => n + p.text.length, 0) || 1;
    if (msPerChar) end = Math.min(end, start + Math.max(1200, chars * msPerChar));
    let at = start;
    return parts.map((p) => {
      const d = ((end - start) * p.text.length) / chars;
      const u = { start: at, end: at + Math.max(1, d), text: p.text, synth: true };
      at += d;
      return { units: [u], space: p.space };
    });
  }

  // ---------------- building ----------------
  const el = (tag, cls) => { const e = document.createElement(tag); e.className = cls; return e; };

  function holdStrength(u) {
    const h = style.hold, dur = u.end - u.start;
    if (u.synth || dur < h.minMs) return 0;
    const n = Array.from(u.text).length;
    if (n < 1 || n > h.maxChars || !SPLITTABLE.test(u.text)) return 0;
    return 0.4 + 0.6 * ease(unit01((dur - h.minMs) / Math.max(1, h.fullMs - h.minMs)));
  }

  function buildLine(words, cls, text, sink) {
    const line = el("div", cls);
    if (RTL.test(text)) line.dir = "rtl";
    words.forEach((w, wi) => {
      const wEl = el("span", "lx-w");
      for (const u of w.units) {
        const uEl = el("span", "lx-u");
        u.hold = holdStrength(u);
        u.chars = null;
        if (u.hold > 0) {
          uEl.classList.add("lx-held");
          const letters = Array.from(u.text), n = letters.length, dur = u.end - u.start;
          u.chars = letters.map((ch, i) => {
            const c = el("span", "lx-c");
            c.textContent = ch;
            uEl.appendChild(c);
            return { el: c, start: u.start + (dur * i) / n, len: dur / n, off: i - (n - 1) / 2, k: -1, e: -1 };
          });
        } else {
          uEl.textContent = u.text;
        }
        u.el = uEl; u.k = -1; u.e = -1;
        sink.push(u);
        wEl.appendChild(uEl);
      }
      line.appendChild(wEl);
      if (w.space && wi < words.length - 1) line.appendChild(document.createTextNode(" "));
    });
    return line;
  }

  function build() {
    content.textContent = "";
    rows = []; live.clear();
    started = -1; focus = -1; visLo = 0; visHi = -1; openGap = null;
    dirty = true; snap = true;
    const src = source;
    if (!src || !Array.isArray(src.Lines) || !src.Lines.length) return;

    const lines = src.Lines.slice().sort((a, b) => a.Start - b.Start);
    const lyricRows = lines.map((l) => {
      const row = { gap: false, start: l.Start, end: Math.max(l.End, l.Start + 1), opp: !!l.Opposite, units: [], el: null, top: 0, h: 0, st: 0, d: -1, rtl: false };
      const text = String(l.Text || "");
      const words = l.Sylls && l.Sylls.length ? wordsFromSylls(l.Sylls) : wordsEven(text, l.Start, row.end, SWEEP_MS_PER_CHAR);
      row.el = el("div", "row lyric lx-off" + (row.opp ? " opp" : ""));
      row.el.dataset.start = String(Math.round(l.Start));
      row.rtl = RTL.test(text);
      row.el.appendChild(buildLine(words, "line", text, row.units));
      return row;
    });

    // A background vocal belongs to the line that was last started when it begins.
    for (const b of (src.Bg || []).slice().sort((a, b2) => a.Start - b2.Start)) {
      let i = 0;
      while (i + 1 < lyricRows.length && lyricRows[i + 1].start <= b.Start + 1) i++;
      const row = lyricRows[i], text = String(b.Text || "");
      const end = Math.max(b.End, b.Start + 1);
      const words = b.Sylls && b.Sylls.length ? wordsFromSylls(b.Sylls) : wordsEven(text, b.Start, end);
      row.el.appendChild(buildLine(words, "line lx-sub", text, row.units));
      row.start = Math.min(row.start, b.Start);
      row.end = Math.max(row.end, end);
    }
    for (const row of lyricRows) for (const u of row.units) row.end = Math.max(row.end, u.end);

    // Instrumental breaks: before the first line and between lines.
    const g = style.gap;
    const gapRow = (from, to, next) => {
      const row = { gap: true, start: from, end: to, opp: next.opp, units: null, el: null, top: 0, h: 0, st: 0, d: -1, ind: null, k: -1 };
      row.el = el("div", "row lx-gap lx-off" + (next.opp ? " opp" : ""));
      const group = el("div", "dotGroup");
      row.ind = el("span", "lx-gapind");
      row.ind.innerHTML = '<span class="lx-bar"><i></i></span><span class="lx-head"><i></i></span>';
      group.appendChild(row.ind);
      row.el.appendChild(group);
      return row;
    };
    let prevEnd = 0;
    lyricRows.forEach((row, i) => {
      const from = i === 0 ? 0 : prevEnd + 250;
      if (row.start - from >= g.minMs) rows.push(gapRow(from, row.start, row));
      rows.push(row);
      prevEnd = Math.max(prevEnd, row.end);
    });

    // Rank among lyric lines, for "how many lines away"; a gap row shares the rank
    // of the line that follows it.
    let ord = 0;
    rows.forEach((r, i) => { r.index = i; r.ord = ord; if (!r.gap) ord++; });

    const frag = document.createDocumentFragment();
    for (const r of rows) frag.appendChild(r.el);
    content.appendChild(frag);
  }

  // ---------------- geometry ----------------
  function originOf(node) {
    const cs = getComputedStyle(node), rtl = cs.direction === "rtl";
    switch (cs.textAlign) {
      case "center": case "-webkit-center": return "center";
      case "right": case "-webkit-right": return "right";
      case "left": case "-webkit-left": return "left";
      case "end": return rtl ? "left" : "right";
      default: return rtl ? "right" : "left";
    }
  }

  // One read pass. Row tops are stored as they are with every gap row folded; the
  // scroll adds the open one back in itself, so a measurement taken while a gap row
  // is mid-transition is still right.
  function measure() {
    geo.hostH = host.clientHeight;
    if (!geo.hostH || !rows.length) return;
    dirty = false;
    geo.gapH = (parseFloat(getComputedStyle(host).fontSize) || 16) * GAP_EM;
    let folded = 0, minH = Infinity, plain = null, opp = null;
    const rtlRows = [];
    for (const r of rows) {
      const h = r.el.offsetHeight;
      r.top = r.el.offsetTop - folded;
      if (r.gap) { folded += h; continue; }
      r.h = h;
      if (h > 0 && h < minH) minH = h;
      if (r.rtl) rtlRows.push([r, originOf(r.el.firstChild)]);
      else if (r.opp) { if (!opp) opp = originOf(r.el.firstChild); }
      else if (!plain) plain = originOf(r.el.firstChild);
    }
    // writes, after all reads
    host.style.setProperty("--lx-ox", plain || "left");
    host.style.setProperty("--lx-oxo", opp || "right");
    for (const [r, o] of rtlRows) { r.el.style.setProperty("--lx-ox", o); r.el.style.setProperty("--lx-oxo", o); }
    geo.span = Math.max(8, Math.min(48, Math.ceil(geo.hostH / (Number.isFinite(minH) ? minH : 40)) + 3));
    if (focus >= 0) window_(focus);
  }

  // ---------------- focus: dimming by distance + paint culling ----------------
  function setDistance(r, d) {
    if (r.d === d) return;
    r.d = d;
    if (d >= style.focus.reach) r.el.style.removeProperty("--lx-d");
    else r.el.style.setProperty("--lx-d", d);
  }

  // Rows far outside the view are not painted at all.
  function window_(f) {
    const lo = Math.max(0, f - geo.span), hi = Math.min(rows.length - 1, f + geo.span);
    for (let i = visLo; i <= visHi; i++) if (i < lo || i > hi) rows[i].el.classList.add("lx-off");
    for (let i = lo; i <= hi; i++) if (i < visLo || i > visHi) rows[i].el.classList.remove("lx-off");
    visLo = lo; visHi = hi;
  }

  function setFocus(f) {
    const prev = focus;
    focus = f;
    const reach = style.focus.reach, pad = reach * 2 + 2, base = rows[f].ord;
    const lo = Math.max(0, Math.min(prev < 0 ? f : prev, f) - pad);
    const hi = Math.min(rows.length - 1, Math.max(prev < 0 ? f : prev, f) + pad);
    for (let i = lo; i <= hi; i++) setDistance(rows[i], Math.min(reach, Math.abs(rows[i].ord - base)));
    window_(f);
  }

  // ---------------- per-row animation ----------------
  function setState(r, st) {
    if (r.st === st) return;
    r.st = st;
    const c = r.el.classList;
    c.toggle("act", st === 1);
    c.toggle("sung", st === 2);
    if (r.gap) openGap = st === 1 ? r : (openGap === r ? null : openGap);
    if (st === 1) live.add(r);
  }

  function paintUnit(u, t) {
    const U = style.unit;
    if (u.chars) { paintHeld(u, t); return; }
    const k = Math.round(unit01((t - u.start) / (u.end - u.start)) * 400) / 400;
    if (k !== u.k) { u.k = k; u.el.style.setProperty("--lx-k", k); }
    if (!moves) return;
    let e = 0;
    if (t >= u.start) e = curve(unit01((t - u.start) / U.attackMs)) * (1 - ease(unit01((t - u.end) / U.releaseMs)));
    e = Math.round(e * 250) / 250;
    if (e !== u.e) {
      u.e = e;
      // 3D, so the unit is moved by the compositor: a 2D transform re-rasterises the
      // text, which snaps to whole pixels and makes a 2-3 px lift move in visible
      // steps. Kept (at 0) until the row retires, so the layer isn't created and
      // dropped around every word.
      u.el.style.transform = `translate3d(0, ${(-U.lift * e).toFixed(4)}em, 0) scale(${(1 + U.grow * e).toFixed(4)})`;
    }
  }

  // A held syllable: the sweep walks through its letters, and each letter it has
  // reached swells, lifts and drifts away from the middle of the word until the
  // note ends, with a slow waver while it lasts.
  function paintHeld(u, t) {
    const H = style.hold, dur = u.end - u.start, s = u.hold;
    const rise = Math.min(900, Math.max(260, dur * 0.45));
    const fall = 1 - ease(unit01((t - u.end) / (style.unit.releaseMs * 1.3)));
    for (const c of u.chars) {
      const k = Math.round(unit01((t - c.start) / c.len) * 200) / 200;
      if (k !== c.k) { c.k = k; c.el.style.setProperty("--lx-k", k); }
      let e = 0;
      if (t >= c.start) {
        const a = settle(unit01((t - c.start) / rise));
        const waver = t < u.end ? 1 + 0.07 * Math.sin((t - c.start) * 0.0105) * a : 1;
        e = a * fall * waver * s;
      }
      e = Math.round(e * 250) / 250;
      if (e !== c.e) {
        c.e = e;
        const st = c.el.style;
        st.transform = `translate3d(${(c.off * H.spread * e).toFixed(4)}em, ${(-H.lift * e).toFixed(4)}em, 0) scale(${(1 + H.grow * e).toFixed(4)})`;
        st.setProperty("--lx-g", (H.glow * e).toFixed(3));
      }
    }
  }

  function paintGap(r, t) {
    const span = Math.max(1, r.end - style.gap.outMs - r.start);
    const k = Math.round(unit01((t - r.start) / span) * 500) / 500;
    if (k !== r.k) { r.k = k; r.ind.style.setProperty("--lx-k", k); }
  }

  // Back to what the row's class says it looks like.
  function retire(r) {
    live.delete(r);
    if (r.gap) return;
    for (const u of r.units) {
      if (u.k !== -1) { u.k = -1; u.el.style.removeProperty("--lx-k"); }
      if (u.e !== -1) { u.e = -1; u.el.style.transform = ""; }
      if (u.chars) for (const c of u.chars) {
        if (c.k !== -1) { c.k = -1; c.el.style.removeProperty("--lx-k"); }
        if (c.e !== -1) { c.e = -1; c.el.style.transform = ""; c.el.style.removeProperty("--lx-g"); }
      }
    }
  }

  // ---------------- frame ----------------
  const GAP_EM = 1.7;       // unfolded height of an interlude row; lyrics.css reads it as --lx-gap-h
  const LOOK_BACK = 4;      // rows behind the newest one that can still be mid-line (overlapping singers)
  let curve = settle;
  let tail = 400;           // ms a finished row keeps animating (its last unit settling)

  function stateAt(r, t) {
    if (t < r.start) return 0;
    if (r.gap) return t < r.end - style.gap.outMs ? 1 : 2;
    return t < r.end ? 1 : 2;
  }

  function frame(t, dt) {
    const n = rows.length;
    if (!n) return;
    if (dirty) measure();
    const jumped = Math.abs(t - lastT) > 1200;
    lastT = t;

    // newest row that has started
    let i = Math.min(started, n - 1);
    while (i + 1 < n && rows[i + 1].start <= t) i++;
    while (i >= 0 && rows[i].start > t) i--;
    const from = Math.max(0, Math.min(i, started) - LOOK_BACK), to = Math.min(n - 1, Math.max(i, started) + 1);
    started = i;
    for (let j = from; j <= to; j++) setState(rows[j], stateAt(rows[j], t));

    for (const r of live) {
      if (t < r.start || t >= r.end + tail) { retire(r); continue; }
      if (r.gap) paintGap(r, t);
      else for (const u of r.units) paintUnit(u, t);
    }

    // which row to centre: the newest, a little ahead of time; a folded gap hands over to the next line
    let f = i;
    if (f + 1 < n && rows[f + 1].start - style.scroll.leadMs <= t) f++;
    if (f < 0) f = 0;
    if (rows[f].gap && rows[f].st !== 1 && t >= rows[f].start && f + 1 < n) f++;
    if (f !== focus) setFocus(f);

    if (!geo.hostH) return;
    const r = rows[f];
    let top = r.top;
    if (openGap && openGap.index < f) top += geo.gapH;
    const target = opts.linePos * geo.hostH - (top + (r.gap ? geo.gapH : r.h) * 0.5);
    if (snap) {
      y = yLag = target; snap = false;
    } else {
      if (jumped && Math.abs(target - y) > geo.hostH * 1.5) {
        // a long seek: arrive from just outside the view instead of flying past every line
        y = yLag = target + Math.sign(y - target) * geo.hostH * 0.6;
      }
      // two smoothing stages in a row: eases out of rest and into the target, never overshoots
      const a = 1 - Math.exp(-dt / style.scroll.settle);
      yLag += (target - yLag) * a;
      y += (yLag - y) * a;
      if (Math.abs(target - y) < 0.05) y = yLag = target;
    }
    if (y !== yDrawn) {
      yDrawn = y;
      content.style.transform = `translate3d(0, ${y.toFixed(2)}px, 0)`;
    }
  }

  // ---------------- options ----------------
  function useStyle(id) {
    const next = LyricStyles.get(id);
    if (style && next.id === style.id) return false;
    if (style) host.classList.remove("lx-style-" + style.id);
    style = next;
    host.classList.add("lx-style-" + style.id);
    host.dataset.lyricStyle = style.id;
    moves = style.unit.lift !== 0 || style.unit.grow !== 0;
    curve = CURVES[style.unit.curve] || settle;
    tail = Math.max(style.unit.releaseMs * 1.3, 200);
    const hs = host.style;
    hs.setProperty("--lx-feather", style.fill.feather + "em");
    hs.setProperty("--lx-reach", style.focus.reach);
    hs.setProperty("--lx-gap-h", GAP_EM + "em");
    for (const k of Object.keys(style.vars)) hs.setProperty(k, style.vars[k]);
    return true;
  }
  useStyle("");

  if (typeof ResizeObserver === "function") {
    const ro = new ResizeObserver(() => { dirty = true; });
    ro.observe(host);
    ro.observe(content);
  } else {
    window.addEventListener("resize", () => { dirty = true; });
  }
  if (document.fonts && document.fonts.ready) document.fonts.ready.then(() => { dirty = true; });

  return {
    set(lyrics) { source = lyrics || null; build(); },
    setOptions(o) {
      if (o.blur !== undefined) { opts.blur = !!o.blur; host.classList.toggle("lx-noblur", !opts.blur); }
      if (o.linePos !== undefined && Number.isFinite(Number(o.linePos))) opts.linePos = Number(o.linePos);
      if (o.style !== undefined && useStyle(o.style)) build();
      dirty = true; // the layout may have changed along with the settings
    },
    frame,
    get hasLyrics() { return rows.length > 0; },
    get style() { return style.id; },
  };
})();
