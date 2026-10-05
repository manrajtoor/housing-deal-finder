// Package geo says from a map, offline, whether a northern Michigan point is
// on Great Lakes or inland lake water. Zillow search cards carry latitude
// and longitude but no water facts, and detail pages (whose description
// names the lake) are often blocked, so this gives every search card a
// waterType without a detail read.
//
// The data (water.json.gz, built by tools/geo/build_water.py) is from
// OpenStreetMap, © OpenStreetMap contributors, ODbL 1.0: the Lake Michigan
// and Lake Huron shore (mainland and islands), named inland lakes of 10 ha or
// more, and Great Lakes bay names, simplified to ~10 m.
//
// A point is great_lakes when the Great Lakes shore is within Threshold
// metres, inland when an inland lake boundary is (inside a lake counts as
// 0 m), whichever is nearer. Distances use a local flat projection, good to
// about 1% across the six counties.
package geo

import (
	"bytes"
	"compress/gzip"
	_ "embed"
	"encoding/json"
	"math"
	"sync"
)

// Water types, as in crawler/internal/water.
const (
	GreatLakes = "great_lakes"
	Inland     = "inland"
)

// Threshold is how far (metres) from the water a listing's point may sit and
// still count as waterfront. Zillow's point is the house or parcel, tens of
// metres back from the shore. Tuned on the 248 live Michigan rows
// (2026-10-05): distances pile up at 10-60 m, stay above the background to
// ~90 m and are flat beyond (about 0.4 listings per metre at 90-200 m, the
// rate of homes near but not on the water), so 90 m keeps the frontage
// cluster and leaves the across-the-road tail out.
const Threshold = 90.0

// searchRadius bounds Nearest: water farther than this is reported as +Inf.
const searchRadius = 2000.0

// cellM is the spatial grid cell size in metres.
const cellM = 500.0

//go:embed water.json.gz
var waterGz []byte

type rawDoc struct {
	Attribution string    `json:"attribution"`
	BBox        []float64 `json:"bbox"` // south, west, north, east
	Scale       float64   `json:"scale"`
	Coast       []struct {
		N string  `json:"n"`
		P []int32 `json:"p"`
	} `json:"coast"`
	Lakes []struct {
		N string    `json:"n"`
		R [][]int32 `json:"r"`
	} `json:"lakes"`
	Bays []struct {
		N string    `json:"n"`
		R [][]int32 `json:"r"`
	} `json:"bays"`
	BayPoints []struct {
		N string  `json:"n"`
		P []int32 `json:"p"`
	} `json:"bayPoints"`
}

type pt struct{ x, y float64 }

type seg struct {
	a, b pt
	f    int32 // feature index (into the kind's slice)
}

type poly struct {
	name                   string
	rings                  [][]pt
	minX, minY, maxX, maxY float64
}

type grid struct {
	cells map[[2]int32][]seg
}

func (g *grid) add(s seg) {
	x0, y0 := cellOf(math.Min(s.a.x, s.b.x), math.Min(s.a.y, s.b.y))
	x1, y1 := cellOf(math.Max(s.a.x, s.b.x), math.Max(s.a.y, s.b.y))
	for i := x0; i <= x1; i++ {
		for j := y0; j <= y1; j++ {
			k := [2]int32{i, j}
			g.cells[k] = append(g.cells[k], s)
		}
	}
}

// nearest returns the smallest distance from p to a segment within r, and
// that segment's feature (-1 when none).
func (g *grid) nearest(p pt, r float64) (float64, int32) {
	best, f := math.Inf(1), int32(-1)
	x0, y0 := cellOf(p.x-r, p.y-r)
	x1, y1 := cellOf(p.x+r, p.y+r)
	for i := x0; i <= x1; i++ {
		for j := y0; j <= y1; j++ {
			for _, s := range g.cells[[2]int32{i, j}] {
				if d := segDist(p, s.a, s.b); d < best {
					best, f = d, s.f
				}
			}
		}
	}
	if best > r {
		return math.Inf(1), -1
	}
	return best, f
}

func cellOf(x, y float64) (int32, int32) {
	return int32(math.Floor(x / cellM)), int32(math.Floor(y / cellM))
}

