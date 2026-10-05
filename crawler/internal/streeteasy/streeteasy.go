// Package streeteasy reads StreetEasy's NYC for-sale search and listing pages.
//
// Both are Next.js App Router pages: the data lives in the RSC flight payload
// (see package rsc), not in __NEXT_DATA__. A search page carries
//
//	"listingData":{"search":{...},"totalCount":5498,"edges":"$21",
//	               "pageInfo":{"currentPage":1,"hasNextPage":true,"totalPages":37}}
//
// where "$21" is a row holding the edges ({"node":"$23","__typename":
// "OrganicSaleEdge"|"FeaturedSaleEdge"|"SponsoredSaleEdge"}) and each node is
// a listing object such as
//
//	{"id":"1851487","areaName":"Beechhurst","bedroomCount":3,"buildingType":"CO_OP",
//	 "fullBathroomCount":2,"geoPoint":"$44","halfBathroomCount":0,"livingAreaSize":0,
//	 "photos":"$45","price":759000,"state":"NY","status":"ACTIVE",
//	 "street":"166-25 Powells Cove Boulevard","displayUnit":"#12F",
//	 "urlPath":"/building/166_25-powells-cove-blvd-whitestone/12f","zipCode":"11357"}
//
// The "nyc" search also returns New Jersey listings (state "NJ"); those are
// dropped. A listing page carries "pricing":{"monthlyMaintenance":3108,
// "monthlyTaxes":0,"monthlyCommonCharges":null,...} and the building's
// "yearBuilt".
package streeteasy

import (
	"fmt"
	"net/url"
	"regexp"
	"strconv"
	"strings"

	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/jsonx"
	"housedeals/crawler/internal/listing"
	"housedeals/crawler/internal/rsc"
)

// Base is the site root; listing URLs are Base + urlPath.
const Base = "https://streeteasy.com"

// MaxSearchPrice is the search ceiling: comps go above the $900k alert
// ceiling on purpose (DESIGN.md).
const MaxSearchPrice = 1500000

// SortNewest orders results by listing date, newest first (verified live:
// the page then reports "sorting":{"attribute":"LISTED_AT","direction":"DESCENDING"}).
const SortNewest = "listed_desc"

// PageCap is the most pages StreetEasy serves for one search: totalPages
// never exceeds 37 (Queens, 1 350 results at ~30 a page, reports 37), and
// page 38 answers a 307 back to page 1 (verified live 2026-10-05).
const PageCap = 37

// BandLimit is the most results a search may report and still be walked
// whole: PageCap pages of ~29-30 listings, with margin.
const BandLimit = 1000

// Search is one StreetEasy search: an area slug ("nyc", "manhattan",
// "brooklyn", "queens", "bronx", "staten-island"), 2+ bedrooms, a price band.
type Search struct {
	Area     string
	MinPrice int // 0 = no floor
	MaxPrice int
}

// CityWide is the whole-city search the quick crawl reads page 1 of.
var CityWide = Search{Area: "nyc", MaxPrice: MaxSearchPrice}

// BoroughAreas are the area slugs of the five boroughs (verified live:
// /for-sale/staten-island/... is criteria area:500, brooklyn area:300,
// manhattan area:100, queens area:400).
var BoroughAreas = []string{"manhattan", "brooklyn", "queens", "bronx", "staten-island"}

// URL returns the URL of a search page (1-based), sorted newest first so
// page 1 holds the new listings and sweeps page through a stable order.
func (s Search) URL(page int) string {
	price := fmt.Sprintf("price:%d-%d", s.MinPrice, s.MaxPrice)
	if s.MinPrice <= 0 {
		price = fmt.Sprintf("price:-%d", s.MaxPrice)
	}
	q := url.Values{}
	if page > 1 {
		q.Set("page", strconv.Itoa(page))
	}
	q.Set("sort_by", SortNewest)
	return Base + "/for-sale/" + s.Area + "/" + price + "%7Cbeds:2-?" + q.Encode()
}

