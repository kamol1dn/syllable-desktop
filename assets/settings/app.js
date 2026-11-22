// Syllable settings page. No framework, no build step.
//
// The host (crates/settings) sends `init` once and `state` on load and after every
// change made anywhere; this page sends `set` / `applyStyle` / `action` / `account`
// messages back (see crates/settings/src/bridge.rs). Every control is bound to a key
// of config.json by its PascalCase name: a test in bridge.rs checks that every key
// of the config has a control here.
'use strict';

const host = window.chrome && window.chrome.webview;
const send = (msg) => { if (host) host.postMessage(JSON.stringify(msg)); };

let catalogue = { taskbar: [], desktop: [], lock: [], vizPresets: [] };
let S = { config: null, active: {}, collections: [], cache: '', accounts: null };
const refreshers = [];
let lastResultSeq = null;

// ---------------------------------------------------------------------------
// tiny helpers

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'html') el.innerHTML = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

function get(path) {
  let v = S.config;
  for (const k of path.split('.')) v = v == null ? undefined : v[k];
  return v;
}

/** Change one setting: shown at once, saved by the host. */
function set(path, value) {
  if (get(path) === value) return;
  const keys = path.split('.');
  let o = S.config;
  for (const k of keys.slice(0, -1)) o = o[k];
  o[keys[keys.length - 1]] = value;
  send({ type: 'set', path, value });
  refresh();
}

function onRefresh(fn) { refreshers.push(fn); }
function refresh() { if (S.config) for (const fn of refreshers) fn(); }

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const px = (v) => `${+v.toFixed(1)} px`;
const pct = (v) => `${Math.round(v)} %`;

let toastTimer = 0;
function toast(text, kind) {
  const t = document.getElementById('toast');
  t.textContent = text;
  t.className = `toast show ${kind || ''}`;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.className = `toast ${kind || ''}`; }, 3200);
}

let savedTimer = 0;
function flashSaved() {
  const s = document.getElementById('saved');
  s.querySelector('span').textContent = 'Saved';
  s.classList.add('flash');
  clearTimeout(savedTimer);
  savedTimer = setTimeout(() => {
    s.classList.remove('flash');
    s.querySelector('span').textContent = 'Changes apply instantly';
  }, 1400);
}

// ---------------------------------------------------------------------------
// controls; each binds to one config path and redraws itself on every state

function toggle(path, label) {
  const input = h('input', { type: 'checkbox', role: 'switch', 'aria-label': label || path });
  input.addEventListener('change', () => set(path, input.checked));
  onRefresh(() => { input.checked = !!get(path); });
  return h('label', { class: 'toggle', 'data-path': path, 'data-kind': 'toggle' }, input, h('i'));
}

function segmented(path, options) {
  const box = h('div', { class: 'seg', role: 'group', 'data-path': path, 'data-kind': 'choice' });
  const buttons = options.map(([value, label]) =>
    h('button', { type: 'button', onclick: () => set(path, value) }, label));
  box.append(...buttons);
  onRefresh(() => buttons.forEach((b, i) => b.setAttribute('aria-pressed', String(get(path) === options[i][0]))));
  return box;
}

function chips(path, optionsFn, emptyText) {
  const box = h('div', { class: 'chips', role: 'group', 'data-path': path, 'data-kind': 'choice' });
  let key = '';
  onRefresh(() => {
    const options = optionsFn();
    const k = JSON.stringify(options);
    if (k !== key) {
      key = k;
      box.replaceChildren(...options.map(([value, label]) =>
        h('button', { type: 'button', 'data-v': value, onclick: () => set(path, value) }, label)));
      if (!options.length && emptyText) box.append(h('span', { class: 'empty' }, emptyText));
    }
    for (const b of box.querySelectorAll('button')) b.setAttribute('aria-pressed', String(b.dataset.v === get(path)));
  });
  return box;
}

/**
 * A slider over a UI scale (`toUi`/`fromUi` convert, e.g. 0..255 alpha <-> 0..100 %).
 * Sends while dragging (throttled) and once more on release; never fights the user's
 * hand when the echo of its own change comes back.
 */
function slider(path, o) {
  const toUi = o.toUi || ((v) => v);
  const fromUi = o.fromUi || ((v) => v);
  const fmt = o.fmt || String;
  const input = h('input', { type: 'range', min: o.min, max: o.max, step: o.step, 'aria-label': o.label || path });
  const out = h('output');
  let dragging = false;
  let timer = 0;
  const paint = () => {
    const v = +input.value;
    out.textContent = fmt(v);
    input.style.setProperty('--p', `${((v - o.min) / (o.max - o.min)) * 100}%`);
  };
  const commit = () => { clearTimeout(timer); timer = 0; set(path, fromUi(+input.value)); };
  input.addEventListener('pointerdown', () => { dragging = true; });
  input.addEventListener('input', () => {
    paint();
    if (!timer) timer = setTimeout(commit, 90);
  });
  input.addEventListener('change', () => { dragging = false; commit(); });
  input.addEventListener('pointerup', () => { dragging = false; });
  onRefresh(() => {
    if (dragging) return;
    input.value = clamp(toUi(get(path)), o.min, o.max);
    paint();
  });
  return h('div', { class: 'slider', 'data-path': path, 'data-kind': 'slider' }, input, out);
}

