-- housedeals D1 schema (Worker housedeals-api, cloudflare/worker).
--
-- Column names are the contract's listing fields in snake_case (CONTRACT.md),
-- plus the store's own bookkeeping. Every hot query is index-driven; the
-- expected EXPLAIN QUERY PLAN for each is written next to its index and is
-- checked by cloudflare/d1/explain.sh against a local sqlite3.

CREATE TABLE IF NOT EXISTS listings (
  id              TEXT PRIMARY KEY,          -- se:<id> | zl:<zpid>
  source          TEXT NOT NULL,             -- streeteasy | zillow
  market          TEXT NOT NULL,             -- nyc | mi
  status          TEXT NOT NULL,             -- active | sold
  url             TEXT,
  address         TEXT,
  unit            TEXT,
  city            TEXT,
  zip             TEXT,
  lat             REAL,
  lon             REAL,
  price           INTEGER NOT NULL,          -- asking, or sale price when sold
  sold_at         TEXT,
  beds            INTEGER,
  baths           REAL,
  sqft            INTEGER,
  lot_sqft        INTEGER,
  year_built      INTEGER,
  home_type       TEXT NOT NULL,
  neighborhood    TEXT,
  borough         TEXT,
  county          TEXT,
  area            TEXT,
  zestimate       INTEGER,
  days_on_market  INTEGER,
  photo_url       TEXT,
  -- detail-page fields: sticky, a push without them never blanks them
  description     TEXT,
  water_type      TEXT,
  water_body      TEXT,
  frontage_ft     INTEGER,
  maintenance     INTEGER,
  taxes           INTEGER,
  detail_read_at  TEXT,
  comp_only       INTEGER NOT NULL DEFAULT 0,
  -- bookkeeping
  first_seen      TEXT NOT NULL,
  last_seen       TEXT NOT NULL,             -- refreshed at most every 20 h when unchanged
  first_price     INTEGER,
  price_changes   INTEGER NOT NULL DEFAULT 0,
  removed_at      TEXT,                      -- set by the daily expiry, cleared on relist
  fresh_at        TEXT                       -- when it last became alert-eligible (new / relisted / cheaper)
);

-- Expiry sweep, /api/stats counts, needs-detail (Michigan):
--   SEARCH listings USING INDEX idx_listings_market_status (market=? AND status=? AND removed_at=?)
--   (stats: SCAN listings USING COVERING INDEX idx_listings_market_status)
CREATE INDEX IF NOT EXISTS idx_listings_market_status ON listings (market, status, removed_at);

-- NYC comp groups 1-2 (neighbourhood × home type):
--   SEARCH listings USING INDEX idx_listings_nyc_group (market=? AND neighborhood=? AND home_type=?)
CREATE INDEX IF NOT EXISTS idx_listings_nyc_group ON listings (market, neighborhood, home_type);

-- NYC comp group 3 (borough × home type × beds):
--   SEARCH listings USING INDEX idx_listings_borough (market=? AND borough=? AND home_type=? AND beds=?)
CREATE INDEX IF NOT EXISTS idx_listings_borough ON listings (market, borough, home_type, beds);

-- Michigan comp groups (area × water type, and all areas × water type via area IN (...)):
--   SEARCH listings USING INDEX idx_listings_mi_group (market=? AND area=? AND water_type=?)
CREATE INDEX IF NOT EXISTS idx_listings_mi_group ON listings (market, area, water_type);

-- Michigan needs-detail: active, unexpired, detail page not read, newest first.
--   SEARCH listings USING INDEX idx_listings_needs_detail (market=?)
CREATE INDEX IF NOT EXISTS idx_listings_needs_detail ON listings (market, first_seen)
  WHERE detail_read_at IS NULL AND status = 'active' AND removed_at IS NULL;

CREATE TABLE IF NOT EXISTS price_history (
  listing_id  TEXT NOT NULL,
  seen_at     TEXT NOT NULL,
  price       INTEGER NOT NULL,
  PRIMARY KEY (listing_id, seen_at)
);

-- One row per priced active listing, written by the Worker for the groups an
-- ingest touched. /api/deals reads these and never runs the scorer.
CREATE TABLE IF NOT EXISTS listing_scores (
  listing_id    TEXT PRIMARY KEY,
  market        TEXT NOT NULL,
  price         INTEGER NOT NULL,
  discount_pct  REAL NOT NULL,
  alert         INTEGER NOT NULL DEFAULT 0,
  deal          TEXT NOT NULL,               -- the Deal JSON (CONTRACT.md)
  scored_at     TEXT NOT NULL
);
-- /api/deals?market=..., NYC needs-detail:
--   SEARCH s USING INDEX idx_scores_market_discount (market=? AND discount_pct>?)
CREATE INDEX IF NOT EXISTS idx_scores_market_discount ON listing_scores (market, discount_pct DESC);
-- /api/deals without a market:
--   SCAN s USING INDEX idx_scores_discount
CREATE INDEX IF NOT EXISTS idx_scores_discount ON listing_scores (discount_pct DESC);

-- An alert per (listing, asking price): INSERT OR IGNORE makes repeats
-- impossible; a further price drop is a new key and alerts again.
CREATE TABLE IF NOT EXISTS deal_alerts (
  listing_id   TEXT NOT NULL,
  price        INTEGER NOT NULL,
  market       TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  deal         TEXT NOT NULL,                -- the Deal JSON when alerted (detail fields patched in later)
  notified_at  TEXT,                         -- for a future Notifier
  PRIMARY KEY (listing_id, price)
);
CREATE INDEX IF NOT EXISTS idx_alerts_market_created ON deal_alerts (market, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alerts_created ON deal_alerts (created_at DESC);

-- One row per crawl run (market, mode, seenAt), stats summed over its batches.
CREATE TABLE IF NOT EXISTS crawls (
  market   TEXT NOT NULL,
  mode     TEXT NOT NULL,                    -- quick | full | sold
  seen_at  TEXT NOT NULL,
  batches  INTEGER NOT NULL DEFAULT 1,
  stats    TEXT NOT NULL,                    -- JSON: seen, added, updated, ...
  PRIMARY KEY (market, mode, seen_at)
);
