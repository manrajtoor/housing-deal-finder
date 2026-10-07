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
| Zillow search pages | Michigan listings, active and sold; NYC sold (comps) | `__NEXT_DATA__` → `cat1.searchResults.listResults`, 41 a page, at most 20 pages a search. County regions + `isWaterfront`; NYC borough regions (type 17) recently sold, 2+ beds, no houses. |
| Zillow detail pages | Michigan water facts (when readable) | Only the description says which lake, how many feet and whether the frontage is private or shared, so it is parsed once per listing. Since 2026-10 Zillow answers 403 to runners on detail pages (search pages still work), so the map below is the usual source. |
| OpenStreetMap (offline, embedded) | Michigan water type from lat/lon | Great Lakes shore and named inland lakes, © OpenStreetMap contributors, ODbL. See "Water from the map". |

Redfin, Realtor.com, Homes.com, LandWatch and Compass block GitHub's runners.
Craigslist (thin and noisy) and NYC DOF sold prices (no sq ft or beds for
units) are possible later additions. NYC sold prices come from Zillow (see
"NYC sold comps").

Zillow Group's terms forbid automated access. The owner accepted that: crawls
stay small (1.5 s between requests, 6 s between detail pages, 8 s between
Zillow search pages in sold mode, one process),
are for personal use only, and nothing is republished; the dashboard sits
behind Access. Zillow answered 403 after 14 detail pages 1.5 s apart, so a
run reads at most 6 (quick) or 20 (full) detail pages per market, and a
blocked detail page ends that market's detail reads for the run with a
warning, not a failed run. Search pages have a limit of their own from
GitHub's runners (a local run read 109 in a row): on 2026-10-07 a sold run
got 429 after ~22 Zillow search pages 1.5 s apart, and every Zillow page
after it too. So a sold run makes at most 25 Zillow requests
(`--sold-budget`), 8 s apart; quick and full runs read only ~7-10 Zillow
pages at the normal 1.5 s. Zillow answering 403, 429 or a bot check ends
every Zillow request of the run (searches and detail pages) with a warning,
and the run still exits 0: it is expected now and then, and the next run
tries again. The fetcher does not retry Zillow's 429 (a retry only counts
against us).

## Comps go above the alert ceiling

Listings are crawled up to **$1.5M** and only those ≤ $900k alert. Cutting
comps at $900k would drag every baseline down and hide real deals.

## Scorer (new Rust crate, same rules as the car scorer)

Medians, never means. A group with too few comps does not get a price, and
the scorer refuses rather than guesses (see "Calibration" for why).

**NYC.** Only apartments: condos and co-ops are subjects and comps;
houses, townhouses, multi-family buildings and StreetEasy's unknown types
are refused ("not an apartment") and are not comps. One group, beds always
matching, condos and co-ops never mixed:

1. borough × neighbourhood × building type (condo / co-op) × beds (2, 3, 4+)

(The borough is in the key because StreetEasy reuses area names: Murray
Hill is in Manhattan and in Queens.) There is no wider fallback: the old
"neighbourhood × type" level mixed 2- and 4-bedroom units and the "borough
× type × beds" level priced e.g. a Starrett City condo against all of
Brooklyn (n = 317), so a listing whose group cannot price it is refused.

With a usable sq ft the comps are the group's units within **±35%** of the
subject's size, and each comp's price is moved to the subject's size with
a size elasticity: `value = comp price × (subject sqft / comp sqft)^b`; the
baseline is the median value. $/sq ft falls with size (b < 1), so a flat
median $/sq ft overprices large units (a 2 230 sf Homecrest condo came out
at $1.67M). `b` is estimated on every run as the pooled Theil–Sen slope
(median of pairwise slopes of log price on log sq ft, pairs of comps in the
same group differing by ≥ 15% in size), per building type, clamped to
0.3-1.0: 0.67 for condos and ≥ 1 (so 1.0) for co-ops on the 2026-10-05
data, whose co-op sq ft are often round agent estimates. Defaults (condo
0.7, co-op 0.9) apply with fewer than 200 pairs. The Deal's `medianPpsf`,
`p25`, `p75` are the comps' $/sq ft at the subject's size.

