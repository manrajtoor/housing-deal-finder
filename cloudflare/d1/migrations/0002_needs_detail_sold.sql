-- Michigan needs-detail also lists sold comps (sold in the last 365 days)
-- whose detail page was never read, so they get a waterType and count in
-- their water-type comp group. Newest sale first.
--   SEARCH listings USING INDEX idx_listings_needs_detail_sold (market=? AND sold_at>?)
CREATE INDEX IF NOT EXISTS idx_listings_needs_detail_sold ON listings (market, sold_at)
  WHERE detail_read_at IS NULL AND status = 'sold';
