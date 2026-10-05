// housedeals dashboard: reads /api/alerts, /api/deals and /api/stats (through
// the Access-protected Pages Function) and renders cards, a table and a footer.
//
// Listing text comes from listing sites, so it is only ever inserted with
// textContent, and links/photos are used only when they parse as http(s)
// (photos: https only).
//
// ?fixtures=1 loads dev/fixtures/*.json instead of /api, for local preview.
'use strict';

const REFRESH_MS = 5 * 60 * 1000;
const MARKETS = ['nyc', 'mi'];
const FIXTURES = new URLSearchParams(location.search).get('fixtures') === '1';

const $ = (sel) => document.querySelector(sel);
const statusEl = $('#status');
const alertsEl = $('#alerts');
const alertsNote = $('#alerts-note');
const form = $('#filters');
const table = $('#deals');
const theadRow = table.querySelector('thead tr');
const tbody = table.querySelector('tbody');
const dealsCount = $('#deals-count');
const statsEl = $('#stats');

// ---------- small helpers ----------

const store = {
  get(k) {
    try { return localStorage.getItem(k); } catch { return null; }
  },
  set(k, v) {
    try { localStorage.setItem(k, v); } catch { /* private mode etc. */ }
  },
};

const isNum = (n) => typeof n === 'number' && Number.isFinite(n);
const money = (n) => (isNum(n) ? '$' + Math.round(n).toLocaleString('en-US') : '–');
const int = (n) => (isNum(n) ? Math.round(n).toLocaleString('en-US') : '–');
const pct = (n) => (isNum(n) ? (n > 0 ? '' : n < 0 ? '−' : '') + Math.abs(n).toFixed(1) + '%' : '–');
const compactMoney = (n) => {
  if (!isNum(n)) return '–';
  if (Math.abs(n) >= 1e6) return '$' + (n / 1e6).toFixed(2).replace(/\.?0+$/, '') + 'M';
  if (Math.abs(n) >= 1e3) return '$' + Math.round(n / 1e3) + 'k';
  return '$' + n;
};
const baths = (n) => (isNum(n) ? String(n) : '–');
const acres = (sqft) => (isNum(sqft) ? (sqft / 43560).toFixed(sqft >= 43560 * 10 ? 1 : 2) : '–');
const ppsf = (d) => (isNum(d.price) && isNum(d.sqft) && d.sqft > 0 ? d.price / d.sqft : null);
const monthly = (d) => (isNum(d.maintenance) || isNum(d.taxes) ? (d.maintenance || 0) + (d.taxes || 0) : null);

const LABELS = {
  homeType: { condo: 'Condo', coop: 'Co-op', townhouse: 'Townhouse', single_family: 'House', multi_family: 'Multi-family', other: 'Other' },
  borough: { manhattan: 'Manhattan', brooklyn: 'Brooklyn', queens: 'Queens', bronx: 'Bronx', staten_island: 'Staten Island' },
  county: { grand_traverse: 'Grand Traverse', leelanau: 'Leelanau', antrim: 'Antrim', benzie: 'Benzie', charlevoix: 'Charlevoix', emmet: 'Emmet' },
  area: { traverse: 'Traverse', petoskey: 'Petoskey' },
  waterType: { great_lakes: 'Great Lakes', inland: 'Inland lake', access: 'Access only', other: 'Other water', unknown: 'Not read yet' },
  market: { nyc: 'NYC', mi: 'Michigan' },
  mode: { quick: 'quick', full: 'full sweep', sold: 'sold comps' },
};
const label = (kind, v) => (v == null || v === '' ? '' : LABELS[kind][v] ?? String(v));

function safeUrl(u, { httpsOnly = false } = {}) {
  if (typeof u !== 'string' || !u) return null;
  try {
    const url = new URL(u, location.href);
    // Fixture photos are local files; real ones must be absolute https.
    if (FIXTURES && url.origin === location.origin && !/^[a-z]+:/i.test(u)) return url.href;
    if (url.protocol === 'https:') return url.href;
    if (url.protocol === 'http:' && !httpsOnly) return url.href;
    return null;
  } catch {
    return null;
  }
}

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = String(text);
  return e;
}

