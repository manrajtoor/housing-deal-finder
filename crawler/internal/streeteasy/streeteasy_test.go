package streeteasy

import (
	"errors"
	"os"
	"strings"
	"testing"

	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/listing"
	"housedeals/crawler/internal/rsc"
)

func readFixture(t *testing.T, name string) string {
	t.Helper()
	b, err := os.ReadFile("testdata/" + name)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

func TestSearchURL(t *testing.T) {
	if got, want := SearchURL(1), "https://streeteasy.com/for-sale/nyc/price:-1500000%7Cbeds:2-?sort_by=listed_desc"; got != want {
		t.Errorf("page 1 = %s", got)
	}
	if got, want := SearchURL(3), "https://streeteasy.com/for-sale/nyc/price:-1500000%7Cbeds:2-?page=3&sort_by=listed_desc"; got != want {
		t.Errorf("page 3 = %s", got)
	}
}

// testdata/search.html is a trimmed copy of a live page 1 (sorted newest
// first, 2026-10-05): 8 edges, one of them a New Jersey listing.
func TestParseSearch(t *testing.T) {
	p, err := ParseSearch(readFixture(t, "search.html"))
	if err != nil {
		t.Fatal(err)
	}
	if p.TotalCount != 5496 || p.CurrentPage != 1 || p.TotalPages != 37 || !p.HasNextPage {
		t.Errorf("page info = %+v", p)
	}
	if len(p.Listings) != 7 || p.Dropped != 1 {
		t.Fatalf("listings = %d, dropped = %d", len(p.Listings), p.Dropped)
	}
	byID := map[string]listing.Listing{}
	for _, l := range p.Listings {
		byID[l.ID] = l
		if l.Source != "streeteasy" || l.Market != "nyc" || l.Status != "active" || l.CompOnly {
			t.Errorf("%s: wrong constants %+v", l.ID, l)
		}
		if l.Borough == nil {
			t.Errorf("%s: no borough", l.ID)
		}
		if l.Sqft != nil && *l.Sqft == 0 {
			t.Errorf("%s: sqft 0 must be null", l.ID)
		}
	}
	if _, ok := byID["se:1851508"]; ok {
		t.Error("the New Jersey listing (McGinley Square) must be dropped")
	}

	coop := byID["se:1851487"]
	checks := []struct {
		name      string
		got, want any
	}{
		{"url", coop.URL, "https://streeteasy.com/building/166_25-powells-cove-blvd-whitestone/12f"},
		{"address", coop.Address, "166-25 Powells Cove Boulevard"},
		{"unit", deref(coop.Unit), "#12F"},
		{"price", coop.Price, 759000},
		{"beds", deref(coop.Beds), 3},
		{"baths", deref(coop.Baths), 2.0},
		{"sqft", deref(coop.Sqft), 2000},
		{"homeType", coop.HomeType, "coop"},
		{"neighborhood", deref(coop.Neighborhood), "Beechhurst"},
		{"borough", deref(coop.Borough), "queens"},
		{"zip", deref(coop.Zip), "11357"},
	}
	for _, c := range checks {
		if c.got != c.want {
			t.Errorf("coop %s = %v, want %v", c.name, c.got, c.want)
		}
	}
	if coop.Lat == nil || *coop.Lat < 40.7 || *coop.Lat > 40.9 || coop.Lon == nil || *coop.Lon > -73.7 {
		t.Errorf("geoPoint not resolved: %v %v", coop.Lat, coop.Lon)
	}
	if coop.PhotoURL == nil || !strings.HasPrefix(*coop.PhotoURL, "https://photos.zillowstatic.com/") {
		t.Errorf("photo not resolved: %v", coop.PhotoURL)
	}

	house := byID["se:1851510"] // Farragut, HOUSE, livingAreaSize 0
	if house.HomeType != "single_family" || house.Sqft != nil || deref(house.Borough) != "brooklyn" || house.Unit != nil {
		t.Errorf("house = %+v", house)
	}
	if got := byID["se:1851483"].HomeType; got != "multi_family" {
		t.Errorf("TWOFAMILY -> %s", got)
	}
	if got := byID["se:1851492"].HomeType; got != "condo" {
		t.Errorf("CONDOP -> %s", got)
	}
	if got := deref(byID["se:1851488"].Borough); got != "bronx" {
		t.Errorf("10469 -> %s", got)
	}
	if got := deref(byID["se:1851492"].Borough); got != "manhattan" {
		t.Errorf("10011 -> %s", got)
	}
}

func TestParseSearchBlocked(t *testing.T) {
	_, err := ParseSearch(`<html><body><div id="px-captcha"></div>Press & Hold</body></html>`)
	var pe *fetch.ParseError
	if !errors.As(err, &pe) || !strings.Contains(err.Error(), "bot check") {
		t.Fatalf("err = %v", err)
	}
}

func TestBorough(t *testing.T) {
	areas := map[string]string{"woodside": "queens", "mcginley square": "new_jersey"}
	cases := []struct{ zip, area, path, want string }{
		{"10025", "", "", "manhattan"},
		{"10280", "", "", "manhattan"},
		{"10314", "", "", "staten_island"},
		{"10463", "", "", "bronx"},
		{"11211", "", "", "brooklyn"},
		{"11101", "", "", "queens"},
		{"11004", "", "", "queens"},
		{"11375", "", "", "queens"},
		{"11691", "", "", "queens"},
		{"", "Woodside", "", "queens"},
		{"07304", "McGinley Square", "", "new_jersey"},
		{"", "", "/building/3923-clarendon-road-brooklyn/sale/1851510", "brooklyn"},
		{"", "", "/building/155-east-73-street-new_york/2a", "manhattan"},
		{"", "Nowhere", "/building/the-austin/404", ""},
	}
	for _, c := range cases {
		if got := Borough(c.zip, c.area, c.path, areas); got != c.want {
			t.Errorf("Borough(%q,%q,%q) = %q, want %q", c.zip, c.area, c.path, got, c.want)
		}
	}
}

func TestAreaBoroughsFromFixture(t *testing.T) {
	payload, err := rsc.Payload(readFixture(t, "search.html"))
	if err != nil {
		t.Fatal(err)
	}
	got := areaBoroughs(payload)
	if got["woodside"] != "queens" || got["farragut"] != "brooklyn" || got["mcginley square"] != "new_jersey" {
		t.Errorf("areas = %v", got)
	}
}

func TestHomeType(t *testing.T) {
	for in, want := range map[string]string{
		"CONDO": "condo", "CO_OP": "coop", "CONDOP": "condo", "TOWNHOUSE": "townhouse",
		"HOUSE": "single_family", "MULTI_FAMILY": "multi_family", "TWOFAMILY": "multi_family",
		"HYBRID": "other", "UNKNOWN": "other", "": "other",
	} {
		if got := HomeType(in); got != want {
			t.Errorf("HomeType(%q) = %q, want %q", in, got, want)
		}
	}
}

// testdata/detail.html is a trimmed live co-op page (166-25 Powells Cove
// Boulevard #12F): pricing.monthlyMaintenance 3108, monthlyTaxes 0.
func TestParseDetail(t *testing.T) {
	d, err := ParseDetail(readFixture(t, "detail.html"))
	if err != nil {
		t.Fatal(err)
	}
	if deref(d.Maintenance) != 3108 || d.Taxes != nil || deref(d.YearBuilt) != 1963 {
		t.Errorf("detail = maint %v taxes %v year %v", d.Maintenance, d.Taxes, d.YearBuilt)
	}
}

func TestParseDetailCondoCommonCharges(t *testing.T) {
	html := `<script>self.__next_f.push([1,"5:{\"pricing\":{\"monthlyMaintenance\":null,\"monthlyTaxes\":812,\"monthlyCommonCharges\":1045,\"price\":899000}}\n"])</script>`
	d, err := ParseDetail(html)
	if err != nil {
		t.Fatal(err)
	}
	if deref(d.Maintenance) != 1045 || deref(d.Taxes) != 812 || d.YearBuilt != nil {
		t.Errorf("detail = %v %v %v", d.Maintenance, d.Taxes, d.YearBuilt)
	}
}

func deref[T any](p *T) T {
	var zero T
	if p == nil {
		return zero
	}
	return *p
}

func TestSearchBands(t *testing.T) {
	bk := Search{Area: "brooklyn", MaxPrice: MaxSearchPrice}
	if got, want := bk.URL(2), "https://streeteasy.com/for-sale/brooklyn/price:-1500000%7Cbeds:2-?page=2&sort_by=listed_desc"; got != want {
		t.Errorf("url = %s", got)
	}
	lo, hi, ok := bk.Split()
	if !ok || lo.MinPrice != 0 || lo.MaxPrice != 750000 || hi.MinPrice != 750001 || hi.MaxPrice != 1500000 {
		t.Fatalf("split = %+v %+v %v", lo, hi, ok)
	}
	if got, want := hi.URL(1), "https://streeteasy.com/for-sale/brooklyn/price:750001-1500000%7Cbeds:2-?sort_by=listed_desc"; got != want {
		t.Errorf("band url = %s", got)
	}
	if hi.Label() != "brooklyn $750,001-1,500,000" || bk.Label() != "brooklyn" {
		t.Errorf("labels %q %q", hi.Label(), bk.Label())
	}
	l2, h2, ok := hi.Split()
	if !ok || l2.MaxPrice != 1125000 || h2.MinPrice != 1125001 {
		t.Errorf("second split = %+v %+v", l2, h2)
	}
	if _, _, ok := (Search{Area: "bronx", MinPrice: 400001, MaxPrice: 425000}).Split(); ok {
		t.Error("a $25k band must not split")
	}
}
