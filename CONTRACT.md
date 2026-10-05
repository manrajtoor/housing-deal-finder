# Contract between crawler, Worker and dashboard

All money is whole US dollars, all times ISO-8601 UTC strings.

## Listing (JSON, camelCase)

| Field | Type | Notes |
|---|---|---|
| `id` | string | `se:<streeteasy id>` or `zl:<zpid>`. Primary key. |
| `source` | `"streeteasy"` \| `"zillow"` | |
| `market` | `"nyc"` \| `"mi"` | |
| `status` | `"active"` \| `"sold"` | Sold rows are comps only (Michigan). |
| `url` | string | Absolute https URL of the listing. |
| `address` | string | Street address. |
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
| `homeType` | string | `condo` `coop` `townhouse` `single_family` `multi_family` `other` |
| `neighborhood` | string? | NYC: StreetEasy `areaName`. |
| `borough` | string? | NYC: `manhattan` `brooklyn` `queens` `bronx` `staten_island`. |
| `county` | string? | Michigan: `grand_traverse` `leelanau` `antrim` `benzie` `charlevoix` `emmet`. |
| `area` | string? | Michigan: `traverse` (GT, Leelanau, Antrim, Benzie) or `petoskey` (Charlevoix, Emmet). |
| `zestimate` | int? | Second opinion only. |
| `daysOnMarket` | int? | As the site reports it when crawled. |
| `photoUrl` | string? | First photo. |
| `description` | string? | Up to 3 000 chars. Michigan detail pages; omitted when not read. |
| `waterType` | string? | Michigan: `great_lakes` `inland` `access` `other`. Null until the detail page is read. |
| `waterBody` | string? | E.g. `Torch Lake`, `West Grand Traverse Bay`. |
| `frontageFt` | int? | Private frontage in feet, when the description states it. |
| `maintenance` | int? | NYC monthly co-op maintenance or condo common charges. |
| `taxes` | int? | NYC monthly taxes. |
| `detailReadAt` | string? | Set when the detail page was read. Omitted fields with a null `detailReadAt` must not wipe stored detail fields. |
| `compOnly` | bool | True for listings the crawler knows are old (sold rows, back pages of a first load). Never fresh, never alerts. |

## Worker HTTP API (`housedeals-api`)

Crawler routes need `Authorization: Bearer <INGEST_TOKEN>`.

- `POST /api/listings` body `{"listings":[Listing…], "scope":{"source","market","mode":"quick|full|sold","seenAt"}}`
  (≤ 200 listings a request; the crawler sends 25). Returns
  `{"seen","added","updated","unchanged","priceDrops","priceRises","relisted","scored","newAlerts"}`.
- `POST /api/listings/needs-detail` body `{"market":"mi"|"nyc","limit":N}` → `{"ids":[…], "urls":[…]}`:
  - Michigan: active listings whose `detailReadAt` is null.
  - NYC: active listings ≤ $900k scoring ≥ 10% under baseline whose `detailReadAt` is null.
- `POST /api/listings/detail` body `{"listings":[{id, detailReadAt, description?, waterType?, waterBody?, frontageFt?, maintenance?, taxes?, yearBuilt?}]}` → `{"updated":N,"scored":N,"newAlerts":N}`.

Read routes (only from the Pages service binding, host `housedeals-api.internal`, unless `PUBLIC_READ_API=true`):

- `GET /api/health`: open.
- `GET /api/deals?market=nyc|mi&maxPrice=&minDiscount=&limit=` → `{"deals":[Deal…]}`, best discount first.
- `GET /api/alerts?market=&limit=` → `{"alerts":[Deal & {createdAt}…]}`, newest first.
- `GET /api/stats` → `{"counts":[{"market","status","n"}…], "crawls":[{"market","mode","at"}…]}` (last crawl per market and mode).

## Deal (scorer output, stored in `listing_scores.deal` as JSON)

Listing fields shown on the dashboard (`id url address unit city neighborhood borough county area price beds baths sqft lotSqft homeType zestimate daysOnMarket photoUrl waterType waterBody frontageFt maintenance taxes`) plus:

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
| `*/30 * * * *` | `quick`: newest page per search, then needs-detail (≤ 15 detail pages). |
| `0 11 * * *` | `full`: every page per search, then needs-detail (≤ 40). The Worker also expires active listings unseen for 3 days in this same cron. |
| `0 12 * * SUN` | `sold`: Michigan sold in the last 12 months. |