type index struct {
	south, west, north, east float64
	lat0, mLon, mLat         float64

	coastNames []string
	coast      grid
	lakes      []poly
	lakeGrid   grid
	bays       []poly // smallest first
	bayPoints  []struct {
		name string
		p    pt
	}
}

var (
	once sync.Once
	idx  *index
)

func load() *index {
	once.Do(func() {
		zr, err := gzip.NewReader(bytes.NewReader(waterGz))
		if err != nil {
			panic("geo: water.json.gz: " + err.Error())
		}
		var d rawDoc
		if err := json.NewDecoder(zr).Decode(&d); err != nil {
			panic("geo: water.json.gz: " + err.Error())
		}
		idx = build(&d)
	})
	return idx
}

const (
	mPerDegLat = 111132.0
	mPerDegLon = 111320.0
)

func build(d *rawDoc) *index {
	ix := &index{south: d.BBox[0], west: d.BBox[1], north: d.BBox[2], east: d.BBox[3]}
	ix.lat0 = (ix.south + ix.north) / 2
	ix.mLat = mPerDegLat
	ix.mLon = mPerDegLon * math.Cos(ix.lat0*math.Pi/180)
	ix.coast.cells = map[[2]int32][]seg{}
	ix.lakeGrid.cells = map[[2]int32][]seg{}

	pts := func(flat []int32) []pt {
		out := make([]pt, 0, len(flat)/2)
		for i := 0; i+1 < len(flat); i += 2 {
			out = append(out, ix.project(float64(flat[i])/d.Scale, float64(flat[i+1])/d.Scale))
		}
		return out
	}
	addLine := func(g *grid, line []pt, f int32) {
		for i := 0; i+1 < len(line); i++ {
			g.add(seg{line[i], line[i+1], f})
		}
	}
	mkPoly := func(name string, rings [][]int32) poly {
		p := poly{name: name, minX: math.Inf(1), minY: math.Inf(1), maxX: math.Inf(-1), maxY: math.Inf(-1)}
		for _, r := range rings {
			ring := pts(r)
			for _, q := range ring {
				p.minX, p.maxX = math.Min(p.minX, q.x), math.Max(p.maxX, q.x)
				p.minY, p.maxY = math.Min(p.minY, q.y), math.Max(p.maxY, q.y)
			}
			p.rings = append(p.rings, ring)
		}
		return p
	}

	for _, c := range d.Coast {
		f := int32(len(ix.coastNames))
		ix.coastNames = append(ix.coastNames, c.N)
		addLine(&ix.coast, pts(c.P), f)
	}
	for _, l := range d.Lakes {
		p := mkPoly(l.N, l.R)
		f := int32(len(ix.lakes))
		ix.lakes = append(ix.lakes, p)
		for _, r := range p.rings {
			addLine(&ix.lakeGrid, r, f)
		}
	}
	for _, b := range d.Bays {
		ix.bays = append(ix.bays, mkPoly(b.N, b.R))
	}
	for _, b := range d.BayPoints {
		if p := pts(b.P); len(p) == 1 {
			ix.bayPoints = append(ix.bayPoints, struct {
				name string
				p    pt
			}{b.N, p[0]})
		}
	}
	return ix
}

func (ix *index) project(lat, lon float64) pt {
	return pt{(lon - ix.west) * ix.mLon, (lat - ix.south) * ix.mLat}
}

func segDist(p, a, b pt) float64 {
	dx, dy := b.x-a.x, b.y-a.y
	l2 := dx*dx + dy*dy
	t := 0.0
	if l2 > 0 {
		t = ((p.x-a.x)*dx + (p.y-a.y)*dy) / l2
		t = math.Max(0, math.Min(1, t))
	}
	return math.Hypot(p.x-(a.x+t*dx), p.y-(a.y+t*dy))
}

// inside is an even-odd test over all rings (outer and island rings alike).
func (p *poly) inside(q pt) bool {
	if q.x < p.minX || q.x > p.maxX || q.y < p.minY || q.y > p.maxY {
		return false
	}
	in := false
	for _, r := range p.rings {
		for i := 0; i+1 < len(r); i++ {
			a, b := r[i], r[i+1]
			if (a.y > q.y) != (b.y > q.y) && q.x < a.x+(q.y-a.y)*(b.x-a.x)/(b.y-a.y) {
				in = !in
			}
		}
	}
	return in
}

