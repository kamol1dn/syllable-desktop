// Offline demo (open index.html?demo): fake track + made-up placeholder lyrics that
// exercise every renderer feature — letter emphasis, a background vocal, a duet
// line, wrapping, interlude dots — and a "song change" every loop. Cover and
// background are generated stand-ins (demo/). ?at=12000 starts 12 s into the song.
//   ?demo=line   the same song with line timing only (no word timing)
//   ?demo=long   a long song (200 lines), for checking that a full track stays smooth
"use strict";

const Demo = (() => {
  function build() {
    const Lines = [], Bg = [];
    let t = 3600;
    const line = (words, o = {}) => {
      const Sylls = [];
      let s = o.at ?? t;
      for (const [text, dur, joined] of words) {
        Sylls.push({ Start: s, End: s + dur, Text: text + (joined ? "" : " "), PartOfWord: !!joined });
        s += dur;
      }
      const l = { Start: Sylls[0].Start, End: s, Text: Sylls.map((x) => x.Text).join("").trim(), Sylls, Opposite: !!o.opp };
      if (!o.at) t = s + (o.gap ?? 300);
      return l;
    };
    Lines.push(line([["Neon", 380], ["rivers", 420], ["running", 460], ["through", 300], ["the", 200], ["midnight", 620], ["glass", 700]]));
    Lines.push(line([["Every", 380], ["heartbeat", 520], ["echoes", 480], ["like", 260], ["a", 180], ["sli", 300, true], ["ding", 320], ["bass", 820]]));
    const hold = line([["Hold", 1300], ["on", 1500]], { gap: 500 });
    Lines.push(hold);
    Bg.push(line([["(hold", 500], ["on)", 700]], { at: hold.Start + 1400 }));
    Lines.push(line([["I", 220], ["can", 260], ["hear", 360], ["the", 200], ["city", 420], ["singing", 520], ["back", 360], ["to", 200], ["me", 900]], { opp: true, gap: 4200 }));
    Lines.push(line([["We", 280], ["are", 260], ["gold", 700], ["beneath", 520], ["the", 200], ["static", 520], ["and", 240], ["the", 200], ["rain", 900]]));
    Lines.push(line([["Oooh", 2200]], { gap: 400 }));
    Lines.push(line([["Carry", 420], ["me", 300], ["home", 1100]], { gap: 2500 }));
    return { Type: "Syllable", StartTime: Lines[0].Start, Lines, Bg, Source: "demo" };
  }

  // Line timing only, as LRC-sourced lyrics arrive.
  function lineOnly(src) {
    const Lines = src.Lines.map((l) => ({ Start: l.Start, End: l.End, Text: l.Text, Sylls: null, Opposite: l.Opposite }));
    return { Type: "Line", StartTime: src.StartTime, Lines, Bg: [], Source: "demo" };
  }

  // The short song repeated back to back until it is `count` lines long.
  function long(src, count) {
    const span = src.Lines[src.Lines.length - 1].End + 600 - src.Lines[0].Start;
    const shift = (l, by) => ({
      Start: l.Start + by, End: l.End + by, Text: l.Text, Opposite: l.Opposite,
      Sylls: l.Sylls.map((x) => ({ Start: x.Start + by, End: x.End + by, Text: x.Text, PartOfWord: x.PartOfWord })),
    });
    const Lines = [], Bg = [];
    for (let rep = 0; Lines.length < count; rep++) {
      for (const l of src.Lines) if (Lines.length < count) Lines.push(shift(l, rep * span));
      for (const b of src.Bg) Bg.push(shift(b, rep * span));
    }
    const last = Lines[Lines.length - 1].End;
    return { Type: "Syllable", StartTime: Lines[0].Start, Lines, Bg: Bg.filter((b) => b.Start < last), Source: "demo" };
  }

  const asset = (p) => new URL(p, location.href).href;

  function start() {
    const params = new URLSearchParams(location.search);
    const kind = params.get("demo");
    const lyrics = kind === "line" ? lineOnly(build()) : kind === "long" ? long(build(), 200) : build();
    const total = lyrics.Lines[lyrics.Lines.length - 1].End + 2500;
    const at = Number(params.get("at")) || 0;
    // ?still: no CSS transitions, so a headless screenshot catches settled states.
    if (params.has("still")) {
      const st = document.createElement("style");
      st.textContent = "*,*::before,*::after{transition:none!important}";
      document.head.appendChild(st);
    }
    let t0 = performance.now() - at, song = 0;
    // Stands in for the user's wallpaper folder in "My wallpapers" mode.
    Background.setFolderFiles([asset("demo/landscape.jpg")]);
    const next = () => {
      song++;
      setTrack({ title: "Neon Rivers", artist: "Demo Artist", album: "Placeholder Sessions", cover: asset("demo/cover.png"), durationMs: total });
      setLyrics("ok", lyrics);
    };
    next();
    Clock.update(at, true, 1);
    setInterval(() => {
      let ms = performance.now() - t0;
      if (ms >= total) { t0 = performance.now(); ms = 0; next(); }
      Clock.update(ms, true, 1);
    }, 250);
  }

  return { start };
})();