function number(path, o) {
  const input = h('input', { type: 'text', inputmode: 'numeric', 'aria-label': o.label || path, spellcheck: 'false' });
  const commit = (v) => {
    if (!Number.isFinite(v)) { input.value = get(path); return; }
    v = clamp(Math.round(v / (o.snap || 1)) * (o.snap || 1), o.min, o.max);
    input.value = v;
    set(path, v);
  };
  input.addEventListener('change', () => commit(parseFloat(input.value.replace(',', '.'))));
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') input.blur();
    else if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
      e.preventDefault();
      commit((parseFloat(input.value) || 0) + (e.key === 'ArrowUp' ? o.step : -o.step));
    }
  });
  onRefresh(() => { if (document.activeElement !== input) input.value = get(path); });
  const step = (d) => h('button', { type: 'button', 'aria-label': d > 0 ? 'More' : 'Less', onclick: () => commit((+get(path) || 0) + d * o.step) }, d > 0 ? '+' : '−');
  return h('div', { class: 'num', 'data-path': path, 'data-kind': 'number' }, step(-1), input, o.unit ? h('span', { class: 'u' }, o.unit) : null, step(1));
}

function text(path, o) {
  const input = h('input', { type: 'text', class: 'text', 'data-path': path, 'data-kind': 'text', placeholder: o.placeholder || '', list: o.list || null, spellcheck: 'false', 'aria-label': o.label || path });
  const commit = () => {
    const v = input.value.trim();
    if (v || o.allowEmpty) set(path, v); else input.value = get(path);
  };
  input.addEventListener('change', commit);
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter') input.blur(); });
  onRefresh(() => { if (document.activeElement !== input) input.value = get(path); });
  return input;
}

function button(label, onclick, cls) {
  return h('button', { type: 'button', class: `btn ${cls || ''}`, onclick }, label);
}

/** A button that asks once more before doing something that cannot be undone. */
function confirmButton(label, confirmLabel, action) {
  let armed = false, timer = 0;
  const b = button(label, () => {
    if (!armed) {
      armed = true;
      b.textContent = confirmLabel;
      b.classList.add('confirm');
      timer = setTimeout(reset, 3500);
      return;
    }
    reset();
    action();
  }, 'danger');
  function reset() { clearTimeout(timer); armed = false; b.textContent = label; b.classList.remove('confirm'); }
  return b;
}

/** One line of a group: title, description and its control. `when` dims it while false. */
function row(title, desc, control, o = {}) {
  const el = h('div', { class: `row ${o.stacked ? 'stacked' : ''}` },
    h('div', { class: 't' }, h('b', null, title), desc ? h('span', typeof desc === 'string' ? null : desc.attrs, typeof desc === 'string' ? desc : desc.node) : null),
    h('div', { class: 'c' }, control));
  if (o.when) onRefresh(() => el.classList.toggle('off', !o.when()));
  return el;
}

function group(title, ...rows) {
  return [title ? h('div', { class: 'section-title' }, title) : null, h('div', { class: 'group' }, ...rows)];
}

function live(fn, cls) {
  const el = h('span', { class: cls || 'value' });
  onRefresh(() => { const v = fn(); if (el.textContent !== v) el.textContent = v; });
  return el;
}

// ---------------------------------------------------------------------------
// style previews, drawn from the style's values (no image assets)

const W = 176, H = 104;
let uid = 0;

function bar(x, y, w, hh, fill, op = 1) {
  return `<rect x="${x.toFixed(1)}" y="${y.toFixed(1)}" width="${Math.max(0, w).toFixed(1)}" height="${hh.toFixed(1)}" rx="${(hh / 2).toFixed(1)}" fill="${fill}" fill-opacity="${op}"/>`;
}

function radial(id, color, cx, cy, r) {
  return `<radialGradient id="${id}" cx="${cx}" cy="${cy}" r="${r}" gradientTransform="translate(${cx} ${cy}) scale(1 ${W / H}) translate(${-cx} ${-cy})"><stop offset="0" stop-color="${color}"/><stop offset="1" stop-color="${color}" stop-opacity="0"/></radialGradient>`;
}

const LINE_WIDTHS = [0.78, 1.0, 0.62, 0.88, 0.5];

function lines(x, cy, w, hh, count, active, far, centred) {
  const gap = hh * 1.15;
  let y = cy - (count * hh + (count - 1) * gap) / 2;
  let out = '';
  for (let i = 0; i < count; i++) {
    const lw = w * LINE_WIDTHS[i % LINE_WIDTHS.length];
    const op = i === active ? 1 : Math.abs(i - active) > 1 ? far * 0.7 : far;
    out += bar(centred ? x - lw / 2 : x, y, lw, hh, '#fff', op);
    y += hh + gap;
  }
  return out;
}

function cover(id, x, y, size, captions) {
  let out = `<linearGradient id="${id}" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#FF7A59"/><stop offset="1" stop-color="#C2337E"/></linearGradient>`;
  out += `<rect x="${x}" y="${y}" width="${size}" height="${size}" rx="4" fill="url(#${id})"/>`;
  out += `<circle cx="${x + size * 0.62}" cy="${y + size * 0.4}" r="${size * 0.2}" fill="#fff" fill-opacity="0.28"/>`;
  if (captions) {
    out += bar(x, y + size + 5, size * 0.8, 3, '#fff', 0.85);
    out += bar(x, y + size + 10.5, size * 0.55, 2.5, '#fff', 0.4);
  }
  return out;
}