Without sq ft (common for co-ops), the median asking price of the group's
units with the same baths (1, 1.5, 2, 2.5+: a size proxy) is used, and only
when that group is tight ((p75 − p25) / median ≤ 0.35, against 0.75 for
$/sq ft). A subject with sq ft is never priced by the median price of comps
without one.

Refused as well, and never comps:
- restricted or special sales, by neighbourhood (Starrett City, Spring
  Creek, Co-op City, Rochdale Village, Penn South: Mitchell-Lama) or by words
  in the address, unit or description (HDFC, Mitchell-Lama, income limits,
  affordable housing, auction, land/ground lease, leasehold, 55+, age
  restricted, timeshare, fractional, life estate). StreetEasy cards have no
  description, so in NYC this mostly catches the neighbourhoods;
- units of a "cheap building": when the other listings at the same address
  (and type) ask, at the median, ≥ 10% under their own comps, the whole
  building is cheap for a reason comps cannot see (restrictions, land lease,
  high maintenance, sponsor terms). Three 246 E 51st St co-ops at $450k and
  555 Kappock St co-ops were such cases. 126 units on the 2026-10-05 data.

Co-op maintenance is not on search cards; it is fetched from the detail
page for alert candidates only and shown next to the alert.

**The $1.5M crawl ceiling.** In a group whose true median is near or above
$1.5M the crawl sees only the cheaper part, so its median is biased down
(and the units it does see are the smaller or worse ones). Both markets: a
group where more than a third of the picked comps ask ≥ $1.35M (90% of the
ceiling) is refused ("at the crawl ceiling"; 125 NYC listings). The bias is
conservative (a low baseline hides deals rather than inventing them), so
the rule is about not showing misleading baselines more than about false
alerts; on this data the refused groups' discounts were distributed like
the rest.

**Michigan.** Water facts come from the map at search time (below) and,
when a detail page can be read, from the description, which then wins:
`great_lakes` (Lake
Michigan and its bays: Grand Traverse, West/East Bay, Little Traverse,
Suttons), `inland` (Torch, Elk, Glen, Leelanau, Crystal, Walloon, Charlevoix,
Burt, Crooked, …), `access` (shared/deeded/association access: never alerts),
`other` (river, pond, unknown). Every home type is priced (they are mostly
houses). The price is size-adjusted like NYC's: comps within **±50%** of
the subject's living area (fewer comps up north), moved to its size with
one Michigan elasticity (0.67 on the 2026-10-05 data, default 0.6).
Groups:

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

Comps are active listings plus homes sold in the last 12 months (both
markets). The sold ones use their sale price, which tends to sit a little
under asking, so the baseline is conservative and alerts lean toward fewer
false positives. Sold rows are never subjects.

**NYC sold comps.** The daily `sold` crawl reads Zillow's recently-sold
search per borough (Manhattan 12530, Brooklyn 37607, Queens 270915, Bronx
17182, Staten Island 27252, region type 17), 2+ beds, no houses, townhouses,
multi-family, land or manufactured homes, up to $1.5M, in price bands. The
window is 6 months (`--sold-window`): ~4 000 sales, ~105 pages on
2026-10-07, against the ~11 Zillow requests a day left after Michigan's ~14
(budget 25). So the window is cut into 14 fixed slots, (borough, price band)
searches of ~5-11 pages each (`zillow.NYCSoldSlots`, boroughs taking turns),
and each run walks them in order from slot (UTC day number mod 14) until the
budget is spent: about one slot a day, so every slot is read every 14 days,
Manhattan, Brooklyn and Queens every 3-4 days, the Bronx and Staten Island
(one slot each) every 14. No cursor is kept anywhere; a run that stops
early (budget, 429) just leaves the rest to the slots' next turn. Results
are most recent first, so even a slot cut short reads its newest sales, and
a slot gains far less than a page of sales in 14 days. A slot that grows
past 780 results is still split in two the way StreetEasy's full sweep is
(Zillow serves 20 pages of 41). Sold rows stay in D1 (score-input keeps
those sold in the last 365 days), so 6-month windows hold a full year after
six months. A one-off backfill can walk every slot from a machine Zillow
does not limit as hard: `housedeals --mode sold --market nyc --sold-budget
150 --push …` (8 s apart, ~15 min). Rows: `zl:<zpid>`, `status: sold`, `compOnly`,
`soldAt`, address without the unit (the unit goes to `unit`), borough from
the ZIP, no neighbourhood. Sales under $100k (transfers, parking, typos) and
undisclosed addresses are dropped.

