// Package zillow reads Zillow search and home-detail pages for the six
// northern Michigan counties, and Zillow's recently-sold search for the five
// NYC boroughs (sold comps; see SoldSearch).
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
	"housedeals/crawler/internal/streeteasy"
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

// NYCBorough is a Zillow borough region (regionType 17). Slug is the
// contract's borough name.
type NYCBorough struct {
	Slug     string
	RegionID int
}

// NYCBoroughs: the regionSelection of https://www.zillow.com/<borough>-new-york-ny/
// (manhattan-, brooklyn-, queens-, bronx-, staten-island-), verified live
// 2026-10-07. ("new-york-ny" is the city, region 6181 type 6.)
var NYCBoroughs = []NYCBorough{
	{"manhattan", 12530},
	{"brooklyn", 37607},
	{"queens", 270915},
	{"bronx", 17182},
	{"staten_island", 27252},
}

// NYC recently-sold searches.
const (
	// SoldPageCap is the most pages Zillow serves for one search
	// (totalPages is capped at 20, 41 results a page).
	SoldPageCap = 20
	// SoldBandLimit is the most results a search may report and still be
	// walked whole: 20 pages × 41 = 820, with margin.
	SoldBandLimit = 780
	// NYCSoldWindow is the sold crawl's window (Zillow "doz"). Sold rows
	// stay in D1 and score-input keeps those sold in the last 365 days, so
	// 6-month windows hold 12 months after six months; the whole 6-month
	// window is ~4 000 results, ~105 pages (2026-10-07), read a few slots a
	// day (NYCSoldSlots). A 12-month window is ~7 900 results (~200 pages).
	// Zillow also accepts "7", "14", "30", "90" and "12m".
	NYCSoldWindow = "6m"
	// NYCMinSoldPrice: a 2+ bedroom NYC apartment "sold" for less is a
	// transfer, a parking space or a typo (one said $16 273), not a comp.
	NYCMinSoldPrice = 100000
)

// SoldSearch is one Zillow recently-sold search of an NYC borough: 2+
// bedrooms, no houses, townhouses, multi-family, land or manufactured
// homes, sold within Window, in a price band. Zillow's price filter is
// inclusive on both ends.
type SoldSearch struct {
	Borough  NYCBorough
	MinPrice int // 0 = no floor
	MaxPrice int
	Window   string // Zillow "doz": "6m", "12m", "90", ...
}

// NewSoldSearch is the whole-borough search (up to MaxPrice).
func NewSoldSearch(b NYCBorough, window string) SoldSearch {
	if window == "" {
		window = NYCSoldWindow
	}
	return SoldSearch{Borough: b, MaxPrice: MaxPrice, Window: window}
}

// NYCSoldSlots is the sold crawl's rotation: one search per (borough, price
// band), in a fixed order that takes the boroughs in turn, so a borough comes
// up every 3-5 slots. The bands are cut from the 6-month counts of
// 2026-10-07 (Manhattan 1 231, Brooklyn 1 088, Queens 1 239, Bronx 330,
// Staten Island 179 results; pages of 41 noted per band) so that each is
// about 5-11 pages, which a day's request budget can walk whole; one that
// grows past SoldBandLimit is still split by the crawler.
func NYCSoldSlots(window string) []SoldSearch {
	mn, bk, qn, bx, si := NYCBoroughs[0], NYCBoroughs[1], NYCBoroughs[2], NYCBoroughs[3], NYCBoroughs[4]
	band := func(b NYCBorough, lo, hi int) SoldSearch {
		s := NewSoldSearch(b, window)
		s.MinPrice, s.MaxPrice = lo, hi
		return s
	}
	return []SoldSearch{
		band(mn, 0, 750000),         // 7
		band(bk, 0, 500000),         // ~7 (14 for $0-750k)
		band(qn, 0, 375000),         // 8
		band(bx, 0, MaxPrice),       // 9
		band(mn, 750001, 1125000),   // 11
		band(bk, 500001, 750000),    // ~7
		band(qn, 375001, 550000),    // ~8 (16 for $375k-750k)
		band(si, 0, MaxPrice),       // 5
		band(mn, 1125001, 1300000),  // ~6 (13 for $1.125M-1.5M)
		band(bk, 750001, 1100000),   // ~7 (14 for $750k-1.5M)
		band(qn, 550001, 750000),    // ~8
		band(mn, 1300001, MaxPrice), // ~7
		band(bk, 1100001, MaxPrice), // ~7
		band(qn, 750001, MaxPrice),  // 7
	}
}

// URL returns page n (1-based) of the search, most recent sales first.
func (s SoldSearch) URL(page int) string {
	v := func(x any) map[string]any { return map[string]any{"value": x} }
	price := map[string]any{"max": s.MaxPrice}
	if s.MinPrice > 0 {
		price["min"] = s.MinPrice
	}
	fs := map[string]any{
		"price": price, "beds": map[string]any{"min": 2}, "sort": v("days"),
		"rs": v(true), "doz": v(s.Window),
	}
	for _, k := range []string{"fsba", "fsbo", "nc", "cmsn", "auc", "fore", "sf", "tow", "mf", "land", "manu"} {
		fs[k] = v(false)
	}
	q := map[string]any{
		"pagination":      map[string]any{"currentPage": max(page, 1)},
		"isMapVisible":    false,
		"regionSelection": []any{map[string]any{"regionId": s.Borough.RegionID, "regionType": 17}},
		"filterState":     fs,
		"isListVisible":   true,
	}
	b, _ := json.Marshal(q)
	return Base + "/homes/for_sale/?searchQueryState=" + url.QueryEscape(string(b))
}

