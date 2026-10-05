// Package zillow reads Zillow search and home-detail pages for the six
// northern Michigan counties.
//
// Search pages embed <script id="__NEXT_DATA__"> with
// props.pageProps.searchPageState.cat1.searchResults.listResults (41 a page)
// and cat1.searchList.totalPages. The search is a county region (regionType
// 4) with Zillow's waterfront filter; no mapBounds are needed, Zillow fills
// them in from the region (verified live 2026-10-05).
//
// Detail pages embed __NEXT_DATA__ too; props.pageProps.componentProps.
// gdpClientCache is a JSON string whose entries hold "property" with the
// agent's description, yearBuilt and resoFacts.
package zillow

import (
	"encoding/json"
	"fmt"
	"net/url"
	"regexp"
	"strings"
	"time"

	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/geo"
	"housedeals/crawler/internal/jsonx"
	"housedeals/crawler/internal/listing"
	"housedeals/crawler/internal/water"
)

// Base is the site root.
const Base = "https://www.zillow.com"

// MaxPrice is the search ceiling: comps go above the $900k alert ceiling.
const MaxPrice = 1500000

// County is a Zillow county region.
type County struct {
	Slug     string // contract county name
	RegionID int
	Area     string // traverse or petoskey
}

// Counties are the six counties of the two Michigan areas.
var Counties = []County{
	{"grand_traverse", 3240, "traverse"},
	{"leelanau", 2390, "traverse"},
	{"antrim", 833, "traverse"},
	{"benzie", 873, "traverse"},
	{"charlevoix", 2898, "petoskey"},
	{"emmet", 501, "petoskey"},
}

type boolValue struct {
	Value bool `json:"value"`
}

type filterState struct {
	Price struct {
		Max int `json:"max"`
	} `json:"price"`
	Wat  boolValue `json:"wat"`
	Land boolValue `json:"land"`
	Sort struct {
		Value string `json:"value"`
	} `json:"sort"`
	// Sold searches: recently sold on, every for-sale status off.
	RS   *boolValue `json:"rs,omitempty"`
	FSBA *boolValue `json:"fsba,omitempty"`
	FSBO *boolValue `json:"fsbo,omitempty"`
	NC   *boolValue `json:"nc,omitempty"`
	CMSN *boolValue `json:"cmsn,omitempty"`
	AUC  *boolValue `json:"auc,omitempty"`
	Fore *boolValue `json:"fore,omitempty"`
	DOZ  *struct {
		Value string `json:"value"`
	} `json:"doz,omitempty"`
}

type queryState struct {
	Pagination struct {
		CurrentPage int `json:"currentPage"`
	} `json:"pagination"`
	IsMapVisible    bool `json:"isMapVisible"`
	RegionSelection []struct {
		RegionID   int `json:"regionId"`
		RegionType int `json:"regionType"`
	} `json:"regionSelection"`
	FilterState   filterState `json:"filterState"`
	IsListVisible bool        `json:"isListVisible"`
}

// SearchURL returns the URL of a county search page (1-based), newest
// first. With sold set it lists homes sold in the last 12 months instead.
func SearchURL(c County, page int, sold bool) string {
	var q queryState
	q.Pagination.CurrentPage = max(page, 1)
	q.RegionSelection = append(q.RegionSelection, struct {
		RegionID   int `json:"regionId"`
		RegionType int `json:"regionType"`
	}{c.RegionID, 4})
	q.FilterState.Price.Max = MaxPrice
	q.FilterState.Wat.Value = true
	q.FilterState.Land.Value = false
	q.FilterState.Sort.Value = "days"
	if sold {
		on, off := &boolValue{true}, &boolValue{false}
		q.FilterState.RS = on
		q.FilterState.FSBA, q.FilterState.FSBO, q.FilterState.NC = off, off, off
		q.FilterState.CMSN, q.FilterState.AUC, q.FilterState.Fore = off, off, off
		q.FilterState.DOZ = &struct {
			Value string `json:"value"`
		}{"12m"}
	}
	q.IsListVisible = true
	b, _ := json.Marshal(q)
	return Base + "/homes/for_sale/?searchQueryState=" + url.QueryEscape(string(b))
}

// SearchPage is one parsed search page.
type SearchPage struct {
	Listings     []listing.Listing
	TotalPages   int
	TotalResults int
	Dropped      int // results outside the search ("relaxed"), of another status, or without a price
}