function ridge(heights, fill, op) {
  const pts = heights.map((v, i) => `${(W * i) / (heights.length - 1)},${H * v}`).join(' ');
  return `<polygon points="0,${H} ${pts} ${W},${H}" fill="${fill}" fill-opacity="${op}"/>`;
}

function wallpaperPreview(s) {
  const id = `p${++uid}`;
  let defs = '', bg = '', fg = '';
  if (s.Background === 'dynamic') {
    defs += radial(`${id}a`, '#E8437F', 0.15, 0.2, 0.6) + radial(`${id}b`, '#4F5BFF', 0.9, 0.15, 0.55)
      + radial(`${id}c`, '#14B8A6', 0.7, 1.0, 0.5) + radial(`${id}d`, '#FF9F45', 0.1, 1.0, 0.4);
    bg += `<rect width="${W}" height="${H}" fill="#24103F"/>`;
    for (const k of 'abcd') bg += `<rect width="${W}" height="${H}" fill="url(#${id}${k})"/>`;
  } else {
    const soft = s.WallpaperBlur > 0;
    defs += `<linearGradient id="${id}s" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#1D2B53"/><stop offset="0.45" stop-color="#5B4B8A"/><stop offset="0.78" stop-color="#F08A5D"/><stop offset="1" stop-color="#F9C784"/></linearGradient>`;
    defs += radial(`${id}g`, '#FFE3A3', 0.68, 0.66, 0.3);
    bg += `<rect width="${W}" height="${H}" fill="url(#${id}s)"/><rect width="${W}" height="${H}" fill="url(#${id}g)"/>`;
    bg += `<circle cx="${W * 0.68}" cy="${H * 0.66}" r="7" fill="#FFF1C9"/>`;
    bg += ridge([0.70, 0.56, 0.66, 0.50, 0.62, 0.72, 0.60], '#3A3168', 1);
    bg += ridge([0.86, 0.78, 0.90, 0.74, 0.84, 0.80, 0.92], '#161A33', 1);
    if (soft) {
      defs += `<filter id="${id}f" x="-10%" y="-10%" width="120%" height="120%"><feGaussianBlur stdDeviation="${clamp(s.WallpaperBlur / 3, 1, 6)}"/></filter>`;
      bg = `<g filter="url(#${id}f)">${bg}</g>`;
    }
  }
  if (s.Dim > 0) bg += `<rect width="${W}" height="${H}" fill="#000" fill-opacity="${clamp(s.Dim, 0, 90) / 100}"/>`;

  const k = clamp(s.LyricsSize, 60, 200) / 100;
  const far = s.LineBlur ? 0.22 : 0.42;
  switch (s.Layout) {
    case 'mirror':
      fg += lines(18, H / 2, 66, 4.5 * k, 4, 1, far, false) + cover(`${id}cv`, W - 62, (H - 56) / 2, 44, true);
      break;
    case 'lyrics':
      fg += lines(26, H / 2, 104, 5.5 * k, 5, 2, far, false);
      break;
    case 'karaoke':
      fg += bar((W - 118) / 2, H / 2 - 5 * k, 118, 9 * k, '#fff', 1) + bar((W - 70) / 2, H / 2 + 9 * k, 70, 4 * k, '#fff', far);
      break;
    case 'corner':
      fg += `<rect x="${W - 78}" y="${H - 46}" width="70" height="38" rx="7" fill="#000" fill-opacity="0.5"/>`;
      fg += cover(`${id}cv`, W - 72, H - 39, 24, false) + lines(W - 43, H - 27, 29, 3.2 * k, 3, 1, far, false);
      break;
    case 'lock':
      fg += `<text x="${W / 2}" y="31" text-anchor="middle" font-family="Segoe UI Variable Display, Segoe UI" font-weight="600" font-size="21" fill="#fff" fill-opacity="0.92">9:41</text>`;
      fg += lines(W / 2, 68, 84, 4.5 * k, 3, 1, far, true);
      break;
    default:
      fg += cover(`${id}cv`, 18, (H - 56) / 2, 44, true) + lines(76, H / 2, 82, 4.5 * k, 4, 1, far, false);
  }
  if (s.Clock && s.Layout !== 'lock') {
    fg += `<text x="8" y="14" font-family="Segoe UI Variable Display, Segoe UI" font-weight="600" font-size="8.5" fill="#fff" fill-opacity="0.55">9:41</text>`;
  }
  return `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="xMidYMid slice"><defs>${defs}</defs>${bg}${fg}</svg>`;
}

const SPECTRUM = [0.35, 0.6, 0.9, 0.7, 0.5, 0.8, 1.0, 0.65, 0.45, 0.7, 0.55, 0.85, 0.6, 0.4, 0.75, 0.5, 0.3, 0.55];

