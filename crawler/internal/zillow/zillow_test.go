package zillow

import (
	"encoding/json"
	"errors"
	"net/url"
	"os"
	"strings"
	"testing"

	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/listing"
)

func readFixture(t *testing.T, name string) string {
	t.Helper()
	b, err := os.ReadFile("testdata/" + name)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

func queryOf(t *testing.T, raw string) map[string]any {
	t.Helper()
	u, err := url.Parse(raw)
	if err != nil {
		t.Fatal(err)
	}
	var q map[string]any
	if err := json.Unmarshal([]byte(u.Query().Get("searchQueryState")), &q); err != nil {
		t.Fatal(err)
	}
	return q
}

func TestSearchURL(t *testing.T) {
	gt := Counties[0]
	u := SearchURL(gt, 2, false)
	if !strings.HasPrefix(u, "https://www.zillow.com/homes/for_sale/?searchQueryState=") {
		t.Fatal(u)
	}
	q := queryOf(t, u)
	fs := q["filterState"].(map[string]any)
	if q["pagination"].(map[string]any)["currentPage"].(float64) != 2 ||
		q["regionSelection"].([]any)[0].(map[string]any)["regionId"].(float64) != 3240 ||
		fs["price"].(map[string]any)["max"].(float64) != 1500000 ||
		fs["wat"].(map[string]any)["value"] != true || fs["land"].(map[string]any)["value"] != false ||
		fs["sort"].(map[string]any)["value"] != "days" {
		t.Errorf("query = %v", q)
	}
	if _, ok := fs["rs"]; ok {
		t.Error("for-sale search must not set rs")
	}

	sq := queryOf(t, SearchURL(gt, 1, true))
	sfs := sq["filterState"].(map[string]any)
	for _, k := range []string{"fsba", "fsbo", "nc", "cmsn", "auc", "fore"} {
		if sfs[k].(map[string]any)["value"] != false {
			t.Errorf("sold %s = %v", k, sfs[k])
		}
	}
	if sfs["rs"].(map[string]any)["value"] != true || sfs["doz"].(map[string]any)["value"] != "12m" {
		t.Errorf("sold filter = %v", sfs)
	}
}

func TestCounties(t *testing.T) {
	areas := map[string]string{}
	for _, c := range Counties {
		areas[c.Slug] = c.Area
	}
	want := map[string]string{"grand_traverse": "traverse", "leelanau": "traverse", "antrim": "traverse",
		"benzie": "traverse", "charlevoix": "petoskey", "emmet": "petoskey"}
	if len(areas) != len(want) {
		t.Fatalf("counties = %v", areas)
	}
	for k, v := range want {
		if areas[k] != v {
			t.Errorf("%s -> %s, want %s", k, areas[k], v)
		}
	}
}

// testdata/search.html: Grand Traverse County, waterfront, for sale, page 1
// of 2 (61 results), trimmed to 6 results.
func TestParseSearch(t *testing.T) {
	p, err := ParseSearch(readFixture(t, "search.html"), Counties[0], false)
	if err != nil {
		t.Fatal(err)
	}
	if p.TotalPages != 2 || p.TotalResults != 61 || len(p.Listings) != 6 || p.Dropped != 0 {
		t.Fatalf("pages %d results %d listings %d dropped %d", p.TotalPages, p.TotalResults, len(p.Listings), p.Dropped)
	}
	l := p.Listings[0]
	checks := []struct {
		name      string
		got, want any
	}{
		{"id", l.ID, "zl:94778208"},
		{"source", l.Source, "zillow"},
		{"market", l.Market, "mi"},
		{"status", l.Status, "active"},
		{"url", l.URL, "https://www.zillow.com/homedetails/10-Island-View-Dr-Traverse-City-MI-49696/94778208_zpid/"},
		{"address", l.Address, "10 Island View Dr"},
		{"city", deref(l.City), "Traverse City"},
		{"zip", deref(l.Zip), "49696"},
		{"price", l.Price, 1250000},
		{"beds", deref(l.Beds), 4},
		{"baths", deref(l.Baths), 4.0},
		{"sqft", deref(l.Sqft), 2372},
		{"lotSqft", deref(l.LotSqft), 55321}, // 1.27 acres
		{"homeType", l.HomeType, "single_family"},
		{"county", deref(l.County), "grand_traverse"},
		{"area", deref(l.Area), "traverse"},
		{"zestimate", deref(l.Zestimate), 1196800},
		{"daysOnMarket", deref(l.DaysOnMarket), 3},
		{"lat", deref(l.Lat), 44.68526},
		{"lon", deref(l.Lon), -85.44109},
		{"compOnly", l.CompOnly, false},
	}
	for _, c := range checks {
		if c.got != c.want {
			t.Errorf("%s = %v, want %v", c.name, c.got, c.want)
		}
	}
	if l.PhotoURL == nil || !strings.HasPrefix(*l.PhotoURL, "https://photos.zillowstatic.com/") {
		t.Errorf("photo = %v", l.PhotoURL)
	}
	if l.SoldAt != nil || l.WaterType != nil || l.Description != nil || l.DetailReadAt != nil {
		t.Error("a search listing must not carry sold or detail fields")
	}
	types := map[string]int{}
	for _, l := range p.Listings {
		types[l.HomeType]++
	}
	if types["condo"] != 1 || types["single_family"] != 5 {
		t.Errorf("home types = %v", types)
	}
}

// testdata/sold.html: Leelanau County, waterfront, sold in the last 12
// months, trimmed to 4 results.
func TestParseSoldSearch(t *testing.T) {
	p, err := ParseSearch(readFixture(t, "sold.html"), Counties[1], true)
	if err != nil {
		t.Fatal(err)
	}
	if len(p.Listings) != 4 || p.TotalPages != 2 {
		t.Fatalf("listings %d pages %d", len(p.Listings), p.TotalPages)
	}
	l := p.Listings[0]
	if l.ID != "zl:115981526" || l.Status != "sold" || !l.CompOnly || l.Price != 1100000 {
		t.Errorf("sold listing = %+v", l)
	}
	// dateSold 1790924400000 ms = 2026-10-02T07:00:00Z ("Sold 10/02/26").
	if deref(l.SoldAt) != "2026-10-02T07:00:00Z" {
		t.Errorf("soldAt = %v", deref(l.SoldAt))
	}
	if l.DaysOnMarket != nil {
		t.Error("sold rows carry no daysOnMarket")
	}
	if deref(l.County) != "leelanau" || deref(l.Area) != "traverse" {
		t.Errorf("county/area = %v/%v", deref(l.County), deref(l.Area))
	}
	for _, l := range p.Listings {
		if l.SoldAt == nil || l.Status != listing.StatusSold {
			t.Errorf("%s: %v %v", l.ID, l.Status, l.SoldAt)
		}
	}
}

func TestParseSearchSkipsWrongStatusAndRelaxed(t *testing.T) {
	html := `<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"searchPageState":{"cat1":{
		"searchList":{"totalPages":1},"searchResults":{"listResults":[
		{"zpid":"1","statusType":"FOR_SALE","unformattedPrice":500000,"relaxed":true,"hdpData":{"homeInfo":{}}},
		{"zpid":"2","statusType":"SOLD","unformattedPrice":500000,"hdpData":{"homeInfo":{}}},
		{"zpid":"3","statusType":"FOR_SALE","unformattedPrice":0,"hdpData":{"homeInfo":{}}},
		{"zpid":"4","statusType":"FOR_SALE","unformattedPrice":450000,"detailUrl":"/homedetails/x/4_zpid/","hdpData":{"homeInfo":{"lotAreaValue":5000,"lotAreaUnit":"sqft","homeType":"LOT","daysOnZillow":0}}}
	]}}}}}}</script>`
	p, err := ParseSearch(html, Counties[5], false)
	if err != nil {
		t.Fatal(err)
	}
	if len(p.Listings) != 1 || p.Dropped != 3 {
		t.Fatalf("listings %d dropped %d", len(p.Listings), p.Dropped)
	}
	l := p.Listings[0]
	if l.URL != "https://www.zillow.com/homedetails/x/4_zpid/" || deref(l.LotSqft) != 5000 || l.HomeType != "other" ||
		l.DaysOnMarket == nil || *l.DaysOnMarket != 0 || deref(l.Area) != "petoskey" {
		t.Errorf("listing = %+v", l)
	}
}

func TestParseSearchBlocked(t *testing.T) {
	_, err := ParseSearch(`<html><div id="px-captcha"></div></html>`, Counties[0], false)
	var pe *fetch.ParseError
	if !errors.As(err, &pe) || !strings.Contains(err.Error(), "bot check") {
		t.Fatalf("err = %v", err)
	}
}

// testdata/detail.html: 10 Island View Dr (zpid 94778208), trimmed.
func TestParseDetail(t *testing.T) {
	d, err := ParseDetail(readFixture(t, "detail.html"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(d.Description, "Welcome to your much-anticipated Northern Michigan Retreat!") {
		t.Errorf("description = %.80q", d.Description)
	}
	if deref(d.YearBuilt) != 1988 {
		t.Errorf("yearBuilt = %v", d.YearBuilt)
	}
	if d.Water.Type != "inland" || d.Water.Body != "Island Lake" || deref(d.Water.FrontageFt) != 200 {
		t.Errorf("water = %+v ft %v", d.Water, deref(d.Water.FrontageFt))
	}
}

func TestHomeType(t *testing.T) {
	for in, want := range map[string]string{
		"SINGLE_FAMILY": "single_family", "CONDO": "condo", "TOWNHOUSE": "townhouse",
		"MULTI_FAMILY": "multi_family", "MANUFACTURED": "other", "LOT": "other", "": "other",
	} {
		if got := HomeType(in); got != want {
			t.Errorf("HomeType(%q) = %q", in, got)
		}
	}
}

func deref[T any](p *T) T {
	var zero T
	if p == nil {
		return zero
	}
	return *p
}
