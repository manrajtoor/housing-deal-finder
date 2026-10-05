# Design

Finds homes asking well under what comparable homes ask. Two markets:

- **NYC**: 2+ bedroom apartments (condo, co-op) for sale, alert at ≤ $900k.
- **Northern Michigan**: waterfront houses for sale (Great Lakes and inland
  lakes) in Grand Traverse, Leelanau, Antrim, Benzie (the "Traverse" area) and
  Charlevoix, Emmet (the "Petoskey" area), alert at ≤ $900k.

Same shape as `used-car-deal-finder`: a Go crawler in GitHub Actions pushes
batches to a Rust Worker, which stores them in D1 and serves a Pages
dashboard behind Cloudflare Access.

Scoring runs in the crawl job, not the Worker. Rescoring comp groups inside
each push cost 13-56 ms of Worker CPU (wasm, plus turning every D1 row into
Rust values) against the Free plan's 10 ms, and a full crawl hit Error 1102.
So after its searches and detail reads, the job reads each crawled market's
rows back (`GET /api/score-input`, JSON built by SQLite), runs the Rust
scorer as a native binary (`housedeals-score`, a few ms) and posts only the
scores that changed, plus alerts (`POST /api/scores`). The Worker's routes
just bind values and pass JSON strings through.

## Sources (probed from GitHub Actions, 2026-10-05)

| Source | Used for | Notes |
|---|---|---|
| StreetEasy search pages | NYC listings | Data in the Next.js RSC payload: id, areaName, bedroomCount, buildingType, livingAreaSize, price. ~86 a page. |
| Zillow search pages | Michigan listings, active and sold | `__NEXT_DATA__` → `cat1.searchResults.listResults`, 41 a page. County regions + `isWaterfront`. |
| Zillow detail pages | Michigan water facts (when readable) | Only the description says which lake, how many feet and whether the frontage is private or shared, so it is parsed once per listing. Since 2026-10 Zillow answers 403 to runners on detail pages (search pages still work), so the map below is the usual source. |
| OpenStreetMap (offline, embedded) | Michigan water type from lat/lon | Great Lakes shore and named inland lakes, © OpenStreetMap contributors, ODbL. See "Water from the map". |

Redfin, Realtor.com, Homes.com, LandWatch and Compass block GitHub's runners.
Craigslist (thin and noisy) and NYC DOF sold prices (no sq ft or beds for
units) are possible later additions, not part of v1.

Zillow Group's terms forbid automated access. The owner accepted that: crawls
stay small (1.5 s between requests, 6 s between detail pages, one process),
are for personal use only, and nothing is republished; the dashboard sits
behind Access. Zillow answered 403 after 14 detail pages 1.5 s apart, so a
run reads at most 6 (quick) or 20 (full) detail pages per market, and a
blocked detail page ends that market's detail reads for the run with a
warning, not a failed run.

## Comps go above the alert ceiling

Listings are crawled up to **$1.5M** and only those ≤ $900k alert. Cutting
comps at $900k would drag every baseline down and hide real deals.

## Scorer (new Rust crate, same rules as the car scorer)

Medians, never means. A group with too few comps does not get a price.

**NYC.** A listing is priced at the group median of price per sq ft × its sq ft.
Groups, tried in order until one has enough comps:

1. neighbourhood × building type (condo / co-op) × beds (2, 3, 4+)
2. neighbourhood × building type
3. borough × building type × beds (thin, marked `*`)

Without sq ft (common for co-ops), the median asking price of the same group
is used instead. Co-op maintenance is not on search cards; it is fetched from
the detail page for alert candidates only and shown next to the alert.

**Michigan.** Water facts come from the map at search time (below) and,
when a detail page can be read, from the description, which then wins:
`great_lakes` (Lake
Michigan and its bays: Grand Traverse, West/East Bay, Little Traverse,
Suttons), `inland` (Torch, Elk, Glen, Leelanau, Crystal, Walloon, Charlevoix,
Burt, Crooked, …), `access` (shared/deeded/association access: never alerts),
`other` (river, pond, unknown). The price is the group median of price per sq ft
of living area × sq ft. Groups:

1. area × water type
2. all six counties × water type (thin)

