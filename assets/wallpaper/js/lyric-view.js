// The lyric renderer: a port of the Android app's (github.com/kamol1dn/syllable-lyrics,
// ui/LyricMotion.kt and ui/LyricsView.kt), so the wallpaper moves exactly as the phone does.
//
//  - Every sung word, held letter and interlude dot has a target that follows how far
//    through it the song is (a natural cubic spline over its own progress) and a damped
//    spring that chases the target, stepped exactly for the frame time.
//  - Sung text is white at LIT, text still to come at UNLIT, with a soft edge FADE wide
//    between them; whole lines sit at LINE_SUNG / LINE_NOT_SUNG on top of that, and blur
//    with their distance from the current line.
//  - Scrolling is a kick: the list jumps to the new line at once, every row is drawn where
//    it was and springs back, each a little after the row above, so the lyrics move as a
//    soft wave.
//  - Line-timed lines are not split into guessed words: the whole line grows a little and
//    fills from top to bottom over its length. Long instrumental gaps show three dots.
//
// Numbers in dp/sp in the Kotlin code are converted with the phone's 30 sp text size as
// 1 em, so everything scales with the wallpaper's lyric size.
//
// Rows keep the class names the page layouts and the click handling rely on:
//   .row.lyric[data-start]  one lyric line (plus .opp for the second singer)
//     .line                 the main text      .line.lx-sub   background vocals
//   .row.lx-gap > .dotGroup instrumental break
// with .act while a row is current and .sung once it has been sung. Everything else
// (.lx-g word group, .lx-u syllable, .lx-c letter of a held word) is private.
"use strict";