// Label names the search in logs: "brooklyn $600,001-900,000".
func (s Search) Label() string {
	if s.MinPrice <= 0 && s.MaxPrice == MaxSearchPrice {
		return s.Area
	}
	return fmt.Sprintf("%s $%s-%s", s.Area, money(s.MinPrice), money(s.MaxPrice))
}

func money(n int) string {
	s := strconv.Itoa(n)
	for i := len(s) - 3; i > 0; i -= 3 {
		s = s[:i] + "," + s[i:]
	}
	return s
}

// Split halves the price band at a round number (a multiple of $25 000):
// [Min, mid] and [mid+1, Max]. ok is false when the band is too narrow.
func (s Search) Split() (lo, hi Search, ok bool) {
	const step = 25000
	mid := (s.MinPrice + s.MaxPrice) / 2 / step * step
	if mid <= s.MinPrice || mid >= s.MaxPrice {
		return s, s, false
	}
	lo, hi = s, s
	lo.MaxPrice = mid
	hi.MinPrice = mid + 1
	return lo, hi, true
}

// SearchURL returns page n of the city-wide search.
func SearchURL(page int) string { return CityWide.URL(page) }

// SearchPage is one parsed search page.
type SearchPage struct {
	Listings    []listing.Listing
	TotalCount  int
	CurrentPage int
	TotalPages  int
	HasNextPage bool
	// Dropped counts results that are not NYC listings (New Jersey) or have no price.
	Dropped int
}

// ParseSearch reads a search page.
func ParseSearch(html string) (SearchPage, error) {
	payload, err := rsc.Payload(html)
	if err != nil {
		return SearchPage{}, &fetch.ParseError{Msg: "streeteasy search: " + err.Error() + blockHint(html)}
	}
	st := rsc.Parse(payload)
	var page SearchPage

	var nodes []map[string]any
	for _, v := range st.ObjectsAfter(`"listingData":`) {
		ld := jsonx.Map(st.Resolve(v))
		if ld == nil || (ld["edges"] == nil && ld["pageInfo"] == nil) {
			continue
		}
		page.TotalCount, _ = jsonx.Int(ld["totalCount"])
		pi := jsonx.Map(ld["pageInfo"])
		page.CurrentPage, _ = jsonx.Int(pi["currentPage"])
		page.TotalPages, _ = jsonx.Int(pi["totalPages"])
		page.HasNextPage, _ = pi["hasNextPage"].(bool)
		for _, e := range jsonx.Slice(ld["edges"]) {
			if n := jsonx.Map(jsonx.Get(e, "node")); n != nil {
				nodes = append(nodes, n)
			}
		}
		break
	}
	if len(nodes) == 0 {
		// Fallback: any listing-shaped object anywhere in the stream.
		nodes = scanListingObjects(st)
	}
	if len(nodes) == 0 && page.TotalPages == 0 {
		return page, &fetch.ParseError{Msg: "streeteasy search: no listingData and no listing objects in the payload"}
	}

	areas := areaBoroughs(st.Text)
	seen := map[string]bool{}
	for _, n := range nodes {
		l, ok := toListing(n, areas)
		if !ok {
			page.Dropped++
			continue
		}
		if seen[l.ID] {
			continue // featured/sponsored edges repeat organic ones
		}
		seen[l.ID] = true
		page.Listings = append(page.Listings, l)
	}
	return page, nil
}

var listingObjRe = regexp.MustCompile(`\{"id":"\d+","advertisedOnSe"`)

func scanListingObjects(st *rsc.Stream) []map[string]any {
	var out []map[string]any
	for _, loc := range listingObjRe.FindAllStringIndex(st.Text, -1) {
		v, err := st.ValueAt(loc[0])
		if err != nil {
			continue
		}
		if m := jsonx.Map(st.Resolve(v)); m != nil {
			out = append(out, m)
		}
	}
	return out
}

