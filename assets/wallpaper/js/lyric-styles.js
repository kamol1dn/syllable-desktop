// Lyric style presets. A style is plain data: the lyric engine (lyric-view.js) reads
// the numbers, lyrics.css carries the matching look under `.lx-style-<id>`. Adding a
// preset is one register() call here plus, if it needs its own colours or type, a
// CSS block; no engine changes.
//
//   fill.feather   soft edge of the sweep, in em (0 = hard karaoke edge)
//   unit           motion of one sung syllable/word while the sweep crosses it
//     lift           how far it rises, em          grow     how much it swells (0.05 = 5 %)
//     attackMs       time to reach the peak        releaseMs time to settle after it ends
//     curve          "soft" (ease out) | "snap" (overshoots, then settles)
//   hold           long-held syllables, animated letter by letter
//     minMs          shortest syllable that counts as held
//     fullMs         length at which the effect is at full strength
//     maxChars       longer chunks are left alone
//     grow/lift      per-letter swell and rise     spread   letters drift apart, em
//     glow           0..1 strength of the halo
//   cascade        line-synced lyrics (no word timing): words light up one after another
//     stepMs         delay between words           wordMs   time one word takes to light
//     maxMs          cap for the whole cascade (also never longer than 40 % of the line)
//   focus          de-emphasis of the lines around the current one
//     reach          lines further than this all look the same
//   scroll         how the list follows the current line
//     settle         time constant, s (smaller = snappier)
//     leadMs         start moving this long before the next line begins
//   gap            interlude indicator
//     minMs          shortest instrumental gap that gets one
//     outMs          it folds away this long before the vocals return
//   vars           CSS custom properties set on #lyrics (see lyrics.css for the list)
"use strict";

const LyricStyles = (() => {
  const styles = new Map();
  let fallback = null;

  const BASE = {
    fill: { feather: 0.42 },
    unit: { lift: 0.045, grow: 0.018, attackMs: 190, releaseMs: 520, curve: "soft" },
    hold: { minMs: 780, fullMs: 2100, maxChars: 14, grow: 0.085, lift: 0.07, spread: 0.042, glow: 0.8 },
    cascade: { stepMs: 46, wordMs: 260, maxMs: 620 },
    focus: { reach: 5 },
    scroll: { settle: 0.085, leadMs: 160 },
    gap: { minMs: 3200, outMs: 520 },
    vars: {},
  };

  function register(def) {
    const s = { id: def.id, label: def.label || def.id };
    for (const k of Object.keys(BASE)) s[k] = Object.assign({}, BASE[k], def[k] || {});
    styles.set(s.id, s);
    if (!fallback) fallback = s;
    return s;
  }

  // Glide: the default. A soft-edged sweep; each syllable rises a touch as it is sung
  // and sinks back, held notes open up letter by letter. Distant lines blur away.
  register({ id: "glide", label: "Glide" });

  // Punch: bold caps in the cover's accent colour. Words snap in with a small
  // overshoot, the sweep has a hard edge, and other lines dim instead of blurring.
  register({
    id: "punch", label: "Punch",
    fill: { feather: 0.06 },
    unit: { lift: 0, grow: 0.07, attackMs: 150, releaseMs: 340, curve: "snap" },
    hold: { grow: 0.13, lift: 0.02, spread: 0.062, glow: 1 },
    cascade: { stepMs: 60, wordMs: 170, maxMs: 700 },
    focus: { reach: 3 },
    scroll: { settle: 0.06, leadMs: 110 },
  });

  // Mist: quiet. No movement at all on ordinary words, a long wash for the sweep,
  // lighter type; held notes only glow.
  register({
    id: "mist", label: "Mist",
    fill: { feather: 1.1 },
    unit: { lift: 0, grow: 0, attackMs: 300, releaseMs: 900 },
    hold: { grow: 0.03, lift: 0, spread: 0.016, glow: 1 },
    cascade: { stepMs: 80, wordMs: 520, maxMs: 1100 },
    focus: { reach: 6 },
    scroll: { settle: 0.13, leadMs: 240 },
  });

  return {
    register,
    get: (id) => styles.get(id) || fallback,
    has: (id) => styles.has(id),
    list: () => [...styles.values()].map((s) => ({ id: s.id, label: s.label })),
  };
})();
