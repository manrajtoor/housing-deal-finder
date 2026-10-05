-- Scoring moved out of the Worker into the crawl job (housedeals-score).
--
-- GET /api/score-input pages through one market's listings by id:
--   SEARCH l USING INDEX idx_listings_market_id (market=? AND id>?)
--   SEARCH s USING INDEX sqlite_autoindex_listing_scores_1 (listing_id=?) LEFT-JOIN
CREATE INDEX IF NOT EXISTS idx_listings_market_id ON listings (market, id);

-- The Worker no longer loads comp groups, so their indexes only cost writes
-- (every listing insert and update also wrote these three).
DROP INDEX IF EXISTS idx_listings_nyc_group;
DROP INDEX IF EXISTS idx_listings_borough;
DROP INDEX IF EXISTS idx_listings_mi_group;