function link(url, text) {
  const href = safeUrl(url);
  if (!href) return el('span', 'muted', '–');
  const a = el('a', null, text);
  a.href = href;
  a.target = '_blank';
  a.rel = 'noopener noreferrer';
  return a;
}

// Discount colour scale: below baseline greens get deeper with the gap.
function discClass(p) {
  if (!isNum(p)) return '';
  if (p < 0) return 'd-neg';
  if (p < 5) return 'd0';
  if (p < 10) return 'd1';
  if (p < 15) return 'd2';
  if (p < 25) return 'd3';
  return 'd4';
}

function ago(iso) {
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return '–';
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) return 'just now';
  const m = s / 60;
  if (m < 60) return `${Math.round(m)} min ago`;
  const h = m / 60;
  if (h < 36) return `${Math.round(h)} h ago`;
  const days = h / 24;
  if (days < 60) return `${Math.round(days)} d ago`;
  return new Date(t).toLocaleDateString();
}

// ---------- data access ----------

class HttpError extends Error {
  constructor(status, msg) {
    super(msg);
    this.status = status;
  }
}

// Fixtures mimic the API's filters so the preview behaves like the real thing.
async function fixture(route, q) {
  const market = q.get('market');
  const name = route === 'stats' || route === 'health' ? route : `${route}-${market}`;
  const res = await fetch(`dev/fixtures/${name}.json`, { cache: 'no-store' });
  if (!res.ok) throw new HttpError(res.status, `fixture ${name}: HTTP ${res.status}`);
  const body = await res.json();
  const limit = Number(q.get('limit')) || Infinity;
  if (route === 'deals') {
    const maxPrice = Number(q.get('maxPrice'));
    const minDiscount = Number(q.get('minDiscount'));
    body.deals = body.deals
      .filter((d) => !q.get('maxPrice') || d.price <= maxPrice)
      .filter((d) => !q.get('minDiscount') || d.discountPct >= minDiscount)
      .slice(0, limit);
  }
  if (route === 'alerts') body.alerts = body.alerts.slice(0, limit);
  return body;
}

async function api(route, params = {}) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== '' && v != null) q.set(k, v);
  if (FIXTURES) return fixture(route, q);
  const qs = q.toString();
  const res = await fetch(`/api/${route}${qs ? '?' + qs : ''}`, {
    headers: { Accept: 'application/json' },
    credentials: 'same-origin',
    cache: 'no-store',
  });
  if (!res.ok) {
    let msg = `HTTP ${res.status}`;
    try {
      const b = await res.json();
      if (b && typeof b.error === 'string') msg = b.error;
    } catch { /* not JSON */ }
    throw new HttpError(res.status, msg);
  }
  return res.json();
}

function explain(err) {
  if (err instanceof HttpError) {
    if (err.status === 403) return 'Your Cloudflare Access session has expired. Reload the page to sign in again.';
    if (err.status === 503) return 'The dashboard is not configured yet (ACCESS_TEAM_DOMAIN / ACCESS_AUD).';
    if (err.status === 502) return 'The API service binding is not configured.';
    return `The API answered: ${err.message}`;
  }
  return 'Could not reach the API. Will retry in 5 minutes.';
}

// ---------- state ----------

const state = {
  market: MARKETS.includes(store.get('housedeals.market')) ? store.get('housedeals.market') : 'nyc',
  deals: [],
  alerts: [],
  stats: null,
  sort: { nyc: { key: 'discountPct', dir: -1 }, mi: { key: 'discountPct', dir: -1 } },
  loadSeq: 0,
};

// ---------- table columns ----------

