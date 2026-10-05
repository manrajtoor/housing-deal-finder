# Design

Finds homes asking well under what comparable homes ask. Two markets:

- **NYC**: 2+ bedroom apartments (condo, co-op) for sale, alert at ≤ $900k.
- **Northern Michigan**: waterfront houses for sale (Great Lakes and inland
  lakes) in Grand Traverse, Leelanau, Antrim, Benzie (the "Traverse" area) and
  Charlevoix, Emmet (the "Petoskey" area), alert at ≤ $900k.

Same shape as `used-car-deal-finder`: a Go crawler in GitHub Actions pushes
batches to a Rust Worker, which stores them in D1, scores only the groups a
batch touched, and serves a Pages dashboard behind Cloudflare Access.

## Sources (probed from GitHub Actions, 2026-10-05)

| Source | Used for | Notes |
|---|---|---|
| StreetEasy search pages | NYC listings | Data in the Next.js RSC payload: id, areaName, bedroomCount, buildingType, livingAreaSize, price. ~86 a page. |
| Zillow search pages | Michigan listings, active and sold | `__NEXT_DATA__` → `cat1.searchResults.listResults`, 41 a page. County regions + `isWaterfront`. |
| Zillow detail pages | Michigan water facts | Only the description says which lake, how many feet and whether the frontage is private or shared, so it is parsed once per listing. |

Redfin, Realtor.com, Homes.com, LandWatch and Compass block GitHub's runners.
Craigslist (thin and noisy) and NYC DOF sold prices (no sq ft or beds for
units) are possible later additions, not part of v1.

Zillow Group's terms forbid automated access. The owner accepted that: crawls
stay small (1.5 s between requests, one process), are for personal use only,
and nothing is republished; the dashboard sits behind Access.

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

**Michigan.** Water facts come from the description: `great_lakes` (Lake
Michigan and its bays: Grand Traverse, West/East Bay, Little Traverse,
Suttons), `inland` (Torch, Elk, Glen, Leelanau, Crystal, Walloon, Charlevoix,
Burt, Crooked, …), `access` (shared/deeded/association access: never alerts),
`other` (river, pond, unknown). The price is the group median of price per sq ft
of living area × sq ft. Groups:

1. area × water type
2. all six counties × water type (thin)

Comps are active listings plus homes sold in the last 12 months. The sold ones
use their sale price, which tends to sit a little under asking, so the
baseline is conservative and alerts lean toward fewer false positives.

**Alert rule:** ≥ 15% under baseline, ≥ 6 comps (Michigan) or ≥ 8 (NYC), not
thin, price ≤ $900k, discount ≤ 50% (bigger means bad data), not `access`.
The Zestimate is shown next to the alert as a second opinion, never used in
the score.

## Schedule (Worker cron → `workflow_dispatch`)

- Every 30 min: newest page of each search (NYC and Michigan), new detail pages.
- Daily 11:00 UTC: full sweep of every page (keeps `last_seen` honest), plus
  expiry of listings unseen for 3 days.
- Weekly: Michigan sold comps.

## D1 budget

~6 000 NYC + ~500 Michigan rows. Unchanged rows are not rewritten; `last_seen`
is refreshed only every 20 h, so a daily sweep touches each row about once a
day (~6 500 writes) plus price changes and new listings.

## Names (personal Cloudflare account 7377038a…)

Worker `housedeals-api`, D1 `housedeals`, Pages `housedeals`.