**Water from the map.** Every Zillow search card has latitude/longitude. The
crawler classifies it offline (`crawler/internal/geo`, data embedded as
`water.json.gz`, ~170 kB): within **90 m** of the Lake Michigan / Lake Huron
shore (mainland or island) → `great_lakes` (body: a bay such as Suttons Bay,
West/East Grand Traverse Bay, Little Traverse Bay when one applies, else the
lake), within 90 m of a named inland lake of ≥ 10 ha (inside counts as 0 m) →
`inland` with the lake's name, whichever is nearer; otherwise no water type.
The card then carries `waterSource: "map"`. The Worker stores map water only
while the row has no detail read; a description that names water replaces it
whole (`waterSource: "description"`) and is never overwritten by the map. The
map cannot tell private frontage from shared access or across-the-road
homes, so map water is a little looser than the description; the alert rule
still counts it (`great_lakes` / `inland`), since without it Michigan could
not alert at all while detail pages are blocked. The dashboard marks each
water badge "map" or "listing".

Data: OpenStreetMap via the Overpass API, built once by
`tools/geo/build_water.py` (the Great Lakes are `natural=water` relations in
OSM, not `natural=coastline`; their member ways in the six counties' bbox
are the shore), simplified to ~10 m. **© OpenStreetMap contributors, ODbL
1.0** (https://www.openstreetmap.org/copyright).

Threshold, tuned on the 248 live Michigan rows (2026-10-05, all
waterfront-filtered by Zillow, none with a description read yet): distance
to the nearest water piles up at 10-60 m (103 rows), stays above the
background to ~90 m, then is flat (~0.4 rows per metre from 90 to 200 m:
homes near, not on, the water). Counts by threshold:

| Threshold | great_lakes | inland | none |
|---|---|---|---|
| 60 m | 17 | 86 | 145 |
| 90 m (chosen) | 23 | 102 | 123 |
| 120 m | 28 | 110 | 110 |
| 200 m | 33 | 137 | 78 |

The rest are rivers, ponds under 10 ha, unnamed lakes or bad geocodes. The
one fixture with a read description (10 Island View Dr, "200 feet of private
frontage on Island Lake") agrees with the map.

Comps are active listings plus homes sold in the last 12 months. The sold ones
use their sale price, which tends to sit a little under asking, so the
baseline is conservative and alerts lean toward fewer false positives.

**Alert rule:** ≥ 15% under baseline, ≥ 6 comps (Michigan) or ≥ 8 (NYC), not
thin, price ≤ $900k, discount ≤ 50% (bigger means bad data), and in
Michigan only `great_lakes` or `inland` water, from the description or the
map (unknown, `other` and `access` listings are scored but never alert).
The Zestimate is shown next to the alert as a second opinion, never used in
the score. The rule's numbers are `housedeals-score` flags with these
defaults. A listing alerts only while fresh: the Worker records `fresh_at`
when a listing is new and young in a quick crawl (not a market's first run),
relisted or cheaper, and the scorer alerts on it for 72 hours from then.

**Stored scores.** The scorer scores every active listing of the market
against all of its rows and rewrites a stored score only when it is missing,
its discount moved by 0.5 point or more, its alert flag flipped, the price
changed, or the detail page was read after it was scored. Scores that no
longer price are deleted; the daily expiry deletes those of removed
listings.

## Schedule (Worker cron → `workflow_dispatch`)

- Every 30 min: newest page of each search (NYC and Michigan), up to 6 new
  detail pages per market, then scoring.
- Daily 11:00 UTC: full sweep of every page (keeps `last_seen` honest), up to
  20 detail pages per market, scoring, plus expiry of listings unseen for
  3 days.
- Weekly: Michigan sold comps, then Michigan scoring.

## D1 budget

The crawler reads score-input in pages of 500 rows and posts detail reads in
chunks of at most 10, to keep each Worker request's CPU small. Map water adds
no reads: ingest's existing per-batch lookup also returns the stored water.


~6 000 NYC + ~500 Michigan rows. Unchanged rows are not rewritten; `last_seen`
is refreshed only every 20 h, so a daily sweep touches each row about once a
day (~6 500 writes) plus price changes and new listings. Score writes are
limited the same way: unchanged scores are not rewritten.

## Names (personal Cloudflare account 7377038a…)

Worker `housedeals-api`, D1 `housedeals`, Pages `housedeals`.