const commonStart = [
  { key: 'discountPct', label: 'Under %', num: true, get: (d) => d.discountPct,
    cell: (d) => {
      const td = el('td', `num disc ${discClass(d.discountPct)}`, pct(d.discountPct));
      if (d.alert) {
        const dot = el('span', 'alert-dot');
        dot.title = 'Alerts';
        td.append(dot);
      }
      return td;
    } },
  { key: 'price', label: 'Price', num: true, get: (d) => d.price, text: (d) => money(d.price) },
  { key: 'baseline', label: 'Baseline', num: true, get: (d) => d.baseline, text: (d) => money(d.baseline) },
];

const compsCol = {
  key: 'n', label: 'Comps', num: true, get: (d) => d.n,
  cell: (d) => {
    const td = el('td', 'num', int(d.n));
    if (d.thin) td.append(el('span', 'thin', '*'));
    td.title = [d.group, d.basis === 'price' ? 'median price' : d.basis === 'ppsf' ? 'median $/sqft' : '', d.thin ? 'fallback group' : '']
      .filter(Boolean).join(' · ');
    return td;
  },
};
const linkCol = { key: null, label: '', cell: (d) => { const td = el('td'); td.append(link(d.url, 'open')); return td; } };

const COLUMNS = {
  nyc: [
    ...commonStart,
    { key: 'ppsf', label: '$/sqft', num: true, get: ppsf, text: (d) => money(ppsf(d)) },
    { key: 'beds', label: 'Beds', num: true, get: (d) => d.beds, text: (d) => int(d.beds) },
    { key: 'baths', label: 'Baths', num: true, get: (d) => d.baths, text: (d) => baths(d.baths) },
    { key: 'sqft', label: 'Sqft', num: true, get: (d) => d.sqft, text: (d) => int(d.sqft) },
    { key: 'homeType', label: 'Type', get: (d) => label('homeType', d.homeType), text: (d) => label('homeType', d.homeType) || '–' },
    { key: 'neighborhood', label: 'Neighbourhood', get: (d) => d.neighborhood ?? '',
      cell: (d) => {
        const td = el('td', 'wrap', d.neighborhood || '–');
        const sub = [d.address, d.unit].filter(Boolean).join(' ');
        if (sub) td.append(el('span', 'small', sub));
        return td;
      } },
    { key: 'monthly', label: 'Maint+Tax', num: true, get: monthly,
      cell: (d) => {
        const m = monthly(d);
        const td = el('td', 'num', m == null ? '–' : `${money(m)}/mo`);
        if (m != null) td.title = `maintenance ${money(d.maintenance)} · taxes ${money(d.taxes)}`;
        return td;
      } },
    compsCol,
    linkCol,
  ],
  mi: [
    ...commonStart,
    { key: 'water', label: 'Water', get: (d) => `${d.waterType ?? 'zz'} ${d.waterBody ?? ''}`,
      cell: (d) => {
        const td = el('td', 'wrap');
        td.append(waterBadge(d.waterType));
        if (d.waterBody) td.append(' ', el('span', null, d.waterBody));
        const src = waterSourceNote(d);
        if (src) td.append(' ', src);
        const sub = [d.address, d.city].filter(Boolean).join(', ');
        if (sub) td.append(el('span', 'small', sub));
        return td;
      } },
    { key: 'frontageFt', label: 'Frontage', num: true, get: (d) => d.frontageFt, text: (d) => (isNum(d.frontageFt) ? `${int(d.frontageFt)} ft` : '–') },
    { key: 'beds', label: 'Beds', num: true, get: (d) => d.beds, text: (d) => int(d.beds) },
    { key: 'sqft', label: 'Sqft', num: true, get: (d) => d.sqft, text: (d) => int(d.sqft) },
    { key: 'lotSqft', label: 'Lot (ac)', num: true, get: (d) => d.lotSqft, text: (d) => acres(d.lotSqft) },
    { key: 'county', label: 'County', get: (d) => label('county', d.county), text: (d) => label('county', d.county) || '–' },
    { key: 'zestimate', label: 'Zestimate', num: true, get: (d) => d.zestimate, text: (d) => money(d.zestimate) },
    compsCol,
    linkCol,
  ],
};

