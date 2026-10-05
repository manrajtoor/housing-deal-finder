// Generates synthetic fixtures for the housedeals dashboard (?fixtures=1).
import { writeFileSync, mkdirSync } from 'node:fs';
const OUT = new URL('../public/dev/fixtures', import.meta.url).pathname;
// Run: node dev/gen-fixtures.mjs (from cloudflare/pages)
mkdirSync(OUT + '/photos', { recursive: true });

let seed = 42;
const rnd = () => ((seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648);
const pick = (a) => a[Math.floor(rnd() * a.length)];
const between = (a, b) => a + rnd() * (b - a);
const round = (n, to) => Math.round(n / to) * to;
const NOW = Date.UTC(2026, 9, 5, 15, 40, 0);
const iso = (minAgo) => new Date(NOW - minAgo * 60000).toISOString().replace(/\.\d+Z$/, 'Z');

// Photos: small local SVGs (fixtures only; real photoUrls are https).
const svg = (sky, ground, shape) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 400 240"><defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${sky[0]}"/><stop offset="1" stop-color="${sky[1]}"/></linearGradient></defs><rect width="400" height="240" fill="url(#g)"/>${shape}<rect y="200" width="400" height="40" fill="${ground}"/></svg>`;
const bldg = (c) => [40, 110, 190, 270, 330].map((x, i) => `<rect x="${x}" y="${60 + (i * 37) % 90}" width="${50 + (i % 2) * 20}" height="${200 - (60 + (i * 37) % 90)}" fill="${c}"/>`).join('');
const lake = (c) => `<path d="M0 170 Q100 150 200 165 T400 160 V240 H0Z" fill="${c}"/><polygon points="250,150 290,110 330,150" fill="#7a4b2a"/><rect x="258" y="150" width="64" height="30" fill="#e8dcc4"/>`;
const photos = {
  'nyc-1.svg': svg(['#9cc3e6', '#f1e3d3'], '#6d6a66', bldg('#8a7f74')),
  'nyc-2.svg': svg(['#f6c9a0', '#f9efe3'], '#5c5853', bldg('#a0583c')),
  'nyc-3.svg': svg(['#b7cde3', '#e9eef3'], '#4f5357', bldg('#6f7b86')),
  'mi-1.svg': svg(['#7fb6e8', '#dcecf8'], '#3f6b3a', lake('#2f7fb5')),
  'mi-2.svg': svg(['#f4b183', '#fde6cf'], '#4d6b37', lake('#3a6f9a')),
  'mi-3.svg': svg(['#a9c9e0', '#eef4f8'], '#567a45', lake('#2a8a9a')),
};
for (const [f, s] of Object.entries(photos)) writeFileSync(`${OUT}/photos/${f}`, s);

// ---------- NYC ----------
const hoods = [
  ['Astoria', 'queens', 900], ['Long Island City', 'queens', 1250], ['Jackson Heights', 'queens', 620],
  ['Forest Hills', 'queens', 700], ['Park Slope', 'brooklyn', 1300], ['Bay Ridge', 'brooklyn', 640],
  ['Crown Heights', 'brooklyn', 900], ['Upper West Side', 'manhattan', 1350], ['Washington Heights', 'manhattan', 720],
  ['Harlem', 'manhattan', 950], ['Inwood', 'manhattan', 600], ['Riverdale', 'bronx', 420],
  ['Kew Gardens', 'queens', 560], ['St. George', 'staten_island', 450],
];
const streets = ['31st Ave', 'Ditmars Blvd', '37th Ave', 'Queens Blvd', '7th Ave', 'Prospect Park W', 'Eastern Pkwy',
  'Riverside Dr', 'W 86th St', 'Fort Washington Ave', 'Convent Ave', 'Seaman Ave', 'Johnson Ave', 'Shore Rd', 'Austin St'];
const nyc = [];
for (let i = 0; i < 46; i++) {
  const [hood, borough, ppsf] = pick(i < 9 ? hoods.filter((h) => h[2] <= 900) : hoods);
  const homeType = rnd() < 0.55 ? 'coop' : 'condo';
  const beds = rnd() < 0.6 ? 2 : rnd() < 0.8 ? 3 : 4;
  const baths = beds === 2 ? pick([1, 1, 1.5, 2]) : pick([1.5, 2, 2, 2.5, 3]);
  const sqft = homeType === 'coop' && rnd() < 0.35 ? null : round(between(700, 1000) + (beds - 2) * 280, 5);
  const discount = Math.round((i < 9 ? between(15.5, 34) : between(-22, 14)) * 10) / 10;
  const baselinePrice = sqft ? ppsf * (homeType === 'coop' ? 0.82 : 1) * sqft : ppsf * 900 * (beds / 2.2);
  const baseline = round(baselinePrice, 1000);
  const price = round(baseline * (1 - discount / 100), 1000);
  const n = Math.floor(i < 9 && i !== 1 ? between(8, 40) : between(3, 40));
  const thin = i === 4 || i === 15 || i === 22;
  const alert = discount >= 15 && n >= 8 && !thin && price <= 900000 && discount <= 50;
  const maintenance = homeType === 'coop' ? round(between(0.8, 1.6) * beds * 600, 1) : round(between(400, 900), 1);
  const detail = discount >= 10 && price <= 900000;
  nyc.push({
    id: `se:${1700000 + i * 731}`, url: `https://streeteasy.com/sale/${1700000 + i * 731}`,
    address: `${Math.floor(between(10, 400))} ${pick(streets)}`, unit: `#${Math.floor(between(1, 12))}${pick('ABCDEFGH'.split(''))}`,
    city: 'New York', neighborhood: hood, borough, price, beds, baths, sqft,
    homeType, daysOnMarket: Math.floor(between(1, 120)),
    photoUrl: i % 5 === 3 ? null : `dev/fixtures/photos/nyc-${(i % 3) + 1}.svg`,
    maintenance: detail ? maintenance : null, taxes: detail && homeType === 'condo' ? round(between(300, 900), 1) : null,
    baseline, discountPct: Math.round(((baseline - price) / baseline) * 1000) / 10,
    basis: sqft ? 'ppsf' : 'price',
    group: thin ? `${borough[0].toUpperCase() + borough.slice(1).replace('_', ' ')} · ${homeType === 'coop' ? 'co-op' : 'condo'} · ${beds >= 4 ? '4+' : beds}bd`
      : `${hood} · ${homeType === 'coop' ? 'co-op' : 'condo'} · ${beds >= 4 ? '4+' : beds}bd`,
    n: thin ? Math.floor(between(8, 30)) : n, thin,
    medianPpsf: sqft ? Math.round(baseline / sqft) : null, p25: null, p75: null, alert,
  });
}
// Adversarial rows: hostile text and a non-http link must render inert.
nyc[7].address = '<img src=x onerror=alert(1)> 12 W 104th St';
nyc[7].url = 'javascript:alert(1)';
nyc[7].alert = false;
nyc[11].photoUrl = 'http://insecure.example/photo.jpg';
nyc.sort((a, b) => b.discountPct - a.discountPct);