func (p *poly) dist(q pt) float64 {
	if p.inside(q) {
		return 0
	}
	best := math.Inf(1)
	for _, r := range p.rings {
		for i := 0; i+1 < len(r); i++ {
			best = math.Min(best, segDist(q, r[i], r[i+1]))
		}
	}
	return best
}

// Near is what the map says about a point: the distance (metres) to the
// nearest Great Lakes shore and to the nearest inland lake (0 inside it),
// +Inf beyond 2 km, with their names.
type Near struct {
	GreatLakesM    float64
	GreatLakesBody string // a bay name when one applies, else "Lake Michigan" / "Lake Huron"
	LakeM          float64
	Lake           string
	InBBox         bool
}

// Nearest measures a point against the map. Points outside the data's
// bounding box get InBBox false and infinite distances.
func Nearest(lat, lon float64) Near {
	ix := load()
	n := Near{GreatLakesM: math.Inf(1), LakeM: math.Inf(1)}
	if math.IsNaN(lat) || math.IsNaN(lon) || lat < ix.south || lat > ix.north || lon < ix.west || lon > ix.east {
		return n
	}
	n.InBBox = true
	q := ix.project(lat, lon)

	if d, f := ix.coast.nearest(q, searchRadius); f >= 0 {
		n.GreatLakesM = d
		n.GreatLakesBody = ix.bayName(q, d, ix.coastNames[f])
	}
	for i := range ix.lakes {
		if ix.lakes[i].inside(q) {
			n.LakeM, n.Lake = 0, ix.lakes[i].name
			return n
		}
	}
	if d, f := ix.lakeGrid.nearest(q, searchRadius); f >= 0 {
		n.LakeM, n.Lake = d, ix.lakes[f].name
	}
	return n
}

// bayName names the stretch of Great Lakes shore near q: a small bay whose
// label is within 1.5 km (Suttons Bay, Bowers Harbor, Omena Bay), else an arm
// of Grand Traverse Bay mapped as an area, else a bay label within 8 km
// (Little Traverse Bay's label is 5.5 km from Petoskey), else the lake.
func (ix *index) bayName(q pt, shoreM float64, lake string) string {
	if b, d := ix.nearestBayPoint(q); d <= 1500 {
		return b
	}
	for i := range ix.bays { // smallest first: the arms before the whole bay
		if ix.bays[i].dist(q) <= shoreM+Threshold {
			return ix.bays[i].name
		}
	}
	if b, d := ix.nearestBayPoint(q); d <= 8000 {
		return b
	}
	return lake
}

func (ix *index) nearestBayPoint(q pt) (string, float64) {
	name, best := "", math.Inf(1)
	for _, b := range ix.bayPoints {
		if d := math.Hypot(b.p.x-q.x, b.p.y-q.y); d < best {
			name, best = b.name, d
		}
	}
	return name, best
}

// ClassifyAt is Classify with a chosen threshold (metres).
func ClassifyAt(lat, lon, threshold float64) (waterType, waterBody string, distanceM float64, ok bool) {
	n := Nearest(lat, lon)
	switch {
	case n.LakeM <= threshold && n.LakeM < n.GreatLakesM:
		return Inland, n.Lake, n.LakeM, true
	case n.GreatLakesM <= threshold:
		return GreatLakes, n.GreatLakesBody, n.GreatLakesM, true
	}
	return "", "", math.Min(n.LakeM, n.GreatLakesM), false
}

// Classify says what water a point is on: great_lakes (with "Lake Michigan"
// or a bay name) or inland (with the lake's name), and how far the point is
// from it. ok is false when neither is within Threshold metres or the point
// is outside the six counties' bounding box.
func Classify(lat, lon float64) (waterType, waterBody string, distanceM float64, ok bool) {
	return ClassifyAt(lat, lon, Threshold)
}

// Attribution is the data licence notice to show wherever the data is used.
func Attribution() string { return "© OpenStreetMap contributors, ODbL 1.0" }