function waterBadge(type) {
  const t = type || 'unknown';
  const b = el('span', `badge ${LABELS.waterType[t] ? t : 'other'}`, label('waterType', t));
  return b;
}

// "map" (OpenStreetMap geography: the home is within ~90 m of that water) or
// "listing" (read from the description). Text only.
function waterSourceNote(d) {
  if (!d.waterType || (d.waterSource !== 'map' && d.waterSource !== 'description')) return null;
  const map = d.waterSource === 'map';
  const s = el('span', 'water-src muted', map ? 'map' : 'listing');
  s.title = map
    ? 'From the map (OpenStreetMap): the home sits by this water. Frontage and access are not known until the listing is read.'
    : 'From the listing description.';
  return s;
}

// ---------- filters ----------

function syncFilterVisibility() {
  for (const lab of form.querySelectorAll('label[data-market]')) lab.hidden = lab.dataset.market !== state.market;
}

function fillSelect(select, values, labeler) {
  const keep = select.value;
  while (select.options.length > 1) select.remove(1);
  for (const v of values) {
    const o = document.createElement('option');
    o.value = v;
    o.textContent = labeler ? labeler(v) : v;
    select.append(o);
  }
  select.value = values.includes(keep) ? keep : '';
}

const uniq = (arr) => [...new Set(arr.filter((v) => v != null && v !== ''))];

function refreshFilterOptions() {
  const f = form.elements;
  fillSelect(f.homeType, uniq(state.deals.map((d) => d.homeType)).sort(), (v) => label('homeType', v));
  if (state.market === 'nyc') {
    fillSelect(f.borough, uniq(state.deals.map((d) => d.borough)).sort(), (v) => label('borough', v));
    const b = f.borough.value;
    fillSelect(f.neighborhood, uniq(state.deals.filter((d) => !b || d.borough === b).map((d) => d.neighborhood)).sort());
  }
}

function clientFilter(d) {
  const f = form.elements;
  if (f.homeType.value && d.homeType !== f.homeType.value) return false;
  if (state.market === 'nyc') {
    if (f.borough.value && d.borough !== f.borough.value) return false;
    if (f.neighborhood.value && d.neighborhood !== f.neighborhood.value) return false;
  } else {
    if (f.area.value && d.area !== f.area.value) return false;
    const wt = f.waterType.value;
    if (wt && (wt === 'unknown' ? d.waterType != null : d.waterType !== wt)) return false;
  }
  return true;
}

// ---------- rendering ----------

function renderHead() {
  const cols = COLUMNS[state.market];
  const sort = state.sort[state.market];
  theadRow.replaceChildren(
    ...cols.map((c) => {
      const th = el('th', c.num ? 'num' : null);
      th.scope = 'col';
      if (!c.key) return th;
      const btn = el('button');
      btn.type = 'button';
      btn.append(el('span', null, c.label));
      if (sort.key === c.key) {
        th.setAttribute('aria-sort', sort.dir < 0 ? 'descending' : 'ascending');
        btn.append(el('span', 'arrow', sort.dir < 0 ? '▼' : '▲'));
      }
      btn.addEventListener('click', () => {
        const s = state.sort[state.market];
        if (s.key === c.key) s.dir = -s.dir;
        else { s.key = c.key; s.dir = c.num ? -1 : 1; }
        renderTable();
      });
      th.append(btn);
      return th;
    }),
  );
}

function sortRows(rows) {
  const cols = COLUMNS[state.market];
  const { key, dir } = state.sort[state.market];
  const col = cols.find((c) => c.key === key) || cols[0];
  return rows
    .map((d, i) => ({ d, i, v: col.get(d) }))
    .sort((a, b) => {
      const an = a.v == null || a.v === '', bn = b.v == null || b.v === '';
      if (an || bn) return an === bn ? a.i - b.i : an ? 1 : -1; // blanks last either way
      const c = typeof a.v === 'number' && typeof b.v === 'number' ? a.v - b.v : String(a.v).localeCompare(String(b.v));
      return c * dir || a.i - b.i;
    })
    .map((x) => x.d);
}

