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
		// From the map (crawler/internal/geo); the fixture's detail page
		// agrees: "200 feet of private water frontage ... on Island Lake".
		{"waterType", deref(l.WaterType), "inland"},
		{"waterBody", deref(l.WaterBody), "Island Lake"},
		{"waterSource", deref(l.WaterSource), "map"},
	}
	for _, c := range checks {
		if c.got != c.want {
			t.Errorf("%s = %v, want %v", c.name, c.got, c.want)
		}
	}
	if l.PhotoURL == nil || !strings.HasPrefix(*l.PhotoURL, "https://photos.zillowstatic.com/") {
		t.Errorf("photo = %v", l.PhotoURL)
	}
	if l.SoldAt != nil || l.FrontageFt != nil || l.Description != nil || l.DetailReadAt != nil {
		t.Error("a search listing must not carry sold or detail fields")
	}
	types := map[string]int{}
	water := map[string]int{}
	for _, l := range p.Listings {
		types[l.HomeType]++
		water[deref(l.WaterType)]++
		if (l.WaterType == nil) != (l.WaterSource == nil) {
			t.Errorf("%s: waterType %v with waterSource %v", l.ID, l.WaterType, l.WaterSource)
		}
	}
	// 274 Bass Lake Rd is on Bass Lake, 1995 N US-31 on East Bay; the other
	// three are 150-300 m from any lake.
	if water["inland"] != 2 || water["great_lakes"] != 1 || water[""] != 3 {
		t.Errorf("water types = %v", water)
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
		"COOPERATIVE": "coop", "APARTMENT": "other",
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

func TestSoldSearchURL(t *testing.T) {
	bk := NYCBoroughs[1]
	s := NewSoldSearch(bk, "")
	if s.Window != "6m" || s.MaxPrice != 1500000 || s.Label() != "brooklyn" {
		t.Fatalf("search = %+v %q", s, s.Label())
	}
	q := queryOf(t, s.URL(3))
	fs := q["filterState"].(map[string]any)
	r := q["regionSelection"].([]any)[0].(map[string]any)
	if r["regionId"].(float64) != 37607 || r["regionType"].(float64) != 17 || q["pagination"].(map[string]any)["currentPage"].(float64) != 3 {
		t.Errorf("query = %v", q)
	}
	if fs["rs"].(map[string]any)["value"] != true || fs["doz"].(map[string]any)["value"] != "6m" ||
		fs["beds"].(map[string]any)["min"].(float64) != 2 || fs["price"].(map[string]any)["max"].(float64) != 1500000 {
		t.Errorf("filter = %v", fs)
	}
	if _, ok := fs["price"].(map[string]any)["min"]; ok {
		t.Error("a whole-borough search has no price floor")
	}
	for _, k := range []string{"fsba", "fsbo", "nc", "cmsn", "auc", "fore", "sf", "tow", "mf", "land", "manu"} {
		if fs[k].(map[string]any)["value"] != false {
			t.Errorf("%s = %v", k, fs[k])
		}
	}
	if _, ok := fs["wat"]; ok {
		t.Error("NYC searches have no waterfront filter")
	}
}

func TestSoldSearchSplit(t *testing.T) {
	s := NewSoldSearch(NYCBoroughs[2], "12m")
	lo, hi, ok := s.Split()
	if !ok || lo.MinPrice != 0 || lo.MaxPrice != 750000 || hi.MinPrice != 750001 || hi.MaxPrice != 1500000 {
		t.Fatalf("split = %+v %+v %v", lo, hi, ok)
	}
	if hi.Label() != "queens $750,001-1,500,000" || hi.Window != "12m" {
		t.Errorf("label %q window %q", hi.Label(), hi.Window)
	}
	fs := queryOf(t, hi.URL(1))["filterState"].(map[string]any)
	if p := fs["price"].(map[string]any); p["min"].(float64) != 750001 || p["max"].(float64) != 1500000 {
		t.Errorf("price = %v", p)
	}
	// Halving stops at $25 000 steps.
	narrow := SoldSearch{Borough: NYCBoroughs[0], MinPrice: 400001, MaxPrice: 425000}
	if _, _, ok := narrow.Split(); ok {
		t.Error("a $25k band must not split")
	}
}

func TestBoroughs(t *testing.T) {
	want := map[string]int{"manhattan": 12530, "brooklyn": 37607, "queens": 270915, "bronx": 17182, "staten_island": 27252}
	if len(NYCBoroughs) != len(want) {
		t.Fatal(NYCBoroughs)
	}
	for _, b := range NYCBoroughs {
		if want[b.Slug] != b.RegionID {
			t.Errorf("%s = %d", b.Slug, b.RegionID)
		}
	}
}

// testdata/nyc_sold.html: Staten Island, recently sold (6 months), 2+ beds,
// page 1 of 8 (302 results), trimmed to 9 results: CONDO and APARTMENT
// rows (Zillow types units of one building either way: 55 Austin Pl 6B is a
// CONDO, 7K an APARTMENT), and an undisclosed address.
func TestParseNYCSold(t *testing.T) {
	p, err := ParseNYCSold(readFixture(t, "nyc_sold.html"), NYCBoroughs[4])
	if err != nil {
		t.Fatal(err)
	}
	if p.TotalPages != 8 || p.TotalResults != 302 || len(p.Listings) != 8 || p.Dropped != 1 {
		t.Fatalf("pages %d results %d listings %d dropped %d", p.TotalPages, p.TotalResults, len(p.Listings), p.Dropped)
	}
	byID := map[string]listing.Listing{}
	for _, l := range p.Listings {
		byID[l.ID] = l
		if l.Market != "nyc" || l.Source != "zillow" || l.Status != "sold" || !l.CompOnly || l.SoldAt == nil ||
			deref(l.Borough) != "staten_island" || l.Neighborhood != nil || l.County != nil || l.Area != nil ||
			l.WaterType != nil || l.LotSqft != nil || l.DaysOnMarket != nil {
			t.Errorf("listing = %+v", l)
		}
	}
	l := byID["zl:32286071"]
	checks := []struct {
		name      string
		got, want any
	}{
		{"address", l.Address, "55 Austin Pl"},
		{"unit", deref(l.Unit), "#6B"},
		{"price", l.Price, 450000},
		{"beds", deref(l.Beds), 2},
		{"baths", deref(l.Baths), 2.0},
		{"sqft", deref(l.Sqft), 1122},
		{"homeType", l.HomeType, "condo"},
		{"zip", deref(l.Zip), "10304"},
		{"city", deref(l.City), "Staten Island"},
		{"url", l.URL, "https://www.zillow.com/homedetails/55-Austin-Pl-APT-6B-Staten-Island-NY-10304/32286071_zpid/"},
	}
	for _, c := range checks {
		if c.got != c.want {
			t.Errorf("%s = %v, want %v", c.name, c.got, c.want)
		}
	}
	if l.Lat == nil || l.Lon == nil || *l.Lat < 40.5 || *l.Lat > 40.7 {
		t.Errorf("lat/lon = %v %v", l.Lat, l.Lon)
	}
	// APARTMENT is not a building type: "other", for the scorer to settle.
	if a := byID["zl:32286101"]; a.HomeType != "other" || a.Address != "55 Austin Pl" || deref(a.Unit) != "#7K" {
		t.Errorf("apartment = %+v", a)
	}
	if b := byID["zl:460789199"]; b.Address != "50 Fort Pl" || deref(b.Unit) != "#B3-B/A" {
		t.Errorf("unit with a slash = %q %q", b.Address, deref(b.Unit))
	}
	if b := byID["zl:32324102"]; b.Address != "99 Stonegate Dr" || b.Unit != nil {
		t.Errorf("no unit = %q %v", b.Address, b.Unit)
	}
}

func TestParseNYCSoldDropsTransfersAndUsesTheSearchedBorough(t *testing.T) {
	html := `<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"searchPageState":{"cat1":{
		"searchList":{"totalPages":1,"totalResultCount":3},"searchResults":{"listResults":[
		{"zpid":"1","statusType":"SOLD","unformattedPrice":16273,"addressStreet":"404 Park Ave S APT 9C","hdpData":{"homeInfo":{"dateSold":1791183600000,"zipcode":"10016"}}},
		{"zpid":"2","statusType":"SOLD","unformattedPrice":700000,"addressStreet":"1 Main St UNIT 4C","hdpData":{"homeInfo":{"dateSold":1791183600000,"homeType":"COOPERATIVE"}}},
		{"zpid":"3","statusType":"FOR_SALE","unformattedPrice":700000,"addressStreet":"2 Main St","hdpData":{"homeInfo":{}}}
	]}}}}}}</script>`
	p, err := ParseNYCSold(html, NYCBoroughs[0])
	if err != nil {
		t.Fatal(err)
	}
	if len(p.Listings) != 1 || p.Dropped != 2 {
		t.Fatalf("listings %d dropped %d", len(p.Listings), p.Dropped)
	}
	l := p.Listings[0]
	if deref(l.Borough) != "manhattan" || l.HomeType != "coop" || l.Address != "1 Main St" || deref(l.Unit) != "#4C" {
		t.Errorf("listing = %+v", l)
	}
}

func TestSplitUnit(t *testing.T) {
	for in, want := range map[string][2]string{
		"1408 Avenue O APT 3B":     {"1408 Avenue O", "3B"},
		"1235 Forest Hill Rd #2E":  {"1235 Forest Hill Rd", "2E"},
		"7 Shirra Avenue #A":       {"7 Shirra Avenue", "A"},
		"150 W 51st St APT 2116":   {"150 W 51st St", "2116"},
		"10 West End Ave Unit 4c":  {"10 West End Ave", "4C"},
		"38B Jennifer Pl":          {"38B Jennifer Pl", ""},
		"1408 Avenue O":            {"1408 Avenue O", ""},
		"166-25 Powells Cove Blvd": {"166-25 Powells Cove Blvd", ""},
	} {
		a, u := SplitUnit(in)
		if a != want[0] || u != want[1] {
			t.Errorf("SplitUnit(%q) = %q, %q", in, a, u)
		}
	}
}

func TestNYCSoldSlotsCoverEachBoroughOnceWithoutGaps(t *testing.T) {
	slots := NYCSoldSlots("90")
	if len(slots) != 14 {
		t.Fatalf("slots = %d", len(slots))
	}
	next := map[string]int{} // borough -> lowest price not yet covered
	for i, s := range slots {
		if s.Window != "90" {
			t.Errorf("slot %d window %q", i, s.Window)
		}
		if s.MinPrice != next[s.Borough.Slug] || s.MaxPrice <= s.MinPrice {
			t.Errorf("slot %d %s: band starts at %d, want %d", i, s.Label(), s.MinPrice, next[s.Borough.Slug])
		}
		next[s.Borough.Slug] = s.MaxPrice + 1
		if i > 0 && slots[i-1].Borough == s.Borough {
			t.Errorf("slots %d and %d are both %s: boroughs take turns", i-1, i, s.Borough.Slug)
		}
	}
	for _, b := range NYCBoroughs {
		if next[b.Slug] != MaxPrice+1 {
			t.Errorf("%s covered up to %d", b.Slug, next[b.Slug]-1)
		}
	}
	if NYCSoldSlots("")[0].Window != NYCSoldWindow {
		t.Error("default window")
	}
}
