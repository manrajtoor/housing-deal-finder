# tools/geo

One-off data prep for `crawler/internal/geo` (Michigan water type from a
listing's latitude/longitude).

```sh
python3 tools/geo/build_water.py   # stdlib only, Python 3.9+
```

Two Overpass API queries (https://overpass-api.de/api/interpreter) over the
bbox lat 44.4–45.9, lon -86.4 to -84.6 (Grand Traverse, Leelanau, Antrim,
Benzie, Charlevoix, Emmet):

1. The member ways of the `Lake Michigan` and `Lake Huron` `natural=water`
   relations inside the bbox: the Great Lakes shore, mainland and islands
   (OSM does not tag the Great Lakes as `natural=coastline`).
2. Named `natural=water` ways and relations (kept: `water=lake|reservoir`,
   or a name with the word "Lake"; ≥ 10 ha) and named `natural=bay`
   features (bay areas for the Grand Traverse Bay arms, bay labels for the
   rest; labels inside or nearer an inland lake are dropped).

Geometry is simplified with Douglas–Peucker at 10 m and written to
`crawler/internal/geo/water.json.gz` (embedded with `go:embed`, ~170 kB).
Raw responses are cached in `tools/geo/.cache/` (git-ignored); delete it to
refetch. Re-run after OSM edits worth picking up, then `go test
./internal/geo` from `crawler/`.

## Attribution

Data © OpenStreetMap contributors, available under the Open Database
License (ODbL) 1.0: https://www.openstreetmap.org/copyright. The derived
file `crawler/internal/geo/water.json.gz` is a derivative database under the
same licence.