function renderTable() {
  renderHead();
  const cols = COLUMNS[state.market];
  const rows = sortRows(state.deals.filter(clientFilter));
  if (!rows.length) {
    const td = el('td', 'empty-cell muted', state.deals.length ? 'No listings match these filters.' : 'No scored listings yet.');
    td.colSpan = cols.length;
    const tr = el('tr');
    tr.append(td);
    tbody.replaceChildren(tr);
  } else {
    tbody.replaceChildren(
      ...rows.map((d) => {
        const tr = el('tr');
        for (const c of cols) tr.append(c.cell ? c.cell(d) : el('td', c.num ? 'num' : null, c.text(d)));
        return tr;
      }),
    );
  }
  dealsCount.textContent = state.deals.length
    ? `${rows.length.toLocaleString('en-US')} of ${state.deals.length.toLocaleString('en-US')} shown`
    : '';
}

function card(d) {
  const c = el('article', 'card');

  const photo = el('div', 'photo');
  const src = safeUrl(d.photoUrl, { httpsOnly: true });
  const noPhoto = () => photo.prepend(el('div', 'ph', 'No photo'));
  if (!src) noPhoto();
  else {
    const img = document.createElement('img');
    img.alt = '';
    img.loading = 'lazy';
    img.decoding = 'async';
    img.referrerPolicy = 'no-referrer';
    img.addEventListener('error', () => {
      img.remove();
      noPhoto();
    });
    img.src = src;
    photo.append(img);
  }
  photo.append(el('span', `pill ${discClass(d.discountPct)}`, `${pct(d.discountPct)} under`));
  if (d.createdAt) photo.append(el('span', 'when', ago(d.createdAt)));
  c.append(photo);

  const body = el('div', 'body');
  const pr = el('div', 'price-row');
  pr.append(el('span', 'price', money(d.price)), el('span', 'baseline', `baseline ${money(d.baseline)}`));
  body.append(pr);

  const addr = [d.address, d.unit].filter(Boolean).join(' ');
  body.append(el('div', 'addr', addr || '(no address)'));

  if (state.market === 'nyc') {
    const place = [d.neighborhood, label('borough', d.borough)].filter(Boolean).join(', ');
    if (place) body.append(el('div', 'place', place));
  } else {
    const place = [d.city, label('county', d.county) && `${label('county', d.county)} County`].filter(Boolean).join(', ');
    if (place) body.append(el('div', 'place', place));
    const w = el('div', 'water');
    w.append(waterBadge(d.waterType));
    if (d.waterBody) w.append(el('span', null, d.waterBody));
    const src = waterSourceNote(d);
    if (src) w.append(src);
    if (isNum(d.frontageFt)) w.append(el('span', 'muted', `${int(d.frontageFt)} ft frontage`));
    body.append(w);
  }

  const facts = el('div', 'facts');
  const add = (t) => t && facts.append(el('span', null, t));
  add(isNum(d.beds) ? `${d.beds} bd` : null);
  add(isNum(d.baths) ? `${d.baths} ba` : null);
  add(isNum(d.sqft) ? `${int(d.sqft)} sqft` : null);
  add(label('homeType', d.homeType) || null);
  if (state.market === 'mi') add(isNum(d.lotSqft) ? `${acres(d.lotSqft)} ac lot` : null);
  if (state.market === 'nyc') {
    if (isNum(d.maintenance)) add(`maint ${money(d.maintenance)}/mo`);
    if (isNum(d.taxes)) add(`taxes ${money(d.taxes)}/mo`);
  }
  if (isNum(d.zestimate)) add(`Zestimate ${money(d.zestimate)}`);
  body.append(facts);

  const foot = el('div', 'foot');
  const comps = el('span', null, `${d.group ? d.group + ' · ' : ''}${int(d.n)} comps${d.thin ? ' *' : ''}`);
  foot.append(comps, link(d.url, 'View listing →'));
  body.append(foot);

  c.append(body);
  return c;
}