function taskbarPreview(s) {
  const id = `t${++uid}`;
  const barH = 20, top = H - barH;
  let defs = `<linearGradient id="${id}bg" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#243055"/><stop offset="1" stop-color="#4B2F5E"/></linearGradient>`
    + radial(`${id}r`, '#6C7BD9', 0.3, 0.25, 0.5)
    + `<linearGradient id="${id}v" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#FF5C7A"/><stop offset="1" stop-color="#6C7BFF"/></linearGradient>`;
  let out = `<rect width="${W}" height="${H}" fill="url(#${id}bg)"/><rect width="${W}" height="${H}" fill="url(#${id}r)"/>`;
  out += `<rect x="34" y="22" width="108" height="52" rx="5" fill="#fff" fill-opacity="0.1"/><rect x="34" y="22" width="108" height="9" rx="4" fill="#fff" fill-opacity="0.1"/>`;

  const bars = (bottom, maxH, op) => SPECTRUM.map((v, i) => {
    const step = W / SPECTRUM.length, hh = maxH * v;
    return `<rect x="${i * step + 1.5}" y="${bottom - hh}" width="${step - 3}" height="${hh}" rx="1.5" fill="url(#${id}v)" fill-opacity="${op}"/>`;
  }).join('');
  const viz = () => {
    switch (s.viz_preset) {
      case 'glow-taskbar': return `<rect x="0" y="${top}" width="${W}" height="${barH}" fill="url(#${id}v)" fill-opacity="0.75"/>`;
      case 'equalizer-taskbar': return bars(H, barH - 4, 0.9);
      case 'pulse-bleed':
        defs += radial(`${id}p`, '#FF5C7A', 0.5, 1.0, 0.62);
        return `<rect width="${W}" height="${H}" fill="url(#${id}p)"/>`;
      case 'wave-bleed': {
        let pts = '';
        for (let i = 0; i <= 22; i++) pts += ` ${(W * i) / 22},${top - 14 - 12 * Math.sin(i * 0.62) - 6 * Math.sin(i * 1.7)}`;
        return `<polygon points="0,${H}${pts} ${W},${H}" fill="url(#${id}v)" fill-opacity="0.6"/>`;
      }
      default: return bars(top + 2, 46, 0.7);
    }
  };
  const onBar = s.visualizer && /-taskbar$/.test(s.viz_preset);
  if (s.visualizer && !onBar) out += viz();
  out += `<rect x="0" y="${top}" width="${W}" height="${barH}" fill="#0C0C12" fill-opacity="0.72"/>`;
  for (let i = 0; i < 4; i++) out += `<rect x="${8 + i * 11}" y="${top + 6.5}" width="7" height="7" rx="2" fill="#fff" fill-opacity="${i ? 0.25 : 0.55}"/>`;
  out += bar(W - 26, top + 6, 18, 2.5, '#fff', 0.3) + bar(W - 22, top + 11.5, 14, 2.5, '#fff', 0.3);
  if (onBar) out += viz();
  if (s.lyrics) {
    const above = s.placement !== 'taskbar';
    const y = above ? top - 13 : top + barH / 2 - 2.5;
    if (above) out += `<rect x="${W / 2 - 40}" y="${y - 4}" width="80" height="13" rx="6.5" fill="#000" fill-opacity="0.28"/>`;
    out += bar(W / 2 - 32, y, 36, 5, '#fff', 1) + bar(W / 2 + 7, y, 25, 5, '#fff', 0.4);
  }
  if (s.app_name) out += `<text x="9" y="15" font-family="Segoe UI Variable Display, Segoe UI" font-weight="700" font-size="10.5" fill="#fff" fill-opacity="0.95">Spotify</text>`;
  return `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="xMidYMid slice"><defs>${defs}</defs>${out}</svg>`;
}

const CHECK = '<svg viewBox="0 0 16 16"><path d="M3.5 8.5l3 3 6-7" fill="none" stroke="#fff" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>';

/** The style grid for a surface, plus a live "Custom" card while no style matches. */
function styleGrid(surface, draw, currentLook) {
  const grid = h('div', { class: 'styles' });
  const cards = catalogue[surface].map((st) => {
    const card = h('button', {
      type: 'button', class: 'style-card', 'aria-pressed': 'false',
      onclick: () => {
        send({ type: 'applyStyle', surface, id: st.id });
        S.active[surface] = st.id;
        paint();
      },
    },
    h('span', { class: 'pv', html: draw(st.look || st) }),
    h('span', { class: 'n' }, st.name),
    h('span', { class: 'b' }, st.blurb),
    h('span', { class: 'check', html: CHECK }));
    card.dataset.id = st.id;
    return card;
  });
  const customPv = h('span', { class: 'pv' });
  const custom = h('div', { class: 'style-card custom', 'aria-pressed': 'true' },
    customPv,
    h('span', { class: 'n' }, 'Custom ', h('span', { class: 'tag' }, 'Yours')),
    h('span', { class: 'b' }, 'Your own mix. Pick a style to start over from it.'),
    h('span', { class: 'check', html: CHECK }));
  grid.append(...cards, custom);
  let lastLook = '';
  function paint() {
    const active = S.active[surface];
    for (const c of cards) c.setAttribute('aria-pressed', String(c.dataset.id === active));
    custom.style.display = active ? 'none' : '';
    if (!active) {
      const look = currentLook();
      const k = JSON.stringify(look);
      if (k !== lastLook) { lastLook = k; customPv.innerHTML = draw(look); }
    }
  }
  onRefresh(paint);
  return [h('div', { class: 'section-title' }, 'Style'), grid];
}

// ---------------------------------------------------------------------------
// sections