// ---------- Michigan ----------
const counties = [
  ['grand_traverse', 'traverse', 'Traverse City'], ['leelanau', 'traverse', 'Suttons Bay'], ['antrim', 'traverse', 'Elk Rapids'],
  ['benzie', 'traverse', 'Frankfort'], ['charlevoix', 'petoskey', 'Charlevoix'], ['emmet', 'petoskey', 'Harbor Springs'],
];
const bodies = {
  great_lakes: ['West Grand Traverse Bay', 'East Grand Traverse Bay', 'Lake Michigan', 'Little Traverse Bay', 'Suttons Bay'],
  inland: ['Torch Lake', 'Elk Lake', 'Glen Lake', 'Lake Leelanau', 'Crystal Lake', 'Walloon Lake', 'Lake Charlevoix', 'Burt Lake', 'Crooked Lake'],
  access: ['Torch Lake', 'Crystal Lake', 'Long Lake'],
  other: ['Boardman River', 'Platte River', null],
};
const roads = ['N West Bay Shore Dr', 'E Torch Lake Dr', 'S Glen Lake Rd', 'Old Mission Rd', 'Crystal Dr', 'Walloon Lake Dr', 'Boyne City Rd', 'M-119', 'Peninsula Dr', 'Elk Lake Rd'];
const mi = [];
const ppsfFor = { great_lakes: 520, inland: 430, access: 260, other: 240 };
for (let i = 0; i < 34; i++) {
  const [county, area, city] = pick(counties);
  const waterType = i === 2 || i === 19 ? 'access' : i === 30 ? null : pick(['great_lakes', 'great_lakes', 'inland', 'inland', 'inland', 'other']);
  const sqft = round(i < 8 ? between(1000, 1900) : between(1100, 3200), 10);
  const discount = Math.round((i < 8 ? between(15.2, 31) : i === 2 ? 29 : between(-25, 13)) * 10) / 10;
  const baseline = round((ppsfFor[waterType ?? 'other']) * sqft * (area === 'petoskey' ? 1.08 : 1), 1000);
  const price = round(baseline * (1 - discount / 100), 500);
  const thin = i === 5 || i === 12;
  const n = thin ? Math.floor(between(6, 20)) : Math.floor(between(4, 28));
  const alert = waterType !== 'access' && waterType != null && discount >= 15 && n >= 6 && !thin && price <= 900000 && discount <= 50;
  const body = waterType ? pick(bodies[waterType]) : null;
  mi.push({
    id: `zl:${41000000 + i * 3917}`, url: `https://www.zillow.com/homedetails/${41000000 + i * 3917}_zpid/`,
    address: `${Math.floor(between(1000, 19000))} ${pick(roads)}`, city, county, area,
    price, beds: Math.floor(between(2, 6)), baths: pick([1.5, 2, 2.5, 3, 3.5]), sqft,
    lotSqft: round(between(0.3, 4.5) * 43560, 10), homeType: 'single_family',
    zestimate: rnd() < 0.85 ? round(price * between(0.92, 1.3), 1000) : null,
    daysOnMarket: Math.floor(between(2, 200)),
    photoUrl: i % 6 === 4 ? null : `dev/fixtures/photos/mi-${(i % 3) + 1}.svg`,
    waterType, waterBody: body,
    frontageFt: waterType === 'access' || waterType == null || rnd() < 0.25 ? null : round(between(50, 260), 5),
    baseline, discountPct: Math.round(((baseline - price) / baseline) * 1000) / 10, basis: 'ppsf',
    group: thin ? `All six counties · ${waterType ?? 'other'}` : `${area === 'traverse' ? 'Traverse' : 'Petoskey'} · ${(waterType ?? 'other').replace('_', ' ')}`,
    n, thin, medianPpsf: Math.round(baseline / sqft), p25: null, p75: null, alert,
  });
}
mi.sort((a, b) => b.discountPct - a.discountPct);