function renderAlerts() {
  if (!state.alerts.length) {
    alertsEl.replaceChildren(el('div', 'empty', 'No alerts yet for this market.'));
    alertsNote.textContent = '';
    return;
  }
  alertsEl.replaceChildren(...state.alerts.map(card));
  alertsNote.textContent = `≥ 15% under, newest first${FIXTURES ? ' · fixtures' : ''}`;
}

function renderStats() {
  const s = normalizeStats(state.stats);
  if (!s) {
    statsEl.replaceChildren();
    return;
  }
  const blocks = MARKETS.map((m) => {
    const box = el('div');
    const head = el('span');
    head.append(el('strong', null, m === 'nyc' ? 'NYC' : 'Michigan'));
    const counts = s.counts[m] || {};
    const parts = Object.entries(counts).map(([st, n]) => `${int(n)} ${st}`);
    if (parts.length) head.append(` · ${parts.join(' · ')}`);
    box.append(head);
    const crawls = s.crawls[m] || {};
    const modes = Object.keys(crawls).sort((a, b) => ['quick', 'full', 'sold'].indexOf(a) - ['quick', 'full', 'sold'].indexOf(b));
    const line = el('span', null, modes.length ? modes.map((mode) => `${label('mode', mode)} ${ago(crawls[mode])}`).join(' · ') : 'no crawls yet');
    if (modes.length) line.title = modes.map((mode) => `${mode}: ${crawls[mode]}`).join('\n');
    box.append(line);
    return box;
  });
  const meta = el('div');
  meta.append(el('span', null, `Updated ${new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`));
  meta.append(el('span', null, FIXTURES ? 'Showing fixture data' : 'Refreshes every 5 minutes'));
  statsEl.replaceChildren(...blocks, meta);
}

// /api/stats: "counts per market and status, last crawl time per (market,
// mode)". Accept a few plausible shapes so the footer keeps working if the
// Worker's exact JSON differs:
//   counts: [{market,status,n|count}] | {nyc:{active:N}}
//   crawls / lastCrawl / lastCrawls: [{market,mode,at|lastAt|finishedAt|seenAt}] | {nyc:{quick:iso}} | {"nyc:quick":iso}
function normalizeStats(raw) {
  if (!raw || typeof raw !== 'object') return null;
  const out = { counts: {}, crawls: {} };
  const counts = raw.counts ?? raw.listings ?? raw.markets;
  if (Array.isArray(counts)) {
    for (const r of counts) {
      if (!r || !r.market) continue;
      const n = r.n ?? r.count ?? r.listings;
      if (isNum(n)) (out.counts[r.market] ||= {})[r.status ?? 'listings'] = n;
    }
  } else if (counts && typeof counts === 'object') {
    for (const [m, v] of Object.entries(counts)) {
      if (isNum(v)) (out.counts[m] ||= {}).listings = v;
      else if (v && typeof v === 'object') for (const [st, n] of Object.entries(v)) if (isNum(n)) (out.counts[m] ||= {})[st] = n;
    }
  }
  const crawls = raw.crawls ?? raw.lastCrawl ?? raw.lastCrawls ?? raw.scopes;
  const put = (m, mode, at) => {
    if (m && mode && typeof at === 'string') (out.crawls[m] ||= {})[mode] = at;
  };
  if (Array.isArray(crawls)) {
    for (const r of crawls) if (r) put(r.market, r.mode, r.at ?? r.lastAt ?? r.finishedAt ?? r.seenAt ?? r.lastSeenAt);
  } else if (crawls && typeof crawls === 'object') {
    for (const [k, v] of Object.entries(crawls)) {
      if (typeof v === 'string' && k.includes(':')) put(...k.split(':'), v);
      else if (v && typeof v === 'object') for (const [mode, at] of Object.entries(v)) put(k, mode, typeof at === 'string' ? at : at?.at);
    }
  }
  return out;
}