const EDGES = [['top', 'Top'], ['bottom', 'Bottom']];
const ALIGNS = [['left', 'Left'], ['center', 'Middle'], ['right', 'Right']];
const PLACEMENTS = [['taskbar', 'On the taskbar'], ['above', 'Floating']];
const FONTS = ['Segoe UI Variable Display', 'Segoe UI Variable Text', 'Segoe UI', 'Bahnschrift', 'Arial', 'Calibri', 'Cascadia Code', 'Consolas', 'Georgia', 'Inter', 'Verdana'];
const alpha = { min: 0, max: 100, step: 5, fmt: pct, toUi: (v) => (v / 255) * 100, fromUi: (v) => Math.round((v / 100) * 255) };

function page(id, title, subtitle, ...parts) {
  return h('section', { class: 'page', id: `page-${id}`, 'aria-labelledby': `h-${id}` },
    h('header', null, h('h1', { id: `h-${id}` }, title), subtitle ? h('p', null, subtitle) : null),
    ...parts);
}

function taskbarSection() {
  const lyricsOn = () => get('LyricsEnabled');
  const appOn = () => get('AppNameEnabled');
  const vizOn = () => get('VizEnabled');
  return page('taskbar', 'Taskbar', 'Word-by-word lyrics, the app name and the visualizer on your taskbar.',
    styleGrid('taskbar', taskbarPreview, () => ({
      lyrics: get('LyricsEnabled'), placement: get('Placement') === 'taskbar' ? 'taskbar' : 'above',
      app_name: get('AppNameEnabled'), visualizer: get('VizEnabled'), viz_preset: get('VizPreset'),
    })),
    group('Lyrics strip',
      row('Show lyrics on the taskbar', null, toggle('LyricsEnabled')),
      row('Hide while the desktop is showing', 'The lyrics wallpaper already shows them there.', toggle('LyricsHideOnDesktop'), { when: lyricsOn }),
      row('Attach', 'Sit inside the taskbar, or float just beside it.', segmented('Placement', PLACEMENTS), { when: lyricsOn }),
      row('Screen edge', null, segmented('VPos', EDGES), { when: lyricsOn }),
      row('Alignment', null, segmented('Align', ALIGNS), { when: lyricsOn }),
      row('Edge offset', 'Distance from the left or right screen edge. No effect while middle-aligned.',
        number('XOffset', { min: 0, max: 4000, step: 10, unit: 'px' }), { when: () => lyricsOn() && get('Align') !== 'center' }),
      row('Width', 'The most room the strip may take (never more than 62 % of the screen).',
        slider('Width', { min: 300, max: 1200, step: 20, fmt: px }), { when: lyricsOn })),
    group('Lyrics text',
      row('Font', 'Any font installed on this PC.', text('FontFamily', { list: 'fonts', placeholder: 'Segoe UI Variable Display' }), { when: lyricsOn }),
      row('Text size', 'Shows from the next lyric line.', slider('MainFontPx', { min: 10, max: 24, step: 0.5, fmt: px }), { when: lyricsOn }),
      row('Backing-vocals size', 'The small second row for ad-libs and echoes.', slider('BgFontPx', { min: 8, max: 16, step: 0.5, fmt: px }), { when: lyricsOn }),
      row('Sung words', 'How bright a word is once it has been sung.', slider('SungAlpha', alpha), { when: lyricsOn }),
      row('Upcoming words', 'How bright the words still to come are.', slider('UnsungAlpha', alpha), { when: lyricsOn }),
      row('Backing vocals', null, slider('BgLineAlpha', alpha), { when: lyricsOn }),
      row('Text shadow', 'Helps on light wallpapers. Applies to the lyrics and the app name.', toggle('TextShadow'))),
    group('Active app name',
      row('Show the focused app’s name', 'Like the macOS menu bar.', toggle('AppNameEnabled')),
      row('Attach', null, segmented('AppNamePlacement', PLACEMENTS), { when: appOn }),
      row('Screen edge', null, segmented('AppNameVPos', EDGES), { when: appOn }),
      row('Alignment', null, segmented('AppNameAlign', ALIGNS), { when: appOn }),
      row('Edge offset', 'Distance from the left or right screen edge. No effect while middle-aligned.',
        number('AppNameXOffset', { min: 0, max: 4000, step: 10, unit: 'px' }), { when: () => appOn() && get('AppNameAlign') !== 'center' }),
      row('Text size', null, slider('AppNameFontPx', { min: 8, max: 48, step: 1, fmt: px }), { when: appOn }),
      row('Width', null, slider('AppNameWidth', { min: 120, max: 600, step: 20, fmt: px }), { when: appOn }),
      row('Opacity', null, slider('AppNameAlpha', { ...alpha, min: 10 }), { when: appOn })),
    group('Visualizer',
      row('Show the audio visualizer', 'Reacts to whatever is playing, coloured from the album art.', toggle('VizEnabled')),
      row('Look', null, chips('VizPreset', () => catalogue.vizPresets.map((p) => [p.id, p.name])), { stacked: true, when: vizOn }),
      row('New look on every song', 'Picks a random one each time the track changes.', toggle('VizRandomizeOnTrack'), { when: vizOn }),
      row('Screen edge', null, segmented('VizEdge', EDGES), { when: vizOn }),
      row('Height', 'How far the “bleed” looks may reach up the screen.',
        slider('VizHeightFraction', { min: 15, max: 60, step: 1, fmt: pct, toUi: (v) => v * 100, fromUi: (v) => v / 100 }), { when: vizOn }),
      row('Brightness', null,
        slider('VizOpacity', { min: 5, max: 100, step: 5, fmt: pct, toUi: (v) => v * 100, fromUi: (v) => v / 100 }), { when: vizOn })),
    h('datalist', { id: 'fonts' }, FONTS.map((f) => h('option', { value: f }))));
}