var nextDataRe = regexp.MustCompile(`(?s)<script id="__NEXT_DATA__"[^>]*>(.*?)</script>`)

func nextData(html, what string) (any, error) {
	m := nextDataRe.FindStringSubmatch(html)
	if m == nil {
		return nil, &fetch.ParseError{Msg: "zillow " + what + ": no __NEXT_DATA__ in the page" + blockHint(html)}
	}
	dec := json.NewDecoder(strings.NewReader(m[1]))
	dec.UseNumber()
	var v any
	if err := dec.Decode(&v); err != nil {
		return nil, &fetch.ParseError{Msg: "zillow " + what + ": __NEXT_DATA__ is not JSON: " + err.Error()}
	}
	return v, nil
}

// ParseSearch reads a search page of county c.
func ParseSearch(html string, c County, sold bool) (SearchPage, error) {
	nd, err := nextData(html, "search")
	if err != nil {
		return SearchPage{}, err
	}
	cat := jsonx.Get(nd, "props", "pageProps", "searchPageState", "cat1")
	if cat == nil {
		return SearchPage{}, &fetch.ParseError{Msg: "zillow search: no searchPageState.cat1 in __NEXT_DATA__"}
	}
	var p SearchPage
	p.TotalPages, _ = jsonx.Int(jsonx.Get(cat, "searchList", "totalPages"))
	p.TotalResults, _ = jsonx.Int(jsonx.Get(cat, "searchList", "totalResultCount"))
	results := jsonx.Get(cat, "searchResults", "listResults")
	if results == nil {
		return p, &fetch.ParseError{Msg: "zillow search: no searchResults.listResults"}
	}
	seen := map[string]bool{}
	for _, r := range jsonx.Slice(results) {
		l, ok := toListing(jsonx.Map(r), c, sold)
		if !ok {
			p.Dropped++
			continue
		}
		if !seen[l.ID] {
			seen[l.ID] = true
			p.Listings = append(p.Listings, l)
		}
	}
	return p, nil
}

func toListing(r map[string]any, c County, sold bool) (listing.Listing, bool) {
	if r == nil {
		return listing.Listing{}, false
	}
	if relaxed, _ := r["relaxed"].(bool); relaxed {
		return listing.Listing{}, false
	}
	status := jsonx.String(r["statusType"])
	if (sold && status != "SOLD") || (!sold && status != "" && status != "FOR_SALE") {
		return listing.Listing{}, false
	}
	hi := jsonx.Map(jsonx.Get(r, "hdpData", "homeInfo"))
	zpid := jsonx.String(r["zpid"])
	if zpid == "" {
		if n, ok := jsonx.Int(hi["zpid"]); ok {
			zpid = fmt.Sprint(n)
		}
	}
	price, _ := jsonx.Int(firstOf(r["unformattedPrice"], hi["price"]))
	if zpid == "" || price <= 0 {
		return listing.Listing{}, false
	}
	l := listing.Listing{
		ID:       "zl:" + zpid,
		Source:   listing.SourceZillow,
		Market:   listing.MarketMI,
		Status:   listing.StatusActive,
		URL:      absURL(jsonx.String(r["detailUrl"]), zpid),
		Address:  strings.TrimSpace(jsonx.String(firstOf(r["addressStreet"], hi["streetAddress"], r["address"]))),
		Price:    price,
		HomeType: HomeType(jsonx.String(hi["homeType"])),
		City:     listing.Str(jsonx.String(firstOf(r["addressCity"], hi["city"]))),
		Zip:      listing.Str(jsonx.String(firstOf(r["addressZipcode"], hi["zipcode"]))),
		County:   listing.Str(c.Slug),
		Area:     listing.Str(c.Area),
		PhotoURL: listing.Str(jsonx.String(r["imgSrc"])),
	}
	l.Lat = jsonx.NumPtr(hi["latitude"])
	l.Lon = jsonx.NumPtr(hi["longitude"])
	if l.Lat == nil {
		l.Lat = jsonx.NumPtr(jsonx.Get(r, "latLong", "latitude"))
		l.Lon = jsonx.NumPtr(jsonx.Get(r, "latLong", "longitude"))
	}
	if l.Lat != nil && l.Lon != nil {
		// Water from the map; a detail read's description replaces it later.
		if typ, body, _, ok := geo.Classify(*l.Lat, *l.Lon); ok {
			l.WaterType, l.WaterBody = listing.Str(typ), listing.Str(body)
			l.WaterSource = listing.Ptr(listing.WaterFromMap)
		}
	}
	if b, ok := jsonx.Int(firstOf(hi["bedrooms"], r["beds"])); ok && b >= 0 {
		l.Beds = &b
	}
	l.Baths = jsonx.PosFloat(firstOf(hi["bathrooms"], r["baths"]))
	l.Sqft = jsonx.PosInt(firstOf(hi["livingArea"], r["area"]))
	l.LotSqft = lotSqft(hi["lotAreaValue"], jsonx.String(hi["lotAreaUnit"]))
	l.Zestimate = jsonx.PosInt(firstOf(hi["zestimate"], r["zestimate"]))
	if sold {
		l.Status = listing.StatusSold
		l.CompOnly = true
		if ms, ok := jsonx.Float(hi["dateSold"]); ok && ms > 0 {
			l.SoldAt = listing.Ptr(listing.Timestamp(time.UnixMilli(int64(ms))))
		}
	} else if d, ok := jsonx.Int(hi["daysOnZillow"]); ok && d >= 0 {
		l.DaysOnMarket = &d
	}
	return l, true
}