// Label names the search in logs: "brooklyn", "brooklyn $750,001-1,500,000".
func (s SoldSearch) Label() string {
	if s.MinPrice <= 0 && s.MaxPrice == MaxPrice {
		return s.Borough.Slug
	}
	return fmt.Sprintf("%s $%s-%s", s.Borough.Slug, money(s.MinPrice), money(s.MaxPrice))
}

func money(n int) string {
	s := fmt.Sprint(n)
	for i := len(s) - 3; i > 0; i -= 3 {
		s = s[:i] + "," + s[i:]
	}
	return s
}

// Split halves the price band at a multiple of $25 000: [Min, mid] and
// [mid+1, Max]. ok is false when the band is too narrow to split.
func (s SoldSearch) Split() (lo, hi SoldSearch, ok bool) {
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

// place is where a search looks: a Michigan county, or an NYC borough.
type place struct {
	market  string
	county  County     // Michigan
	borough NYCBorough // NYC
}

// ParseSearch reads a search page of county c.
func ParseSearch(html string, c County, sold bool) (SearchPage, error) {
	return parseSearch(html, place{market: listing.MarketMI, county: c}, sold)
}

// ParseNYCSold reads a recently-sold search page of borough b (SoldSearch).
func ParseNYCSold(html string, b NYCBorough) (SearchPage, error) {
	return parseSearch(html, place{market: listing.MarketNYC, borough: b}, true)
}

func parseSearch(html string, pl place, sold bool) (SearchPage, error) {
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
		l, ok := toListing(jsonx.Map(r), pl, sold)
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

func toListing(r map[string]any, pl place, sold bool) (listing.Listing, bool) {
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
		Market:   pl.market,
		Status:   listing.StatusActive,
		URL:      absURL(jsonx.String(r["detailUrl"]), zpid),
		Address:  strings.TrimSpace(jsonx.String(firstOf(r["addressStreet"], hi["streetAddress"], r["address"]))),
		Price:    price,
		HomeType: HomeType(jsonx.String(hi["homeType"])),
		City:     listing.Str(jsonx.String(firstOf(r["addressCity"], hi["city"]))),
		Zip:      listing.Str(jsonx.String(firstOf(r["addressZipcode"], hi["zipcode"]))),
		PhotoURL: listing.Str(jsonx.String(r["imgSrc"])),
	}
	if pl.market == listing.MarketNYC {
		// A sale with no address cannot be matched to a building, and one far
		// under any NYC apartment price is not an open-market sale.
		if undisclosed, _ := r["isUndisclosedAddress"].(bool); undisclosed ||
			strings.Contains(strings.ToLower(l.Address), "undisclosed") || l.Address == "" || price < NYCMinSoldPrice {
			return listing.Listing{}, false
		}
		var unit string
		l.Address, unit = SplitUnit(l.Address)
		if unit == "" {
			_, unit = SplitUnit("x " + jsonx.String(hi["unit"]))
		}
		if unit != "" {
			l.Unit = listing.Str("#" + unit)
		}
		b := streeteasy.BoroughFromZip(strOf(l.Zip))
		if b == "" {
			b = pl.borough.Slug
		}
		l.Borough = listing.Str(b)
	} else {
		l.County = listing.Str(pl.county.Slug)
		l.Area = listing.Str(pl.county.Area)
	}
	l.Lat = jsonx.NumPtr(hi["latitude"])
	l.Lon = jsonx.NumPtr(hi["longitude"])
	if l.Lat == nil {
		l.Lat = jsonx.NumPtr(jsonx.Get(r, "latLong", "latitude"))
		l.Lon = jsonx.NumPtr(jsonx.Get(r, "latLong", "longitude"))
	}
	if pl.market == listing.MarketMI && l.Lat != nil && l.Lon != nil {
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
	if pl.market == listing.MarketMI {
		l.LotSqft = lotSqft(hi["lotAreaValue"], jsonx.String(hi["lotAreaUnit"]))
	}
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

func strOf(p *string) string {
	if p == nil {
		return ""
	}
	return *p
}

// unitRe matches the unit at the end of a Zillow street address:
// "1408 Avenue O APT 3B", "1235 Forest Hill Rd #2E", "50 Fort Pl #B3-b/a",
// "10 W End Ave UNIT 4C".
var unitRe = regexp.MustCompile(`(?i)\s+(?:(?:APT|APARTMENT|UNIT|STE|SUITE|RM|FL)\.?\s*#?\s*|#\s*)([A-Z0-9][A-Z0-9/-]*)\s*$`)

// SplitUnit cuts the unit off a street address: ("1408 Avenue O", "3B") for
// "1408 Avenue O APT 3B". The unit is upper case; "" when there is none.
func SplitUnit(street string) (address, unit string) {
	street = strings.TrimSpace(street)
	m := unitRe.FindStringSubmatchIndex(street)
	if m == nil {
		return street, ""
	}
	return strings.TrimSpace(street[:m[0]]), strings.ToUpper(street[m[2]:m[3]])
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