const alerts = (rows, startMin) => rows.filter((d) => d.alert).map((d, i) => ({ ...d, createdAt: iso(startMin + i * 173) }));
writeFileSync(`${OUT}/deals-nyc.json`, JSON.stringify({ deals: nyc }, null, 1));
writeFileSync(`${OUT}/deals-mi.json`, JSON.stringify({ deals: mi }, null, 1));
writeFileSync(`${OUT}/alerts-nyc.json`, JSON.stringify({ alerts: alerts(nyc, 14) }, null, 1));
writeFileSync(`${OUT}/alerts-mi.json`, JSON.stringify({ alerts: alerts(mi, 47) }, null, 1));
writeFileSync(`${OUT}/stats.json`, JSON.stringify({
  generatedAt: iso(0),
  counts: [
    { market: 'nyc', status: 'active', n: 5874 }, { market: 'nyc', status: 'expired', n: 312 },
    { market: 'mi', status: 'active', n: 412 }, { market: 'mi', status: 'sold', n: 158 },
  ],
  alerts: [{ market: 'nyc', n: alerts(nyc, 0).length }, { market: 'mi', n: alerts(mi, 0).length }],
  crawls: [
    { market: 'nyc', mode: 'quick', at: iso(9) }, { market: 'nyc', mode: 'full', at: iso(280) },
    { market: 'mi', mode: 'quick', at: iso(11) }, { market: 'mi', mode: 'full', at: iso(282) },
    { market: 'mi', mode: 'sold', at: iso(60 * 24 * 1 + 200) },
  ],
}, null, 1));
writeFileSync(`${OUT}/health.json`, JSON.stringify({ ok: true }));
console.log('nyc', nyc.length, 'alerts', alerts(nyc, 0).length, 'mi', mi.length, 'alerts', alerts(mi, 0).length,
  'thin', nyc.filter((d) => d.thin).length + mi.filter((d) => d.thin).length, 'access', mi.filter((d) => d.waterType === 'access').length);
