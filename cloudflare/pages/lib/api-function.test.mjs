// Tests for the Pages Function functions/api/[[path]].js.
// It lives in lib/ rather than functions/ so Pages never routes it.
// node --test lib/  (from cloudflare/pages)
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';

import { onRequest } from '../functions/api/[[path]].js';
import { clearKeyCache } from './access.js';

const TEAM = 'https://team.cloudflareaccess.com';
const AUD = 'aud-tag';
const now = Date.now();

const b64url = (bytes) => Buffer.from(bytes).toString('base64url');
const enc = (obj) => b64url(new TextEncoder().encode(JSON.stringify(obj)));

const pair = await crypto.subtle.generateKey(
  { name: 'RSASSA-PKCS1-v1_5', modulusLength: 2048, publicExponent: new Uint8Array([1, 0, 1]), hash: 'SHA-256' },
  true,
  ['sign', 'verify'],
);
const jwk = { ...(await crypto.subtle.exportKey('jwk', pair.publicKey)), kid: 'k1', alg: 'RS256' };
const input = `${enc({ alg: 'RS256', kid: 'k1' })}.${enc({ aud: [AUD], iss: TEAM, exp: now / 1000 + 600 })}`;
const sig = await crypto.subtle.sign('RSASSA-PKCS1-v1_5', pair.privateKey, new TextEncoder().encode(input));
const TOKEN = `${input}.${b64url(new Uint8Array(sig))}`;

// verifyAccess fetches the team's keys with the global fetch.
const realFetch = globalThis.fetch;
beforeEach(() => {
  clearKeyCache();
  globalThis.fetch = async (u) => {
    assert.equal(String(u), `${TEAM}/cdn-cgi/access/certs`);
    return new Response(JSON.stringify({ keys: [jwk] }));
  };
});
afterEach(() => {
  globalThis.fetch = realFetch;
});

function mockApi() {
  const calls = [];
  return {
    calls,
    fetch: async (req) => {
      calls.push(req);
      return new Response('{"ok":true}', { headers: { 'Content-Type': 'application/json' } });
    },
  };
}

const env = (extra = {}) => ({ ACCESS_TEAM_DOMAIN: TEAM, ACCESS_AUD: AUD, API: mockApi(), ...extra });
const call = (path, { method = 'GET', headers = { 'Cf-Access-Jwt-Assertion': TOKEN }, e = env() } = {}) =>
  onRequest({ request: new Request(`https://housedeals.pages.dev${path}`, { method, headers }), env: e });

test('503 when Access vars are unset', async () => {
  for (const extra of [{ ACCESS_AUD: '' }, { ACCESS_TEAM_DOMAIN: '' }, { ACCESS_AUD: undefined }]) {
    const e = env(extra);
    const res = await call('/api/deals?market=nyc', { e });
    assert.equal(res.status, 503);
    assert.equal(e.API.calls.length, 0);
  }
});

test('403 without a valid Access token', async () => {
  const cases = [{}, { 'Cf-Access-Jwt-Assertion': 'not.a.jwt' }, { Cookie: 'CF_Authorization=abc.def.ghi' }];
  // Signature broken by changing its last characters.
  cases.push({ 'Cf-Access-Jwt-Assertion': TOKEN.slice(0, -4) + (TOKEN.endsWith('AAAA') ? 'BBBB' : 'AAAA') });
  for (const headers of cases) {
    const e = env();
    const res = await call('/api/deals', { headers, e });
    assert.equal(res.status, 403, JSON.stringify(headers));
    assert.equal(e.API.calls.length, 0);
  }
});

test('only the read routes are reachable', async () => {
  for (const p of ['/api/listings', '/api/listings/detail', '/api/listings/needs-detail', '/api/', '/api/deals/x', '/api/statsx']) {
    const e = env();
    const res = await call(p, { e });
    assert.equal(res.status, 404, p);
    assert.equal(e.API.calls.length, 0);
  }
  for (const p of ['/api/deals', '/api/alerts', '/api/stats', '/api/health', '/api/deals/']) {
    const res = await call(p);
    assert.equal(res.status, 200, p);
  }
});

test('only GET and HEAD pass', async () => {
  for (const method of ['POST', 'PUT', 'DELETE', 'PATCH', 'OPTIONS']) {
    const e = env();
    const res = await call('/api/deals', { method, e });
    assert.equal(res.status, 405, method);
    assert.equal(e.API.calls.length, 0);
  }
  for (const method of ['GET', 'HEAD']) {
    const e = env();
    assert.equal((await call('/api/alerts', { method, e })).status, 200, method);
    assert.equal(e.API.calls[0].method, method);
  }
});

test('forwards a clean request to the internal host', async () => {
  const e = env();
  const res = await call('/api/deals?market=mi&maxPrice=900000&minDiscount=10&limit=200', {
    e,
    headers: {
      'Cf-Access-Jwt-Assertion': TOKEN,
      Cookie: `CF_Authorization=${TOKEN}; other=1`,
      Authorization: 'Bearer x',
      'X-Forwarded-For': '1.2.3.4',
    },
  });
  assert.equal(res.status, 200);
  assert.deepEqual(await res.json(), { ok: true });
  assert.equal(e.API.calls.length, 1);
  const fwd = e.API.calls[0];
  assert.equal(fwd.url, 'https://housedeals-api.internal/api/deals?market=mi&maxPrice=900000&minDiscount=10&limit=200');
  assert.equal(fwd.method, 'GET');
  assert.deepEqual([...fwd.headers.keys()], ['accept']);
  assert.equal(fwd.headers.get('accept'), 'application/json');
});

test('502 when the service binding is missing', async () => {
  const res = await call('/api/health', { e: env({ API: undefined }) });
  assert.equal(res.status, 502);
});
