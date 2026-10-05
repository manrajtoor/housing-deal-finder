-- Where a Michigan listing's water facts came from (CONTRACT.md waterSource):
--   'map'         the crawler's offline OpenStreetMap geography, from a search
--                 card's lat/lon (stored only while detail_read_at is NULL);
--   'description' the detail page's description (always wins).
-- No index: read only as part of score-input rows.
ALTER TABLE listings ADD COLUMN water_source TEXT;

-- Water read before this migration came from descriptions.
UPDATE listings SET water_source = 'description' WHERE water_type IS NOT NULL AND water_source IS NULL;