// ---------- loading ----------

function setStatus(msg, isError = false) {
  statusEl.textContent = msg || '';
  statusEl.classList.toggle('error', Boolean(isError && msg));
}

function dealParams() {
  const f = form.elements;
  return {
    market: state.market,
    maxPrice: f.maxPrice.value.trim(),
    minDiscount: f.minDiscount.value.trim(),
    limit: f.limit.value,
  };
}

async function loadDeals() {
  const seq = ++state.loadSeq;
  const market = state.market;
  const body = await api('deals', dealParams());
  if (seq !== state.loadSeq || market !== state.market) return; // a newer load won
  state.deals = Array.isArray(body?.deals) ? body.deals : [];
  refreshFilterOptions();
  renderTable();
}

async function loadAll() {
  const market = state.market;
  if (!state.deals.length) setStatus('Loading…');
  const [alerts, deals, stats] = await Promise.allSettled([
    api('alerts', { market, limit: 12 }),
    loadDeals(),
    api('stats'),
  ]);
  if (market !== state.market) return;
  if (alerts.status === 'fulfilled') {
    state.alerts = Array.isArray(alerts.value?.alerts) ? alerts.value.alerts : [];
    renderAlerts();
  }
  if (stats.status === 'fulfilled') {
    state.stats = stats.value;
    renderStats();
  }
  const failed = [alerts, deals, stats].find((r) => r.status === 'rejected');
  setStatus(failed ? explain(failed.reason) : '', Boolean(failed));
}

function selectMarket(m, { save = true } = {}) {
  state.market = m;
  if (save) store.set('housedeals.market', m);
  for (const b of document.querySelectorAll('.tabs [role=tab]')) {
    const on = b.dataset.market === m;
    b.setAttribute('aria-selected', String(on));
    b.tabIndex = on ? 0 : -1;
  }
  document.title = `House deals · ${m === 'nyc' ? 'NYC' : 'Michigan'}`;
  // Market-specific filters start fresh; price and discount carry over.
  for (const name of ['homeType', 'borough', 'neighborhood', 'area', 'waterType']) form.elements[name].value = '';
  syncFilterVisibility();
  state.deals = [];
  state.alerts = [];
  alertsEl.replaceChildren();
  tbody.replaceChildren();
  renderHead();
  loadAll();
}

// ---------- wiring ----------

const tabs = [...document.querySelectorAll('.tabs [role=tab]')];
for (const b of tabs) b.addEventListener('click', () => b.dataset.market !== state.market && selectMarket(b.dataset.market));
document.querySelector('.tabs').addEventListener('keydown', (e) => {
  if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
  const i = tabs.findIndex((b) => b.dataset.market === state.market);
  const next = tabs[(i + (e.key === 'ArrowRight' ? 1 : tabs.length - 1)) % tabs.length];
  next.focus();
  selectMarket(next.dataset.market);
});

form.addEventListener('submit', (e) => e.preventDefault());
form.addEventListener('change', (e) => {
  const name = e.target.name;
  if (['maxPrice', 'minDiscount', 'limit'].includes(name)) {
    loadDeals().then(() => setStatus(''), (err) => setStatus(explain(err), true));
  } else {
    if (name === 'borough') refreshFilterOptions();
    renderTable();
  }
});

let lastLoad = Date.now();
setInterval(() => {
  if (document.hidden) return;
  lastLoad = Date.now();
  loadAll();
}, REFRESH_MS);
document.addEventListener('visibilitychange', () => {
  if (!document.hidden && Date.now() - lastLoad > REFRESH_MS) {
    lastLoad = Date.now();
    loadAll();
  }
});

selectMarket(state.market, { save: false });