const titleCase = (s) => s.charAt(0).toUpperCase() + s.slice(1);

function wallpaperSection(id, surface, key, title, subtitle) {
  const p = (k) => `${key}.${k}`;
  const hostOn = () => surface !== 'desktop' || get('WallpaperHostEnabled');
  const usesImages = () => hostOn() && get(p('Background')) === 'wallpapers';
  const folderPath = h('span', { class: 'path' });
  onRefresh(() => {
    const f = get('WallpaperFolder');
    folderPath.textContent = f || 'None chosen';
    folderPath.title = f || '';
    folderPath.classList.toggle('none', !f);
  });
  return page(id, title, subtitle,
    surface === 'desktop'
      ? h('div', { class: 'banner' },
          h('div', { class: 't' }, h('b', null, 'Show the lyrics wallpaper on the desktop'),
            h('span', null, 'Lyrics, cover and your wallpapers behind the desktop icons.')),
          toggle('WallpaperHostEnabled', 'Show the lyrics wallpaper on the desktop'))
      : null,
    styleGrid(surface, wallpaperPreview, () => ({ ...get(key) })),
    group('Background',
      row('Background', 'Your own images, or a moving gradient made from the album art.',
        segmented(p('Background'), [['wallpapers', 'My wallpapers'], ['dynamic', 'Album gradient']]), { when: hostOn }),
      row('Wallpaper folder', 'A new image from here on every song. Shared by the desktop and the lock screen.',
        [folderPath, button('Choose…', () => send({ type: 'action', name: 'chooseFolder' }))], { when: hostOn }),
      row('Collection', 'Subfolders and filename prefixes (nord_forest.jpg → Nord) with at least 3 images.',
        chips(p('Collection'), () => [['all', 'All'], ...S.collections.map((c) => [c, titleCase(c)])]),
        { stacked: true, when: usesImages }),
      row('Next wallpaper', 'Skip to another image now, on every open wallpaper.',
        button('Next', () => send({ type: 'action', name: 'nextWallpaper' })), { when: usesImages })),
    group('Layout',
      row('Layout', null, chips(p('Layout'), () => [
        ['split', 'Cover + lyrics'], ['mirror', 'Lyrics + cover'], ['lyrics', 'Lyrics only'],
        ['karaoke', 'Karaoke'], ['corner', 'Corner panel'], ['lock', 'Centred'],
      ]), { stacked: true, when: hostOn }),
      row('Lyrics size', null, slider(p('LyricsSize'), { min: 80, max: 150, step: 5, fmt: pct }), { when: hostOn }),
      row('Blur distant lines', 'Only the current line stays sharp.', toggle(p('LineBlur')), { when: hostOn }),
      row('Clock when nothing plays', surface === 'lock' ? 'Windows shows its own clock on the lock screen too.' : null,
        toggle(p('Clock')), { when: hostOn })),
    group('Background look',
      row('Darken background', 'Keeps the lyrics readable on bright images.',
        slider(p('Dim'), { min: 0, max: 65, step: 5, fmt: (v) => (v ? pct(v) : 'Off') }), { when: hostOn }),
      row('Wallpaper blur', null,
        slider(p('WallpaperBlur'), { min: 0, max: 24, step: 2, fmt: (v) => (v ? px(v) : 'Off') }), { when: hostOn }),
      row('Slow zoom drift', 'Images drift slowly instead of sitting still.', toggle(p('KenBurns')), { when: usesImages }),
      row('Gradient reacts to bass', 'The album gradient pulses with the music.', toggle(p('AudioReactive')), { when: hostOn })));
}