func toListing(n map[string]any, areas map[string]string) (listing.Listing, bool) {
	id := jsonx.String(n["id"])
	price, _ := jsonx.Int(n["price"])
	state := jsonx.String(n["state"])
	if id == "" || price <= 0 || (state != "" && state != "NY") {
		return listing.Listing{}, false
	}
	zip := jsonx.String(n["zipCode"])
	urlPath := jsonx.String(n["urlPath"])
	area := strings.TrimSpace(jsonx.String(n["areaName"]))
	borough := Borough(zip, area, urlPath, areas)
	if borough == "new_jersey" {
		return listing.Listing{}, false
	}
	l := listing.Listing{
		ID:           "se:" + id,
		Source:       listing.SourceStreetEasy,
		Market:       listing.MarketNYC,
		Status:       listing.StatusActive,
		URL:          absURL(urlPath, id),
		Address:      strings.TrimSpace(jsonx.String(n["street"])),
		Price:        price,
		HomeType:     HomeType(jsonx.String(n["buildingType"])),
		Zip:          listing.Str(zip),
		Neighborhood: listing.Str(area),
		Borough:      listing.Str(borough),
	}
	unit := jsonx.String(n["displayUnit"])
	if unit == "" {
		unit = jsonx.String(n["unit"])
	}
	l.Unit = listing.Str(strings.TrimSpace(unit))
	if geo := jsonx.Map(n["geoPoint"]); geo != nil {
		l.Lat = jsonx.NumPtr(geo["latitude"])
		l.Lon = jsonx.NumPtr(geo["longitude"])
	}
	if b, ok := jsonx.Int(n["bedroomCount"]); ok && b >= 0 {
		l.Beds = &b
	}
	full, okF := jsonx.Float(n["fullBathroomCount"])
	half, okH := jsonx.Float(n["halfBathroomCount"])
	if (okF || okH) && full+half > 0 {
		l.Baths = listing.Ptr(full + 0.5*half)
	}
	l.Sqft = jsonx.PosInt(n["livingAreaSize"]) // 0 means unknown
	for _, p := range jsonx.Slice(n["photos"]) {
		if u := jsonx.String(jsonx.Get(p, "url")); u != "" {
			l.PhotoURL = &u
			break
		}
	}
	return l, true
}

func absURL(urlPath, id string) string {
	switch {
	case strings.HasPrefix(urlPath, "http"):
		return urlPath
	case strings.HasPrefix(urlPath, "/"):
		return Base + urlPath
	}
	return Base + "/sale/" + id
}

// HomeType maps StreetEasy's buildingType to the contract's homeType.
func HomeType(buildingType string) string {
	switch strings.ToUpper(buildingType) {
	case "CONDO", "CONDOP":
		return listing.HomeCondo
	case "CO_OP", "COOP":
		return listing.HomeCoop
	case "TOWNHOUSE":
		return listing.HomeTownhouse
	case "HOUSE", "SINGLE_FAMILY":
		return listing.HomeSingleFamily
	case "MULTI_FAMILY", "TWOFAMILY", "TWO_FAMILY", "THREEFAMILY", "THREE_FAMILY", "FOURFAMILY", "FOUR_FAMILY":
		return listing.HomeMultiFamily
	}
	return listing.HomeOther
}

// Boroughs, as the contract spells them.
const (
	Manhattan    = "manhattan"
	Brooklyn     = "brooklyn"
	Queens       = "queens"
	Bronx        = "bronx"
	StatenIsland = "staten_island"
)

// BoroughFromZip maps an NYC ZIP code to its borough by prefix.
func BoroughFromZip(zip string) string {
	if len(zip) < 3 {
		return ""
	}
	switch zip[:3] {
	case "100", "101", "102":
		return Manhattan
	case "103":
		return StatenIsland
	case "104":
		return Bronx
	case "112":
		return Brooklyn
	case "110", "111", "113", "114", "116":
		return Queens
	}
	return ""
}

