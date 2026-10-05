package geo

import (
	"math"
	"testing"
	"time"
)

func TestClassify(t *testing.T) {
	cases := []struct {
		name     string
		lat, lon float64
		typ      string // "" = not ok
		body     string
		maxM     float64
	}{
		// ~40 m inland of OSM shore vertices.
		{"Torch Lake east shore", 44.9515, -85.2795, Inland, "Torch Lake", 90},
		{"West Grand Traverse Bay, Traverse City West End", 44.77115, -85.63797, GreatLakes, "West Grand Traverse Bay", 90},
		{"Little Traverse Bay, Petoskey", 45.37545, -84.96054, GreatLakes, "Little Traverse Bay", 90},
		{"Lake Charlevoix is inland", 45.1580, -85.1300, Inland, "Lake Charlevoix", 90},
		// Inside a lake polygon counts as 0 m.
		{"in Torch Lake", 44.9600, -85.2960, Inland, "Torch Lake", 0},
		{"in Big Glen Lake", 44.8700, -85.9800, Inland, "Glen Lake", 0},
		// Away from water.
		{"downtown Traverse City, Front St", 44.7633, -85.6215, "", "", 0},
		{"Central Traverse City", 44.7570, -85.6300, "", "", 0},
		{"near Hanley Lake but 280 m off", 45.0950, -85.2600, "", "", 0},
		{"Detroit (outside the data)", 42.33, -83.05, "", "", 0},
	}
	for _, c := range cases {
		typ, body, d, ok := Classify(c.lat, c.lon)
		if c.typ == "" {
			if ok {
				t.Errorf("%s: got %s %q at %.0f m, want none", c.name, typ, body, d)
			}
			continue
		}
		if !ok || typ != c.typ || body != c.body || d > c.maxM {
			t.Errorf("%s: got %s %q %.0f m ok=%v, want %s %q ≤ %.0f m", c.name, typ, body, d, ok, c.typ, c.body, c.maxM)
		}
	}
}

func TestThresholdAndNearest(t *testing.T) {
	// 250 m off the Hanley Lake shore: no at 90 m, yes at 300 m.
	if _, _, _, ok := ClassifyAt(45.0950, -85.2600, 90); ok {
		t.Error("ClassifyAt 90 m: want none")
	}
	if typ, body, _, ok := ClassifyAt(45.0950, -85.2600, 300); !ok || typ != Inland || body != "Hanley Lake" {
		t.Errorf("ClassifyAt 300 m: got %s %q %v", typ, body, ok)
	}
	n := Nearest(42.33, -83.05)
	if n.InBBox || !math.IsInf(n.GreatLakesM, 1) || !math.IsInf(n.LakeM, 1) {
		t.Errorf("Nearest outside bbox: %+v", n)
	}
	if n := Nearest(math.NaN(), -85.6); n.InBBox {
		t.Error("NaN lat should be outside")
	}
}

func TestFast(t *testing.T) {
	load()
	start := time.Now()
	for i := 0; i < 300; i++ {
		lat := 44.5 + float64(i%30)*0.045
		lon := -86.3 + float64(i/30)*0.17
		Classify(lat, lon)
	}
	if el := time.Since(start); el > 2*time.Second {
		t.Errorf("300 points took %v", el)
	}
}

func TestDataLoads(t *testing.T) {
	ix := load()
	if len(ix.coastNames) == 0 || len(ix.lakes) < 100 || len(ix.bays) == 0 {
		t.Fatalf("data looks empty: %d coast, %d lakes, %d bays", len(ix.coastNames), len(ix.lakes), len(ix.bays))
	}
}