function accountsGroup() {
  const A = () => S.accounts || { ready: false, spicy: {}, spotify: {}, busy: [] };
  const busy = (a) => (A().busy || []).includes(a);
  const ready = () => !!A().ready;
  const account = (action, value) => send({ type: 'account', action, value });

  // Spicy Lyrics key: typed here, stored by the lyrics module, never sent back.
  const keyInput = h('input', { type: 'password', class: 'text', placeholder: 'Paste your API key', spellcheck: 'false', autocomplete: 'off', 'aria-label': 'Spicy Lyrics API key' });
  const keyStatus = h('span', { class: 'pill' });
  const testBtn = button('Test', () => account('testSpicyKey', keyInput.value));
  const saveBtn = button('Save', () => { account('setSpicyKey', keyInput.value); keyInput.value = ''; paintKey(); }, 'primary');
  const removeBtn = confirmButton('Remove', 'Remove key?', () => account('clearSpicyKey'));
  keyInput.addEventListener('input', paintKey);
  keyInput.addEventListener('keydown', (e) => { if (e.key === 'Enter' && keyInput.value.trim()) saveBtn.click(); });
  function paintKey() {
    const has = !!A().spicy.hasKey, typed = !!keyInput.value.trim();
    keyStatus.textContent = !ready() ? 'Starting…' : has ? 'Key saved' : 'No key';
    keyStatus.className = `pill ${!ready() ? 'wait' : has ? 'on' : ''}`;
    keyInput.placeholder = has ? 'Paste a new key to replace it' : 'Paste your API key';
    testBtn.disabled = !typed || busy('testSpicyKey') || !ready();
    testBtn.innerHTML = busy('testSpicyKey') ? '<span class="spin"></span>Testing' : 'Test';
    saveBtn.disabled = !typed || !ready();
    removeBtn.style.display = has ? '' : 'none';
  }
  onRefresh(paintKey);

  // Spotify web-player sign-in.
  const spStatus = h('span', { class: 'pill' });
  const spBtn = button('Sign in', () => account(A().spotify.signedIn ? 'spotifySignOut' : 'spotifySignIn'));
  onRefresh(() => {
    const sp = A().spotify, working = busy('spotifySignIn') || busy('spotifySignOut');
    spStatus.textContent = !ready() ? 'Starting…' : sp.signedIn ? (sp.user ? `Signed in as ${sp.user}` : 'Signed in') : 'Not signed in';
    spStatus.className = `pill ${!ready() ? 'wait' : sp.signedIn ? 'on' : ''}`;
    spBtn.disabled = working || !ready();
    spBtn.className = `btn ${sp.signedIn ? 'danger' : 'primary'}`;
    spBtn.innerHTML = working ? '<span class="spin"></span>Waiting for Spotify' : sp.signedIn ? 'Sign out' : 'Sign in';
  });

  // The user's own Spotify app (PKCE in the browser).
  const idInput = h('input', { type: 'text', class: 'text', placeholder: 'Client ID from developer.spotify.com', spellcheck: 'false', 'aria-label': 'Spotify app client ID' });
  const appStatus = h('span', { class: 'pill' });
  const connectBtn = button('Connect', () => account('connectSpotifyApp', idInput.value), 'primary');
  const cancelBtn = button('Cancel', () => account('cancelSpotifyApp'));
  const disconnectBtn = confirmButton('Disconnect', 'Disconnect?', () => account('disconnectSpotifyApp'));
  const redirect = h('code');
  const copyBtn = h('button', { type: 'button', class: 'btn', style: 'height:26px;padding:0 9px;font-size:12px', onclick: () => copy(redirect.textContent) }, 'Copy');
  idInput.addEventListener('input', () => { connectBtn.disabled = !idInput.value.trim() || busy('connectSpotifyApp') || !ready(); });
  onRefresh(() => {
    const sp = A().spotify, working = busy('connectSpotifyApp');
    if (document.activeElement !== idInput && !idInput.value) idInput.value = sp.clientId || '';
    appStatus.textContent = !ready() ? 'Starting…' : sp.appConnected ? 'Connected' : working ? 'Waiting for the browser' : 'Not connected';
    appStatus.className = `pill ${!ready() || working ? 'wait' : sp.appConnected ? 'on' : ''}`;
    connectBtn.disabled = !idInput.value.trim() || working || !ready();
    connectBtn.style.display = sp.appConnected ? 'none' : '';
    cancelBtn.style.display = working ? '' : 'none';
    disconnectBtn.style.display = sp.appConnected || sp.clientId ? '' : 'none';
    // The port is a setting: show it from the config even before the lyrics module re-reads it.
    const port = get('SpotifyRedirectPort') || 9013;
    redirect.textContent = sp.redirectUri ? sp.redirectUri.replace(/:\d+(?=\/)/, `:${port}`) : `http://127.0.0.1:${port}/callback`;
  });

  return group('Accounts',
    row('Spicy Lyrics API key', 'Your own key for word-by-word lyrics straight from Spicy Lyrics. Stored encrypted on this PC.',
      [keyStatus]),
    row(null, null, [keyInput, testBtn, saveBtn, removeBtn], { stacked: true }),
    row('Spotify sign-in', 'Sign in to the Spotify web player for Spotify’s own lyrics and exact song matching.', [spStatus, spBtn]),
    row('Your Spotify app', { node: ['Optional: exact song matching and the queue through your own app. Add ', redirect, ' ', copyBtn, ' as its redirect URI first.'] }, [appStatus]),
    row(null, null, [idInput, connectBtn, cancelBtn, disconnectBtn], { stacked: true }),
    row('Sign-in port', 'The local port Spotify sends you back to after connecting your app.',
      number('SpotifyRedirectPort', { min: 1024, max: 65535, step: 1 })));
}

function copy(textToCopy) {
  send({ type: 'action', name: 'copy', text: textToCopy });
}

function lyricsSection() {
  return page('lyrics', 'Lyrics', 'Timing, the cache and where lyrics come from.',
    group('Timing',
      row('Sync offset', 'Positive shows the lyrics earlier (use it if they lag the song), negative later. Taskbar and wallpaper.',
        number('GlobalOffsetMs', { min: -5000, max: 5000, step: 50, unit: 'ms' })),
      row('Instrumental dots', 'Show ● ● ● when a gap between lines is at least this long.',
        number('InterludeGapMs', { min: 500, max: 20000, step: 250, unit: 'ms' }))),
    accountsGroup(),
    group('Cache',
      row('Lyrics cache', 'Lyrics are kept on this PC so a song you have played before loads instantly.',
        [live(() => S.cache), confirmButton('Clear', 'Clear the cache?', () => send({ type: 'action', name: 'clearCache' }))]),
      row('Log', 'What the app has been doing; handy when lyrics don’t show up.',
        button('Open log', () => send({ type: 'action', name: 'openLog' })))));
}

