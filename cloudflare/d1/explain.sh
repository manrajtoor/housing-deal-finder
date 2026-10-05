#!/bin/sh
# Applies the migrations to a throwaway sqlite3 database and prints
# EXPLAIN QUERY PLAN for the Worker's hot queries (same SQL shapes as
# cloudflare/worker/src/scores.rs; the native test
# service::tests::hot_queries_use_indexes asserts the same plans on the
# generated statements). Every line should be SEARCH ... USING INDEX.
set -eu
dir=$(cd "$(dirname "$0")" && pwd)
db=$(mktemp -t housedeals-explain).db
trap 'rm -f "$db"' EXIT
for m in "$dir"/migrations/*.sql; do sqlite3 "$db" < "$m"; done
q() { printf '\n-- %s\n' "$1"; sqlite3 "$db" "EXPLAIN QUERY PLAN $2"; }
q "group load: NYC neighbourhood unit, borough fallback, Michigan water units" \
"SELECT l.id FROM listings l LEFT JOIN listing_scores s ON s.listing_id = l.id
 WHERE ((l.market = 'nyc' AND l.neighborhood = 'Astoria' AND l.home_type = 'condo')
     OR (l.market = 'nyc' AND l.borough = 'queens' AND l.home_type = 'condo' AND l.beds >= 4)
     OR (l.market = 'mi' AND l.area IN ('petoskey', 'traverse') AND l.water_type = 'inland')
     OR (l.market = 'mi' AND l.area IN ('petoskey', 'traverse') AND l.water_type IS NULL))
   AND ((l.status = 'active' AND +l.removed_at IS NULL) OR (l.status = 'sold' AND l.sold_at >= '2025-10-05'))"
q "needs-detail mi" \
"SELECT id, url FROM listings WHERE market = 'mi' AND detail_read_at IS NULL AND status = 'active'
 AND removed_at IS NULL ORDER BY first_seen DESC LIMIT 15"
q "needs-detail nyc" \
"SELECT l.id, l.url FROM listing_scores s JOIN listings l ON l.id = s.listing_id
 WHERE s.market = 'nyc' AND s.discount_pct >= 10 AND s.price <= 900000 AND l.detail_read_at IS NULL
 AND l.status = 'active' AND l.removed_at IS NULL ORDER BY s.discount_pct DESC LIMIT 15"
q "deals" "SELECT deal FROM listing_scores WHERE market = 'nyc' AND discount_pct >= 15 ORDER BY discount_pct DESC LIMIT 50"
q "expiry" "UPDATE listings SET removed_at = 'N' WHERE market IN (SELECT DISTINCT market FROM crawls WHERE mode = 'full' AND seen_at >= 'X') AND status = 'active' AND removed_at IS NULL AND last_seen < 'C'"
q "stats counts" "SELECT market, CASE WHEN removed_at IS NULL THEN status ELSE 'expired' END AS status, COUNT(*) FROM listings GROUP BY market, 2"
q "needs-detail mi sold (after the active ones)" \
"SELECT id, url FROM listings WHERE market = 'mi' AND detail_read_at IS NULL AND status = 'sold'
 AND sold_at >= '2025-10-05' ORDER BY sold_at DESC LIMIT 15"