const LyricsView = (() => {
  const host = document.getElementById("lyrics");
  const content = document.getElementById("lyricsContent");
  host.classList.add("lx");

  // ---------------- LyricMotion.kt ----------------
  const LIT = 0.85, UNLIT = 0.35;          // sung / still to come, as white at this alpha
  const LINE_NOT_SUNG = 0.51, LINE_SUNG = 0.497;
  const FADE = 0.2;                         // soft edge, share of the word (or of the line's height)
  const EMPHASIS_MS = 1000;                 // a syllable held this long is drawn letter by letter
  const EMPHASIS_TAIL_MS = 250;             // its letters finish this long before it does
  const INTERLUDE_MS = 3000;                // a gap this long between lines shows dots
  const DOTS_END_EARLY_MS = 550, DOTS_HIDE_EARLY_MS = 500;
  const REST_SCALE = 0.95, REST_LIFT = 0.01;
  const SUNG_LETTER_GLOW = 0.2;
  const SETTLE_MS = 900;                    // a finished line keeps animating this long
  const STAGGER_S = 0.042, KICK_LIFETIME_S = 2.5;
  const DP = 1 / 30;                        // em per dp/sp (the phone's lyrics are 30 sp)

  /** Natural cubic spline through a few points, read between its first and last x. */
  function spline(points) {
    const xs = points.map((p) => p[0]), ys = points.map((p) => p[1]), n = xs.length;
    const m = new Float64Array(n);
    if (n > 2) {
      const c = new Float64Array(n), d = new Float64Array(n);
      for (let i = 1; i < n - 1; i++) {
        const h0 = xs[i] - xs[i - 1], h1 = xs[i + 1] - xs[i];
        const diag = 2 * (h0 + h1) - h0 * c[i - 1];
        const rhs = 6 * ((ys[i + 1] - ys[i]) / h1 - (ys[i] - ys[i - 1]) / h0);
        c[i] = h1 / diag;
        d[i] = (rhs - h0 * d[i - 1]) / diag;
      }
      for (let i = n - 2; i >= 1; i--) m[i] = d[i] - c[i] * m[i + 1];
    }
    return (x) => {
      const t = Math.min(xs[n - 1], Math.max(xs[0], x));
      let i = 0;
      while (i < n - 2 && t > xs[i + 1]) i++;
      const h = xs[i + 1] - xs[i], a = (xs[i + 1] - t) / h, b = (t - xs[i]) / h;
      return a * ys[i] + b * ys[i + 1] + ((a * a * a - a) * m[i] + (b * b * b - b) * m[i + 1]) * h * h / 6;
    };
  }

  /** A damped spring, stepped exactly for any frame time. `hz` is its natural frequency. */
  class Spring {
    constructor(start, hz, damping) {
      this.w = hz * 2 * Math.PI;
      this.damping = damping;
      this.velocity = 0;
      this.value = start;
      this.goal = start;
    }
    snap(to) { this.value = to; this.goal = to; this.velocity = 0; }
    step(dt) {
      if (dt <= 0) return this.value;
      const w = this.w, z = this.damping, o = this.value - this.goal;
      if (z >= 1) {
        const e = Math.exp(-w * dt), k = this.velocity + w * o;
        this.value = this.goal + (o + k * dt) * e;
        this.velocity = (this.velocity - k * w * dt) * e;
      } else {
        const wd = w * Math.sqrt(1 - z * z), e = Math.exp(-z * w * dt);
        const c = Math.cos(wd * dt), s = Math.sin(wd * dt);
        const k = (this.velocity + z * w * o) / wd;
        this.value = this.goal + e * (o * c + k * s);
        this.velocity = e * (this.velocity * c - (o * wd + z * w * k) * s);
      }
      return this.value;
    }
  }

  const WordScale = spline([[0, 0.95], [0.7, 1.0505], [1, 1]]);
  const WordLift = spline([[0, 0.01], [0.9, -1 / 60], [1, 0]]);
  const WordGlow = spline([[0, 0], [0.15, 1], [0.6, 1], [1, 0]]);
  const LetterScale = spline([[0, 0.95], [0.7, 1.175], [1, 1]]);
  const LetterLift = spline([[0, 0.01], [0.9, -1 / 56], [1, 0]]);
  const DotScale = spline([[0, 0.75], [0.7, 1.05], [1, 1]]);
  const DotLift = spline([[0, 0], [0.9, -0.12], [1, 0]]);
  const DotOpacity = spline([[0, 0.35], [0.6, 1], [1, 1]]);

  const progress = (p, start, end) => (p <= start ? 0 : p >= end ? 1 : (p - start) / (end - start));
  const fillAt = (pct) => -FADE + (1 + FADE) * pct;

  /** One word or syllable: swells, lifts, glows through the middle, fills left to right. */
  class WordMotion {
    constructor(start, end) {
      this.start = start; this.end = end;
      this.scale = new Spring(REST_SCALE, 0.88, 0.64);
      this.lift = new Spring(REST_LIFT, 1.45, 0.4);
      this.glow = new Spring(0, 1.18, 0.56);
      this.fill = -FADE;
      this.fresh = true;
    }
    step(p, dt) {
      const pct = progress(p, this.start, this.end), sung = p >= this.end;
      this.fill = fillAt(pct);
      this.scale.goal = sung ? 1 : WordScale(pct);
      this.lift.goal = sung ? 0 : WordLift(pct);
      this.glow.goal = sung ? 0 : WordGlow(pct);
      if (this.fresh) {
        // First frame on screen: a word already sung starts at rest instead of springing up.
        if (sung) { this.scale.snap(1); this.lift.snap(0); this.glow.snap(0); }
        this.fresh = false;
      } else {
        this.scale.step(dt); this.lift.step(dt); this.glow.step(dt);
      }
    }
  }

  /** The letters of a held word: the one being sung swells, lifts and glows, its neighbours lean in. */
  class LetterMotion {
    constructor(start, end, count) {
      this.start = start; this.count = count; this.wordEnd = end;
      this.lettersEnd = Math.max(start + count, end - EMPHASIS_TAIL_MS);
      this.scale = Array.from({ length: count }, () => new Spring(REST_SCALE, 0.88, 0.64));
      this.lift = Array.from({ length: count }, () => new Spring(REST_LIFT, 1.45, 0.4));
      this.glow = Array.from({ length: count }, () => new Spring(0, 1.18, 0.56));
      this.fill = new Float64Array(count).fill(-FADE);
      this.fresh = true;
    }
    letterStart(i) { return this.start + Math.floor(((this.lettersEnd - this.start) * i) / this.count); }
    step(p, dt) {
      const n = this.count, wordSung = p >= this.wordEnd, wordActive = p > this.start && !wordSung;
      let active = -1, activePct = 0;
      if (wordActive) {
        for (let i = 0; i < n; i++) {
          const s = this.letterStart(i), e = this.letterStart(i + 1);
          if (p >= s && p < e) { active = i; activePct = progress(p, s, e); break; }
        }
      }
      const first = this.fresh;
      for (let i = 0; i < n; i++) {
        const letterSung = p >= this.letterStart(i + 1), letterWaiting = p < this.letterStart(i);
        let scale = REST_SCALE, lift = REST_LIFT, glow = 0;
        if (wordSung) { scale = 1; lift = 0; }
        else if (!wordActive || letterWaiting) { /* at rest */ }
        else if (active !== -1) {
          const d = Math.abs(i - active), near = 1 / (1 + Math.pow(d, 2.8));
          scale = REST_SCALE + (LetterScale(activePct) - REST_SCALE) * near;
          lift = REST_LIFT + (LetterLift(activePct) - REST_LIFT) * near;
          glow = WordGlow(activePct) / (1 + d * 0.9);
        } else if (letterSung) glow = WordGlow(SUNG_LETTER_GLOW);
        this.fill[i] = wordSung || letterSung ? 1 : i === active ? fillAt(Math.sin((activePct * Math.PI) / 2)) : -FADE;
        this.scale[i].goal = scale; this.lift[i].goal = lift; this.glow[i].goal = glow;
        if (first) {
          if (wordSung) { this.scale[i].snap(1); this.lift[i].snap(0); this.glow[i].snap(0); }
        } else {
          this.scale[i].step(dt); this.lift[i].step(dt); this.glow[i].step(dt);
        }
      }
      this.fresh = false;
    }
  }

  /** One of the three interlude dots: it pops up and brightens during its third of the gap. */
  class DotMotion {
    constructor(start, end) {
      this.start = start; this.end = end;
      this.scale = new Spring(DotScale(0), 0.7, 0.6);
      this.lift = new Spring(0, 1.25, 0.4);
      this.opacity = new Spring(DotOpacity(0), 1, 0.5);
      this.fresh = true;
    }
    step(p, dt) {
      const pct = progress(p, this.start, this.end);
      this.scale.goal = DotScale(pct); this.lift.goal = DotLift(pct); this.opacity.goal = DotOpacity(pct);
      if (this.fresh) {
        if (pct >= 1) { this.scale.snap(1); this.lift.snap(0); this.opacity.snap(1); }
        this.fresh = false;
      } else {
        this.scale.step(dt); this.lift.step(dt); this.opacity.step(dt);
      }
    }
  }

  /** The dot group's size as it leaves, t 0..1: a small swell, then down to nothing. */
  function dotsExitScale(t) {
    const x = Math.min(1, Math.max(0, t)), swell = 0.19, turn = 0.56;
    return x < turn ? 1 + swell * Math.sin(((x / turn) * Math.PI) / 2) : (1 + swell) * (1 - Math.pow((x - turn) / (1 - turn), 2));
  }

  /** Sung part at LIT, the rest at UNLIT, a soft edge at `fill` (0..1 across the text), as gradient stops. */
  function fillStops(fill) {
    if (fill >= 1) return [0, 0.0005, LIT, LIT];
    if (fill <= -FADE) return [0, 0.0005, UNLIT, UNLIT];
    const tone = (t) => LIT + (UNLIT - LIT) * Math.min(1, Math.max(0, t));
    const a = Math.min(0.999, Math.max(0, fill));
    const b = Math.max(a + 0.0005, Math.min(fill + FADE, 1));
    return [a, b, tone(-fill / FADE), tone((1 - fill) / FADE)];
  }

  // ---------------- text ----------------
  const RTL = /[֐-ࣿיִ-﷿ﹰ-ﻼ]/;
  const CJK = /[぀-ヿ㐀-鿿豈-﫿가-힯]/;
  const graphemes = (() => {
    const seg = typeof Intl !== "undefined" && Intl.Segmenter ? new Intl.Segmenter(undefined, { granularity: "grapheme" }) : null;
    return (text) => (seg ? Array.from(seg.segment(text), (s) => s.segment) : Array.from(text));
  })();

  /** Timed syllables -> words, each keeping its trailing space, as the phone's LyricWord does. */
  function wordsFromSylls(sylls) {
    const out = [];
    sylls.forEach((s, i) => {
      let text = String(s.Text ?? "");
      if (i === sylls.length - 1) text = text.replace(/\s+$/, ""); // the last one never carries a space
      else if (!s.PartOfWord && !/\s$/.test(text)) text += " ";
      if (!text) return;
      out.push({ text, start: s.Start, end: Math.max(s.End, s.Start + 1), joined: !!s.PartOfWord });
    });
    return out;
  }

  /** Joins the syllables of one word so a line only wraps between words; CJK stands alone. */
  function wordGroups(ws) {
    const groups = [];
    let cur = [];
    ws.forEach((w, i) => {
      cur.push(w);
      const last = w.text.slice(-1), next = ws[i + 1] ? ws[i + 1].text.charAt(0) : "";
      const boundary = !w.joined || !last || /\s/.test(last) || CJK.test(last) || !next || /\s/.test(next) || CJK.test(next);
      if (boundary) { groups.push(cur); cur = []; }
    });
    if (cur.length) groups.push(cur);
    return groups;
  }

  // ---------------- building (LyricsView.kt buildRows) ----------------
  const el = (tag, cls) => { const e = document.createElement(tag); if (cls) e.className = cls; return e; };

  let source = null;
  let rows = [];          // lines and gaps, in time order; plus `creditsStart`, when the last line hands over
  let creditsStart = Infinity;
  const opts = { blur: true, linePos: 0.45 };

  function buildFlow(words, cls, rtl) {
    const line = el("div", cls);
    if (rtl) line.dir = "rtl";
    const units = [];
    for (const g of wordGroups(words)) {
      const gEl = el("span", "lx-g");
      for (const w of g) {
        const body = w.text.replace(/\s+$/, ""), space = w.text.slice(body.length);
        const held = w.end - w.start >= EMPHASIS_MS && !RTL.test(w.text) && body.length > 0;
        const u = { start: w.start, end: w.end, el: el("span", "lx-u"), letters: null, motion: null, letterMotion: null, w: {} };
        if (held) {
          u.el.classList.add("lx-held");
          u.letters = graphemes(body).map((ch) => {
            const c = el("span", "lx-c");
            c.textContent = ch;
            c.dataset.t = ch;
            u.el.appendChild(c);
            return { el: c, w: {} };
          });
          if (space) u.el.appendChild(document.createTextNode(space));
        } else {
          u.el.textContent = w.text;
          u.el.dataset.t = w.text;
        }
        gEl.appendChild(u.el);
        units.push(u);
      }
      line.appendChild(gEl);
    }
    return { el: line, units };
  }

  function build() {
    content.textContent = "";
    rows = []; creditsStart = Infinity;
    resetView();
    const src = source;
    if (!src || !Array.isArray(src.Lines) || !src.Lines.length) return;

    const lines = src.Lines.slice().sort((a, b) => a.Start - b.Start).map((l) => {
      const words = l.Sylls && l.Sylls.length ? wordsFromSylls(l.Sylls) : null;
      const text = String(l.Text || "");
      let end = Math.max(l.End, l.Start + 1);
      if (words && words.length) end = Math.max(end, words[words.length - 1].end);
      return { start: l.Start, end, text, words, opp: !!l.Opposite, bg: [], rtl: RTL.test(text) };
    });
    // Background vocals belong to the line they were sung over. The phone keeps them with
    // their own lead; here they arrive separately, so take the latest line that has started
    // by the time they end and whose own end covers them (a lead's end includes its
    // background vocals), else the line last started when they begin.
    const rawEnd = src.Lines.slice().sort((a, b) => a.Start - b.Start).map((l) => l.End);
    for (const b of (src.Bg || []).slice().sort((a, b2) => a.Start - b2.Start)) {
      let i = 0;
      while (i + 1 < lines.length && lines[i + 1].start <= b.Start + 1) i++;
      for (let j = lines.length - 1; j >= 0; j--) {
        if (lines[j].start <= b.End && rawEnd[j] >= b.End - 50) { if (j >= i && j <= i + 1) i = j; break; }
      }
      const words = b.Sylls && b.Sylls.length
        ? wordsFromSylls(b.Sylls)
        : [{ text: String(b.Text || ""), start: b.Start, end: Math.max(b.End, b.Start + 1), joined: false }];
      const l = lines[i];
      if (l.bg.length && words.length) l.bg[l.bg.length - 1].text = l.bg[l.bg.length - 1].text.replace(/\s*$/, " ");
      l.bg.push(...words.filter((w) => w.text.trim()));
      for (const w of words) l.end = Math.max(l.end, w.end);
    }

    // When a line stops being sung. A line-timed line usually runs to the next one; over a
    // long instrumental that would keep it lit for half a minute, so it gets a reading time.
    const sungEnd = (l, next) => {
      if (l.words || !next || l.end < next.start) return l.end;
      const reading = l.start + Math.max(7000, l.text.length * 110);
      return next.start - reading >= INTERLUDE_MS ? reading : l.end;
    };

    if (lines[0].start >= INTERLUDE_MS) rows.push(gapRow(0, lines[0].start));
    lines.forEach((l, i) => {
      const next = lines[i + 1], end = sungEnd(l, next);
      rows.push(lineRow(l, end));
      if (next && next.start - end >= INTERLUDE_MS) rows.push(gapRow(end, next.start));
    });
    creditsStart = Math.max(...lines.map((l) => l.end), lines[lines.length - 1].start) + 1500;

    rows.forEach((r, i) => { r.index = i; });
    const frag = document.createDocumentFragment();
    for (const r of rows) frag.appendChild(r.el);
    content.appendChild(frag);
  }

  function lineRow(l, end) {
    const r = {
      gap: false, start: l.start, end, line: l, el: el("div", "row lyric lx-off" + (l.opp ? " opp" : "")),
      flows: [], whole: null, leadEnd: l.words && l.words.length ? l.words[l.words.length - 1].end : l.end,
      bgEnd: l.bg.length ? l.bg[l.bg.length - 1].end : -Infinity,
      state: "", animated: false, settleUntil: 0, d: -1, top: 0, h: 0, kick: 0,
    };
    r.el.dataset.start = String(Math.round(l.start));
    if (l.words) {
      const lead = buildFlow(l.words, "line", l.rtl);
      r.el.appendChild(lead.el);
      r.flows.push(lead);
      if (l.bg.length) {
        const bg = buildFlow(l.bg, "line lx-sub", l.rtl);
        r.el.appendChild(bg.el);
        r.flows.push(bg);
      }
    } else {
      // Line timing only: nothing is guessed about its words.
      const w = el("div", "line lx-whole");
      if (l.rtl) w.dir = "rtl";
      w.textContent = l.text;
      r.el.appendChild(w);
      r.whole = { el: w, w: {} };
      if (l.bg.length) {
        const bg = buildFlow(l.bg, "line lx-sub", l.rtl);
        r.el.appendChild(bg.el);
        r.flows.push(bg);
      }
    }
    return r;
  }

  function gapRow(start, end) {
    const r = {
      gap: true, start, end, el: el("div", "row lx-gap lx-off"), group: el("div", "dotGroup"), dots: [], motions: null,
      state: "", d: -1, top: 0, h: 0, kick: 0, open: 0, openSpring: null, closing: null, visible: false,
    };
    for (let i = 0; i < 3; i++) { const d = el("i", ""); r.group.appendChild(d); r.dots.push({ el: d, w: {} }); }
    r.el.appendChild(r.group);
    return r;
  }

  // ---------------- per-frame painting ----------------
  // Writes a style only when its rounded value changed.
  function setVar(o, key, css, v, digits) {
    const s = v.toFixed(digits);
    if (o.w[key] === s) return;
    o.w[key] = s;
    o.el.style.setProperty(css, s);
  }
  function setTransform(o, scale, liftEm) {
    const s = `translate3d(0, ${liftEm.toFixed(4)}em, 0) scale(${scale.toFixed(4)})`;
    if (o.w.tf === s) return;
    o.w.tf = s;
    o.el.style.transform = s;
  }
  function paintFill(o, fill) {
    const [a, b, c0, c1] = fillStops(fill);
    setVar(o, "fa", "--lx-fa", a, 4);
    setVar(o, "fb", "--lx-fb", b, 4);
    setVar(o, "f0", "--lx-f0", c0, 3);
    setVar(o, "f1", "--lx-f1", c1, 3);
  }
  function clearInline(o) {
    o.w = {};
    const st = o.el.style;
    st.removeProperty("transform");
    for (const k of ["--lx-fa", "--lx-fb", "--lx-f0", "--lx-f1", "--lx-glow"]) st.removeProperty(k);
  }

  function paintUnit(u, p, dt) {
    if (!u.letters) {
      const m = u.motion || (u.motion = new WordMotion(u.start, u.end));
      m.step(p, dt);
      setTransform(u, m.scale.value, m.lift.value);
      setVar(u, "glow", "--lx-glow", Math.min(1, Math.max(0, m.glow.value * 0.35)), 3);
      paintFill(u, m.fill);
      return;
    }
    const g = u.motion || (u.motion = new WordMotion(u.start, u.end));
    const lm = u.letterMotion || (u.letterMotion = new LetterMotion(u.start, u.end, u.letters.length));
    g.step(p, dt);
    lm.step(p, dt);
    setTransform(u, g.scale.value, g.lift.value);
    u.letters.forEach((c, i) => {
      setTransform(c, lm.scale[i].value, lm.lift[i].value * 2); // letters lift twice as far
      setVar(c, "glow", "--lx-glow", Math.min(1, Math.max(0, lm.glow[i].value * 1.85)), 3);
      paintFill(c, lm.fill[i]);
    });
  }

  function restUnit(u) {
    u.motion = null; u.letterMotion = null;
    clearInline(u);
    if (u.letters) for (const c of u.letters) clearInline(c);
  }

  function setAnimated(r, on) {
    if (r.animated === on) return;
    r.animated = on;
    r.el.classList.toggle("lx-anim", on);
    if (!on) {
      for (const f of r.flows) for (const u of f.units) restUnit(u);
      if (r.whole) clearInline(r.whole);
    }
  }

  function paintLine(r, p, dt, isActive) {
    if (r.whole && isActive) {
      const pct = Math.min(1, Math.max(0, (p - r.start) / Math.max(1, r.end - r.start)));
      paintFill(r.whole, pct);
    }
    if (!r.animated) return;
    for (const f of r.flows) for (const u of f.units) paintUnit(u, p, dt);
  }

  // Interlude: shown while current, expanding in on a soft spring and folding away in 400 ms.
  const GAP_CLOSED_EM = 4 * DP;                       // the spacer an idle gap row leaves
  const GAP_OPEN_EM = (13 + 2 * 16) * DP;             // dots plus their padding
  const fastOutSlowIn = bezier(0.4, 0, 0.2, 1);

  function paintGap(r, p, dt, isActive, now) {
    if (isActive && !r.visible) {
      r.visible = true;
      r.closing = null;
      // From the current size, also when it comes back while still folding away.
      r.openSpring = new Spring(Math.max(0, r.open), Math.sqrt(200) / (2 * Math.PI), 1);
      r.openSpring.goal = 1;
      if (!r.motions) {
        const dotsEnd = Math.max(r.start + 300, r.end - DOTS_END_EARLY_MS);
        const third = Math.floor((dotsEnd - r.start) / 3);
        r.motions = [0, 1, 2].map((i) => new DotMotion(r.start + third * i, i === 2 ? dotsEnd : r.start + third * (i + 1)));
      }
    } else if (!isActive && r.visible) {
      r.visible = false;
      r.closing = { from: r.open, at: now };
    }
    if (r.visible) {
      r.openSpring.step(dt);
      r.open = r.openSpring.value;
    } else if (r.closing) {
      const k = Math.min(1, (now - r.closing.at) / 400);
      r.open = r.closing.from * (1 - fastOutSlowIn(k));
      if (k >= 1) { r.closing = null; r.open = 0; r.openSpring = null; r.motions = null; }
    }
    const h = r.visible || r.closing ? GAP_OPEN_EM * Math.max(0, r.open) : 0;
    // The dots and the 4 dp spacer overlap (siblings in one Box on the phone).
    const hs = Math.max(r.visible ? 0 : GAP_CLOSED_EM, h);
    if (r.hs !== hs.toFixed(4)) { r.hs = hs.toFixed(4); r.el.style.height = r.hs + "em"; }
    if (!r.motions) return;
    const intro = Math.min(1, Math.max(0, (p - r.start) / 300));
    const leaving = (p - (r.end - DOTS_HIDE_EARLY_MS)) / 400;
    const s = leaving > 0 ? dotsExitScale(leaving) : 1 - (1 - intro) * (1 - intro);
    const gs = `scale(${s.toFixed(4)})`;
    if (r.gs !== gs) { r.gs = gs; r.group.style.transform = gs; }
    r.motions.forEach((m, i) => {
      m.step(p, dt);
      const d = r.dots[i];
      setTransform(d, m.scale.value, m.lift.value * 1.3); // the phone lifts dots by 1.3 em
      setVar(d, "op", "opacity", Math.min(1, Math.max(0, m.opacity.value)), 3);
    });
  }

  function closeGap(r) {
    r.visible = false; r.closing = null; r.open = 0; r.openSpring = null; r.motions = null;
    r.hs = GAP_CLOSED_EM.toFixed(4);
    r.el.style.height = r.hs + "em";
  }

  function bezier(x1, y1, x2, y2) {
    const cx = 3 * x1, bx = 3 * (x2 - x1) - cx, ax = 1 - cx - bx;
    const cy = 3 * y1, by = 3 * (y2 - y1) - cy, ay = 1 - cy - by;
    const fx = (t) => ((ax * t + bx) * t + cx) * t, fy = (t) => ((ay * t + by) * t + cy) * t;
    return (x) => {
      let t = x;
      for (let i = 0; i < 8; i++) {
        const dx = fx(t) - x, d = (3 * ax * t + 2 * bx) * t + cx;
        if (Math.abs(dx) < 1e-5 || !d) break;
        t -= dx / d;
      }
      return fy(Math.min(1, Math.max(0, t)));
    };
  }

  // ---------------- state of each row ----------------
  function rowClass(r, isActive, isPast) {
    const st = isActive ? "act" : isPast ? "sung" : "";
    if (r.state === st) return;
    r.state = st;
    r.el.classList.toggle("act", st === "act");
    r.el.classList.toggle("sung", st === "sung");
  }

  // Inactive lines blur with distance: min(1.25 dp per line, 6.8 dp), as the phone's
  // RenderEffect radius (sigma = 0.57735 r + 0.5 px).
  function setDistance(r, d) {
    if (r.d === d) return;
    r.d = d;
    if (d <= 0) r.el.style.removeProperty("--lx-blur");
    else {
      const radiusEm = Math.min(d * 1.25, 6.8) * DP;
      r.el.style.setProperty("--lx-blur", `calc(${(0.57735 * radiusEm).toFixed(4)}em + 0.5px)`);
    }
  }

  // ---------------- scrolling: the kick ----------------
  let kicks = [];          // { delta, at (s), anchor }
  let snapping = false;
  let y = 0, yDrawn = NaN, placed = false, kickedTarget = -2;
  let dirtyGeo = true;
  let hostH = 0, emPx = 16;
  let visLo = 0, visHi = -1;

  function resetView() {
    kicks = []; placed = false; kickedTarget = -2; yDrawn = NaN;
    dirtyGeo = true; visLo = 0; visHi = -1;
  }

  /** Damped spring from 1 to 0 (slightly under-damped, settles in roughly 0.6 s). */
  function springBack(t) {
    if (t <= 0) return 1;
    const z = 0.8, w0 = 10.5, wd = w0 * Math.sqrt(1 - z * z);
    return Math.exp(-z * w0 * t) * (Math.cos(wd * t) + ((z * w0) / wd) * Math.sin(wd * t));
  }

  function kickOffset(index, nowS) {
    let sum = 0;
    for (const k of kicks) {
      const delay = Math.min(10, Math.max(0, index - k.anchor + 1)) * STAGGER_S;
      sum += k.delta * springBack(nowS - k.at - delay);
    }
    return sum;
  }

  // Rows far outside the view are not painted at all.
  function windowAround(f) {
    const span = 24;
    const lo = Math.max(0, f - span), hi = Math.min(rows.length - 1, f + span);
    for (let i = visLo; i <= visHi; i++) if (i < lo || i > hi) rows[i].el.classList.add("lx-off");
    for (let i = lo; i <= hi; i++) if (i < visLo || i > visHi) rows[i].el.classList.remove("lx-off");
    visLo = lo; visHi = hi;
  }

  /** Where the content must sit for row `target` to be at the anchor (sentinel = just past the end). */
  function placeFor(target) {
    if (target >= rows.length) {
      const last = rows[rows.length - 1];
      return opts.linePos * hostH - (last.el.offsetTop + last.el.offsetHeight);
    }
    const r = rows[target];
    // An interlude opens downward after it is placed: centre it on its open height.
    const h = r.gap ? GAP_OPEN_EM * emPx : r.el.offsetHeight;
    return opts.linePos * hostH - (r.el.offsetTop + h * 0.5);
  }

  function scrollTo(target, nowS) {
    hostH = host.clientHeight;
    emPx = parseFloat(getComputedStyle(content).fontSize) || emPx;
    if (!hostH) return;
    const next = placeFor(target);
    if (!placed || Math.abs(next - y) > hostH) {
      // Not in view (a seek, a new song): jump there without a wave.
      kicks = [];
      y = next;
      placed = true;
      return;
    }
    const delta = y - next;
    if (Math.abs(delta) < 1) return;
    kicks = kicks.filter((k) => nowS - k.at < KICK_LIFETIME_S);
    kicks.push({ delta, at: nowS, anchor: target });
    y = next;
  }

  // ---------------- frame ----------------
  function frame(p, dt) {
    const n = rows.length;
    if (!n) return;
    const nowMs = performance.now(), nowS = nowMs / 1000;
    // New size or settings: re-place without a wave.
    if (dirtyGeo) { hostH = host.clientHeight; dirtyGeo = false; placed = false; kickedTarget = -2; }

    // A row is current from its start until the next row starts (the credits take over at the end).
    let active = -1;
    for (let i = 0; i < n; i++) { if (rows[i].start <= p) active = i; else break; }
    if (active === n - 1 && p >= creditsStart) active = n;
    // Earlier lines whose singing runs on over the current one (overlapping singers, background vocals).
    const still = new Set();
    if (active > 0) {
      for (let i = active - 1; i >= Math.max(0, active - 2); i--) {
        const r = rows[i];
        if (!r.gap && r.start <= p && p < Math.max(r.leadEnd, r.bgEnd)) still.add(i);
      }
    }
    // Keep an older line in view while its main vocal is still going.
    let target = active;
    for (const i of still) if (rows[i].leadEnd > p && i < target) target = i;

    // A jump (new lyrics, a seek, a new size): rows come into view at their final look,
    // as the phone's freshly laid out rows do, instead of fading or blurring in.
    if (snapping) { host.classList.remove("lx-snap"); snapping = false; }
    const focus = Math.min(Math.max(target, 0), n - 1);
    let jump = !placed || focus < visLo || focus > visHi;
    if (!jump && target !== kickedTarget) {
      // Farther than a screen away: scrollTo will jump there without a wave.
      hostH = host.clientHeight;
      jump = Math.abs(placeFor(focus) - y) > hostH;
    }
    if (jump) { host.classList.add("lx-snap"); snapping = true; }
    windowAround(focus);

    const blur = opts.blur;
    const from = Math.max(0, Math.min(visLo, target) - 1), to = Math.min(n - 1, Math.max(visHi, target) + 1);
    for (let i = 0; i < n; i++) {
      const r = rows[i];
      const isActive = i === active || still.has(i);
      const isPast = active >= 0 && i < active && !isActive;
      if (i < from - 2 || i > to + 2) {
        // Far away: only keep the class right, no animation.
        rowClass(r, isActive, isPast);
        if (!r.gap) setAnimated(r, false);
        else if (!isActive && (r.visible || r.closing)) closeGap(r);
        continue;
      }
      rowClass(r, isActive, isPast);
      if (r.gap) { paintGap(r, p, dt, isActive, nowMs); continue; }
      setDistance(r, blur && !isActive ? (active < 0 ? 0 : Math.abs(i - active)) : 0);
      if (isActive) { r.settleUntil = nowMs + SETTLE_MS; setAnimated(r, true); }
      else setAnimated(r, isPast && nowMs < r.settleUntil && r.animated);
      paintLine(r, p, dt, isActive);
    }

    if (target !== kickedTarget || !placed) {
      kickedTarget = target;
      scrollTo(Math.max(target, 0), nowS);
    }
    if (y !== yDrawn) {
      yDrawn = y;
      content.style.transform = `translate3d(0, ${y.toFixed(2)}px, 0)`;
    }

    // Every row drawn where the kick left it, springing back a little after the row above.
    kicks = kicks.filter((k) => nowS - k.at < KICK_LIFETIME_S);
    for (let i = visLo; i <= visHi; i++) {
      const r = rows[i];
      const off = kicks.length ? Math.round(kickOffset(i, nowS) * 100) / 100 : 0;
      if (off !== r.kick) {
        r.kick = off;
        if (off) r.el.style.translate = `0 ${off}px`;
        else r.el.style.removeProperty("translate");
      }
    }
  }

  // ---------------- options ----------------
  if (typeof ResizeObserver === "function") {
    new ResizeObserver(() => { dirtyGeo = true; }).observe(host);
  } else {
    window.addEventListener("resize", () => { dirtyGeo = true; });
  }
  if (document.fonts && document.fonts.ready) document.fonts.ready.then(() => { dirtyGeo = true; });

  return {
    set(lyrics) { source = lyrics || null; build(); },
    setOptions(o) {
      if (o.blur !== undefined) { opts.blur = !!o.blur; host.classList.toggle("lx-noblur", !opts.blur); }
      if (o.linePos !== undefined && Number.isFinite(Number(o.linePos))) opts.linePos = Number(o.linePos);
      dirtyGeo = true; // the layout may have changed along with the settings
    },
    frame,
    get hasLyrics() { return rows.length > 0; },
    get style() { return "phone"; },
  };
})();
