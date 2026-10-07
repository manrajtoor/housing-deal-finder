# Contract between crawler, Worker and dashboard

All money is whole US dollars, all times ISO-8601 UTC strings.

## Listing (JSON, camelCase)

| Field | Type | Notes |
|---|---|---|
| `id` | string | `se:<streeteasy id>` or `zl:<zpid>`. Primary key. |
| `source` | `"streeteasy"` \| `"zillow"` | |
| `market` | `"nyc"` \| `"mi"` | |
| `status` | `"active"` \| `"sold"` | Sold rows are comps only (Michigan; NYC from Zillow's recently sold search). |
| `url` | string | Absolute https URL of the listing. |
| `address` | string | Street address (NYC Zillow sold rows: without the unit, which goes to `unit`). |
| `unit` | string? | NYC unit, e.g. `#4D`. |
| `city` | string? | |
| `zip` | string? | |
| `lat`, `lon` | number? | |
| `price` | int | Asking price, or sale price when `status = "sold"`. |
| `soldAt` | string? | Sale date, sold rows only. |
| `beds` | int? | |
| `baths` | number? | Full + 0.5 × half. |
| `sqft` | int? | Living area. Null when unknown (never 0). |
| `lotSqft` | int? | Lot area in sq ft (acres × 43 560). |
| `yearBuilt` | int? | |
| `homeType` | string | `condo` `coop` `townhouse` `single_family` `multi_family` `other`. Zillow: `CONDO` → `condo`, `COOPERATIVE` → `coop`, `APARTMENT` → `other` (NYC sold rows: the scorer settles the type from StreetEasy, see DESIGN.md "NYC sold comps"). |
| `neighborhood` | string? | NYC: StreetEasy `areaName`. Null on NYC sold rows (Zillow); the scorer places them. |
| `borough` | string? | NYC: `manhattan` `brooklyn` `queens` `bronx` `staten_island` (Zillow sold rows: from the ZIP, else the searched borough). |
| `county` | string? | Michigan: `grand_traverse` `leelanau` `antrim` `benzie` `charlevoix` `emmet`. |
| `area` | string? | Michigan: `traverse` (GT, Leelanau, Antrim, Benzie) or `petoskey` (Charlevoix, Emmet). |
| `zestimate` | int? | Second opinion only. |
| `daysOnMarket` | int? | As the site reports it when crawled. |
| `photoUrl` | string? | First photo. |
| `description` | string? | Up to 3 000 chars. Michigan detail pages; omitted when not read. |
| `waterType` | string? | Michigan: `great_lakes` `inland` `access` `other`. From the map on a search card (`great_lakes` / `inland` only) or the description. Null when neither says. |
| `waterBody` | string? | E.g. `Torch Lake`, `West Grand Traverse Bay`. |
| `waterSource` | string? | `map` (search card: OpenStreetMap geography from `lat`/`lon`, crawler/internal/geo) or `description` (detail page). A card's water is accepted only with `waterSource: "map"` and stored only while the row's `detailReadAt` is null; a detail read that names a water type replaces type, body and source; one that names none keeps them. |
| `frontageFt` | int? | Private frontage in feet, when the description states it. |
| `maintenance` | int? | NYC monthly co-op maintenance or condo common charges. |
| `taxes` | int? | NYC monthly taxes. |
| `detailReadAt` | string? | Set when the detail page was read. Omitted fields with a null `detailReadAt` must not wipe stored detail fields (map water aside, see `waterSource`). |
| `compOnly` | bool | True for listings the crawler knows are old (sold rows, back pages of a first load). Never fresh, never alerts. |

## Worker HTTP API (`housedeals-api`)

Crawler routes need `Authorization: Bearer <INGEST_TOKEN>`.

- `POST /api/listings` body `{"listings":[Listing…], "scope":{"source","market","mode":"quick|full|sold","seenAt"}}`
  (≤ 200 listings a request; the crawler sends 25). Stores only: no scoring,
  no alerts. Returns
  `{"seen","added","updated","unchanged","priceDrops","priceRises","relisted","scored","newAlerts","rowsWritten"}`
  (`scored` and `newAlerts` are always 0, kept for older crawlers).
- `POST /api/listings/needs-detail` body `{"market":"mi"|"nyc","limit":N}` → `{"ids":[…], "urls":[…]}`:
  - Michigan: active listings whose `detailReadAt` is null.
  - NYC: active listings ≤ $900k whose stored score (written by the crawl job) is ≥ 10% under baseline and whose `detailReadAt` is null.
- `POST /api/listings/detail` body `{"listings":[{id, detailReadAt, description?, waterType?, waterBody?, waterSource?, frontageFt?, maintenance?, taxes?, yearBuilt?}]}`
  (the crawler sends ≤ 10 a request; the Worker sets `waterSource` to `description` whenever `waterType` is given) → `{"updated","unknown","scored":0,"newAlerts":0,"rowsWritten"}`. Stores only.
- `GET /api/score-input?market=nyc|mi&after=<id>&limit=<≤1000>` (the crawler asks 500) → `{"market","columns":[…],"rows":[[…]…],"last","n"}`:
  one page, by id, of the market's active unremoved listings and homes sold in the last 365 days.
  Each row is an array in `columns` order: the Deal's listing fields, `lat lon` (to place NYC sold rows), `status soldAt removedAt compOnly freshAt detailReadAt`,
  and the stored score `storedDiscount storedAlert storedPrice scoredAt` (null when none). The next page asks `after=<last>`;
  a page with `n` < `limit` is the last. SQLite builds the whole body.
- `POST /api/scores` body `{"market","upserts":[{"id","price","discountPct","alert","deal":Deal}],"deletes":["id"…],"alerts":[{"id","price","deal":Deal}]}`
  (≤ 100 items; the crawler sends ≤ 50, counting an alert twice) → `{"upserted","deleted","newAlerts","patchedAlerts","rowsWritten"}`.
  Upserts and alerts apply only to active unremoved listings of the market. Alerts are `INSERT OR IGNORE` keyed by
  (id, price); a repeated alert refreshes the stored Deal (`patchedAlerts`), so detail fields read later show up.

Read routes (only from the Pages service binding, host `housedeals-api.internal`, unless `PUBLIC_READ_API=true`):

- `GET /api/health`: open.
- `GET /api/deals?market=nyc|mi&maxPrice=&minDiscount=&limit=` → `{"deals":[Deal…]}`, best discount first.
- `GET /api/alerts?market=&limit=` → `{"alerts":[Deal & {createdAt}…]}`, newest first.
- `GET /api/stats` → `{"generatedAt","counts":[{"market","status","n"}…], "crawls":[{"market","mode","at"}…], "alerts":[{"market","n"}…], "scored":[{"market","n"}…]}` (last crawl per market and mode).

Read bodies are built by SQLite from the stored Deal JSON and passed through.

## Scorer CLI (`housedeals-score`, scorer/)

Reads `{"market","now","columns","rows"}` (the score-input pages joined) on stdin and writes
`{"market","upserts","deletes","alerts","stats"}` on stdout (`stats.nycSold`: NYC sold rows, how many got a
StreetEasy building type, a neighbourhood, and both). Flags: `--min-discount 15 --min-comps-nyc 8
--min-comps-mi 6 --max-price 900000 --fresh-hours 72`. No network.

## Deal (scorer output, stored in `listing_scores.deal` as JSON)

Listing fields shown on the dashboard (`id url address unit city neighborhood borough county area price beds baths sqft lotSqft homeType zestimate daysOnMarket photoUrl waterType waterBody waterSource frontageFt maintenance taxes`) plus:

| Field | Notes |
|---|---|
| `baseline` | Estimated price from comps. |
| `discountPct` | `(baseline − price) / baseline × 100`, one decimal. |
| `basis` | `"ppsf"` (price per sq ft) or `"price"` (median price). |
| `group` | Human label of the comp group, e.g. `Astoria · condo · 2bd` or `Traverse · inland`. |
| `n` | Number of comps (excluding the listing itself). |
| `thin` | True when a fallback group was used. |
| `medianPpsf` | When `basis = "ppsf"`. |
| `p25`, `p75` | Of the comp metric (ppsf or price). |
| `alert` | Whether the alert rule accepts it. |

## Worker crons → GitHub `workflow_dispatch` on `crawl.yml`

| Cron | Input `mode` |
|---|---|
| `*/30 * * * *` | `quick`: newest page per search, then needs-detail (≤ 6 detail pages per market), then scoring. |
| `0 11 * * *` | `full`: every page per search, then needs-detail (≤ 20), then scoring. The Worker also expires active listings unseen for 3 days in this same cron and deletes the scores of removed listings. |
| `0 12 * * SUN` | `sold`: Michigan sold in the last 12 months and NYC apartments sold in the last 6 months (Zillow, ≤ 150 requests), then scoring of both. |