Zillow's type is not usable: of the sold rows matched to a StreetEasy
building, 791 Zillow `CONDO`s were co-ops and 381 condos, and `APARTMENT`
(14 condo, 8 co-op) means nothing either; no row said `COOPERATIVE`. So the
scorer, which has every NYC row, places each sold row before scoring
(`scorer/src/nyc_sold.rs`):

- building type: the StreetEasy rows at the same address in the same borough
  (normalised: unit cut off, `Avenue`/`Ave`, `Street`/`St`, `East`/`E`,
  `Fifth`/`5`, `21st`/`21`, Queens hyphens and spaces ignored), majority
  condo / co-op, a tie is no answer. Without a match only an explicit
  Zillow `COOPERATIVE` is kept; anything else becomes `other`, so the row
  joins no group (not a comp);
- neighbourhood: that building's StreetEasy neighbourhood when it has
  StreetEasy rows, else the majority of the 5 nearest StreetEasy rows within
  600 m in the same borough (ties: the nearest); none near, no group.

This needs `lat`/`lon` in score-input (added 2026-10-07). On the 2026-10-07
data (3 997 sales in 6 months: Manhattan 1 200, Brooklyn 1 057, Queens
1 231, Bronx 330, Staten Island 179) 30% got a StreetEasy type (StreetEasy
lists ~4 400 apartments, so most sold buildings have nothing for sale now),
88% a neighbourhood, and 30% (1 194) both, i.e. became comps.