function aboutSection() {
  const version = h('span');
  onRefresh(() => { version.textContent = `Version ${catalogue.version || ''}`; });
  return page('about', 'About', null,
    h('div', { class: 'hero' },
      h('span', { class: 'mark', html: document.querySelector('.brand .mark').innerHTML }),
      h('div', null, h('h2', null, 'Syllable'), h('p', null, 'Word-by-word synced lyrics on your taskbar, desktop and lock screen.'), h('p', null, version))),
    group('This PC',
      row('App data', 'Settings, the log and the lyrics cache.', button('Open folder', () => send({ type: 'action', name: 'openDataFolder' }))),
      row('Local port', 'The wallpapers talk to the app on this port. Takes effect after a restart.',
        number('Port', { min: 1024, max: 65535, step: 1 }))),
    group('Good to know',
      row('Quick actions live in the tray', 'Right-click the tray icon for the menu; left-click opens this window.', null),
      row('Styles are starting points', 'Pick one, then change anything below it: the picker shows “Custom”.', null)));
}

// ---------------------------------------------------------------------------
// navigation

const ICONS = {
  taskbar: '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2.5" y="3.5" width="15" height="13" rx="2.5"/><path d="M2.5 12.5h15"/><path d="M6 14.5h3" stroke-linecap="round"/></svg>',
  desktop: '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2.5" y="3" width="15" height="10.5" rx="2"/><path d="M7 17h6M10 13.5V17" stroke-linecap="round"/><path d="M5 11l3-3 2.5 2.5L12 9l3 2.5" stroke-linejoin="round"/></svg>',
  lock: '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="4" y="8.5" width="12" height="9" rx="2"/><path d="M6.5 8.5V6.5a3.5 3.5 0 0 1 7 0v2"/><circle cx="10" cy="13" r="1.2" fill="currentColor"/></svg>',
  lyrics: '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M3 5h9M3 9h7M3 13h5"/><path d="M14 15.5V7l4-1v7.5"/><circle cx="12.5" cy="15.5" r="1.5"/><circle cx="16.5" cy="13.5" r="1.5"/></svg>',
  about: '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="10" cy="10" r="7.5"/><path d="M10 9v5" stroke-linecap="round"/><circle cx="10" cy="6.3" r="0.9" fill="currentColor" stroke="none"/></svg>',
};
const SECTIONS = [['taskbar', 'Taskbar'], ['desktop', 'Desktop wallpaper'], ['lock', 'Lock screen'], ['lyrics', 'Lyrics'], ['about', 'About']];

function show(id, remember) {
  if (!SECTIONS.some(([s]) => s === id)) id = 'taskbar';
  for (const b of document.querySelectorAll('#nav button')) {
    if (b.dataset.id === id) b.setAttribute('aria-current', 'page'); else b.removeAttribute('aria-current');
  }
  for (const pg of document.querySelectorAll('.page')) pg.classList.toggle('on', pg.id === `page-${id}`);
  document.getElementById('main').scrollTop = 0;
  if (remember) send({ type: 'section', id });
}

function build(section) {
  const nav = document.getElementById('nav');
  nav.replaceChildren(...SECTIONS.map(([id, label]) =>
    h('button', { type: 'button', 'data-id': id, html: ICONS[id] + `<span>${label}</span>`, onclick: () => show(id, true) })));
  document.getElementById('pages').replaceChildren(
    taskbarSection(),
    wallpaperSection('desktop', 'desktop', 'WallpaperDesktop', 'Desktop wallpaper', 'The lyrics wallpaper behind your windows.'),
    wallpaperSection('lock', 'lock', 'WallpaperLock', 'Lock screen', 'The same wallpaper on the lock screen, with its own look.'),
    lyricsSection(),
    aboutSection());
  show(section || 'taskbar', false);
}

// ---------------------------------------------------------------------------
// host messages

function onMessage(msg) {
  switch (msg.type) {
    case 'init':
      catalogue = { ...msg.styles, version: msg.version };
      refreshers.length = 0;
      build(msg.section);
      break;
    case 'state': {
      const first = !S.config;
      S = { config: msg.config, active: msg.active || {}, collections: msg.collections || [], cache: msg.cache || '', accounts: msg.accounts };
      const r = S.accounts && S.accounts.result;
      if (r && lastResultSeq !== null && r.seq > lastResultSeq) toast(r.message, r.ok ? 'ok' : 'error');
      if (r || lastResultSeq === null) lastResultSeq = r ? r.seq : 0;
      refresh();
      if (!first) flashSaved();
      break;
    }
    case 'toast':
      toast(msg.text, msg.kind);
      break;
  }
}

if (host) {
  host.addEventListener('message', (e) => onMessage(typeof e.data === 'string' ? JSON.parse(e.data) : e.data));
  send({ type: 'ready' });
} else {
  document.getElementById('pages').textContent = 'This page runs inside Syllable.';
}

// No browser behaviour that does not belong in a settings window.
document.addEventListener('contextmenu', (e) => { if (!e.target.closest('input, textarea')) e.preventDefault(); });
document.addEventListener('keydown', (e) => {
  if (e.ctrlKey && ['p', 's', 'f', 'r', 'u'].includes(e.key.toLowerCase())) e.preventDefault();
  if (e.key === 'F5') e.preventDefault();
});