// firstOf returns the first value that is neither null nor "".
func firstOf(vals ...any) any {
	for _, v := range vals {
		if v != nil && v != "" {
			return v
		}
	}
	return nil
}

func lotSqft(v any, unit string) *int {
	f, ok := jsonx.Float(v)
	if !ok || f <= 0 {
		return nil
	}
	switch strings.ToLower(unit) {
	case "acres", "acre":
		f *= 43560
	case "sqft", "":
	default:
		return nil
	}
	n := int(f + 0.5)
	return &n
}

func absURL(u, zpid string) string {
	switch {
	case strings.HasPrefix(u, "http"):
		return u
	case strings.HasPrefix(u, "/"):
		return Base + u
	}
	return Base + "/homedetails/" + zpid + "_zpid/"
}

// HomeType maps Zillow's homeType to the contract's.
func HomeType(t string) string {
	switch strings.ToUpper(strings.ReplaceAll(t, " ", "_")) {
	case "SINGLE_FAMILY", "SINGLEFAMILY":
		return listing.HomeSingleFamily
	case "CONDO":
		return listing.HomeCondo
	case "TOWNHOUSE":
		return listing.HomeTownhouse
	case "MULTI_FAMILY", "MULTIFAMILY":
		return listing.HomeMultiFamily
	case "COOPERATIVE", "COOP":
		return listing.HomeCoop
	}
	return listing.HomeOther
}

// Detail is what a home-detail page adds.
type Detail struct {
	Description string
	YearBuilt   *int
	Water       water.Facts
}

// ParseDetail reads a home-detail page.
func ParseDetail(html string) (Detail, error) {
	nd, err := nextData(html, "detail")
	if err != nil {
		return Detail{}, err
	}
	cache := jsonx.Get(nd, "props", "pageProps", "componentProps", "gdpClientCache")
	if s, ok := cache.(string); ok {
		dec := json.NewDecoder(strings.NewReader(s))
		dec.UseNumber()
		var v any
		if err := dec.Decode(&v); err == nil {
			cache = v
		}
	}
	var prop map[string]any
	for _, v := range jsonx.Map(cache) {
		if p := jsonx.Map(jsonx.Get(v, "property")); p != nil {
			prop = p
			break
		}
	}
	if prop == nil {
		return Detail{}, &fetch.ParseError{Msg: "zillow detail: no property in gdpClientCache"}
	}
	var d Detail
	d.Description = strings.TrimSpace(jsonx.String(prop["description"]))
	d.YearBuilt = year(prop["yearBuilt"])
	if d.YearBuilt == nil {
		d.YearBuilt = year(jsonx.Get(prop, "resoFacts", "yearBuilt"))
	}
	d.Water = water.Classify(d.Description)
	return d, nil
}

func year(v any) *int {
	n, ok := jsonx.Int(v)
	if !ok || n < 1700 || n > 2100 {
		return nil
	}
	return &n
}

func blockHint(html string) string {
	low := strings.ToLower(html)
	if strings.Contains(low, "px-captcha") || strings.Contains(low, "press & hold") || strings.Contains(low, "captcha") {
		return " (bot check page)"
	}
	return fmt.Sprintf(" (%d bytes)", len(html))
}