// Borough derives a listing's borough: from the ZIP code, else from the
// neighbourhood's borough in the page's area list, else from the building
// slug ("...-brooklyn/", "...-new_york/" for Manhattan). "" when unknown;
// "new_jersey" for New Jersey areas.
func Borough(zip, areaName, urlPath string, areas map[string]string) string {
	if b := BoroughFromZip(zip); b != "" {
		return b
	}
	if b := areas[strings.ToLower(areaName)]; b != "" {
		return b
	}
	parts := strings.Split(strings.Trim(urlPath, "/"), "/")
	if len(parts) >= 2 && parts[0] == "building" {
		slug := parts[1]
		for suffix, b := range map[string]string{
			"-new_york": Manhattan, "-manhattan": Manhattan, "-brooklyn": Brooklyn,
			"-bronx": Bronx, "-staten_island": StatenIsland, "-queens": Queens,
		} {
			if strings.HasSuffix(slug, suffix) {
				return b
			}
		}
	}
	return ""
}

// The search page lists every area with its borough:
// {"id":"401","name":"Astoria","short":"astoria","level":2,"parentId":400,
// "mapCoordinates":{...},"borough":{"id":"400","name":"Queens","short":"queens"}}
var areaRe = regexp.MustCompile(`\{"id":"\d+","name":"((?:[^"\\]|\\.)*)","short":"[^"]*","level":\d+,"parentId":\d+,"mapCoordinates":\{(?:"encodedBoundary":"(?:[^"\\]|\\.)*")?\},"borough":\{"id":"\d+","name":"[^"]*","short":"([a-z-]+)"\}`)

func areaBoroughs(text string) map[string]string {
	out := map[string]string{}
	for _, m := range areaRe.FindAllStringSubmatch(text, -1) {
		b := strings.ReplaceAll(m[2], "-", "_")
		switch b {
		case Manhattan, Brooklyn, Queens, Bronx, StatenIsland, "new_jersey":
			out[strings.ToLower(m[1])] = b
		}
	}
	return out
}

// Detail is what a listing page adds.
type Detail struct {
	Maintenance *int // monthly co-op maintenance or condo common charges
	Taxes       *int // monthly taxes
	YearBuilt   *int
}

var yearBuiltRe = regexp.MustCompile(`"yearBuilt":(\d{4})\b`)

// ParseDetail reads a listing page.
func ParseDetail(html string) (Detail, error) {
	payload, err := rsc.Payload(html)
	if err != nil {
		return Detail{}, &fetch.ParseError{Msg: "streeteasy detail: " + err.Error() + blockHint(html)}
	}
	st := rsc.Parse(payload)
	var d Detail
	found := false
	for _, v := range st.ObjectsAfter(`"pricing":`) {
		p := jsonx.Map(st.Resolve(v))
		if p == nil {
			continue
		}
		if _, ok := p["monthlyMaintenance"]; !ok {
			if _, ok := p["monthlyCommonCharges"]; !ok {
				continue
			}
		}
		found = true
		d.Maintenance = jsonx.PosInt(p["monthlyMaintenance"])
		if d.Maintenance == nil {
			d.Maintenance = jsonx.PosInt(p["monthlyCommonCharges"])
		}
		// Co-op taxes are inside maintenance; the page then says 0.
		d.Taxes = jsonx.PosInt(p["monthlyTaxes"])
		break
	}
	if m := yearBuiltRe.FindStringSubmatch(st.Text); m != nil {
		if y, err := strconv.Atoi(m[1]); err == nil && y > 1700 && y < 2100 {
			d.YearBuilt = &y
		}
	}
	if !found && d.YearBuilt == nil {
		return d, &fetch.ParseError{Msg: "streeteasy detail: no pricing block in the payload"}
	}
	return d, nil
}

func blockHint(html string) string {
	low := strings.ToLower(html)
	if strings.Contains(low, "px-captcha") || strings.Contains(low, "press & hold") || strings.Contains(low, "access to this page has been denied") {
		return " (bot check page)"
	}
	return fmt.Sprintf(" (%d bytes)", len(html))
}