**Alert rule:** ≥ 25% under baseline (the `housedeals-score` default; see
Calibration for why not 15%), ≥ 6 comps (Michigan) or ≥ 8 (NYC), not
thin, price ≤ $900k, discount ≤ 40% (bigger is refused as implausible: on the
2026-10-05 data the 15 NYC listings over 40% were older, plainer buildings
priced against newer ones, e.g. Forest Hills and Brighton Beach condos, not
40% bargains), and in
Michigan only `great_lakes` or `inland` water, from the description or the
map (unknown, `other` and `access` listings are scored but never alert).
The Zestimate is shown next to the alert as a second opinion, never used in
the score. The rule's numbers are `housedeals-score` flags with these
defaults. A listing alerts only while fresh: the Worker records `fresh_at`
when a listing is new and young in a quick crawl (not a market's first run),
relisted or cheaper, and the scorer alerts on it for 72 hours from then.

**Calibration.** `cargo run --release --features calibrate --bin calibrate
-- d1-export.sql [--water map.txt] [--sold crawled.json]`
(scorer/src/bin/calibrate.rs; SQLite only in that feature) loads a
`wrangler d1 export`, scores each market as the crawl job does (same
columns as score-input: no description) and prints the share priced, the discount distribution and
counts ≥ 15% / ≥ 40% by level, basis, type, borough or water, the refusals
by reason, and the top listings with their group, n, basis, sq ft and comps'
p25/p75. `--water` fills Michigan water from `id|type|body` lines (the map
classifier's output for the rows' lat/lon); `--sold` merges crawled
listings (a crawler `--dry-run` JSON or NDJSON, e.g. NYC sold rows) and
prints how the NYC sold rows were placed; `--set name=value` tries other
Options. Results on the 2026-10-05 export (4 345 NYC, 248 Michigan rows,
Michigan water filled from the map):

| | priced | p5 | p25 | median | p75 | p95 | ≥ 15% | ≥ 40% |
|---|---|---|---|---|---|---|---|---|
| NYC before (all types) | 4 161 / 4 345 | −57.4 | −17.9 | −0.6 | 14.8 | 34.6 | 1 026 (25%) | 117 |
| NYC after (apartments) | 905 / 2 684 | −37.1 | −15.0 | −2.7 | 9.1 | 24.8 | 133 (15%) | 0 |
| Michigan before | 231 / 248 | −104 | −38.8 | −2.8 | 18.8 | 39.8 | 66 (29%) | 11 |
| Michigan after | 213 / 248 | −73.3 | −26.8 | −4.3 | 13.4 | 30.1 | 50 (23%) | 0 |

The pile-up at the cap is gone and the middle is tighter, but about one
priced NYC listing in seven is still ≥ 15% under. That share barely moved
with any comp rule tried (size window 25-70%, min comps 8-20, spread caps
0.25-0.75, radius-based comps from lat/lon, baths matching, building
adjustment): asking prices of similar units differ by more than the
features on a search card explain. Even units of the same building with the
same beds, baths and sq ft within 10% differ by a median 5.8% in $/sq ft,
and across buildings of one neighbourhood maintenance, condition, floor
and building quality add more (comps' p25-p75 is about ±10%). So 15% under
is roughly a 1-sigma event in NYC and about a 0.7-sigma one up north. A
≥ 15% discount marks a listing worth a look, not a bargain; the alert
threshold, not the model, sets how rare alerts are (NYC's p95 is ~25%), so
the live threshold is 25% (`--min-discount` to change it).
Sharper baselines would need building-level facts (year built, maintenance
for every co-op, lat/lon in score-input for radius comps).

NYC sold comps, 2026-10-07 export (4 444 NYC rows, 2 741 apartments) with
and without the 3 997 crawled Zillow sales (1 194 of them usable comps):

| | priced | p5 | p25 | median | p75 | p95 | ≥ 15% | ≥ 25% |
|---|---|---|---|---|---|---|---|---|
| active only | 940 | −37.8 | −14.9 | −2.1 | 9.2 | 25.4 | 145 (15.4%) | 52 (5.5%) |
| with sold | 1 120 | −34.5 | −13.2 | −2.0 | 9.0 | 24.6 | 164 (14.6%) | 54 (4.8%) |

Coverage of apartments goes from 34% to 41% (Manhattan 217 → 288, Queens
361 → 415, Brooklyn 295 → 347), the lower tail tightens (more comps per
group) and the deal tail barely moves.

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
- Daily 13:15 UTC (between the quick runs, away from the 11:00 sweep):
  Michigan sold comps (12 months), then the day's NYC sold slots (6 months,
  ≤ 25 Zillow requests in all, 8 s apart), then scoring of both.

## D1 budget

The crawler reads score-input in pages of 500 rows and posts detail reads in
chunks of at most 10, to keep each Worker request's CPU small. Map water adds
no reads: ingest's existing per-batch lookup also returns the stored water.


~6 000 NYC + ~500 Michigan rows, plus NYC sales: ~650 a month, ~8 000 kept
(365 days), so every NYC scoring run reads about twice the rows it did. Unchanged rows are not rewritten; `last_seen`
is refreshed only every 20 h, so a daily sweep touches each row about once a
day (~6 500 writes) plus price changes and new listings. Score writes are
limited the same way: unchanged scores are not rewritten.

## Names (personal Cloudflare account 7377038a…)

Worker `housedeals-api`, D1 `housedeals`, Pages `housedeals`.
