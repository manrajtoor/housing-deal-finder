// Package listing holds the wire types shared by the crawler and the Worker.
// They follow CONTRACT.md exactly: camelCase JSON, whole dollars, ISO-8601
// UTC times, optional fields omitted when unknown.
package listing

import "time"

// Sources, markets, statuses and home types (CONTRACT.md).
const (
	SourceStreetEasy = "streeteasy"
	SourceZillow     = "zillow"

	MarketNYC = "nyc"
	MarketMI  = "mi"

	StatusActive = "active"
	StatusSold   = "sold"

	HomeCondo        = "condo"
	HomeCoop         = "coop"
	HomeTownhouse    = "townhouse"
	HomeSingleFamily = "single_family"
	HomeMultiFamily  = "multi_family"
	HomeOther        = "other"
)

// Listing is one home as the Worker stores it.
type Listing struct {
	ID      string `json:"id"`
	Source  string `json:"source"`
	Market  string `json:"market"`
	Status  string `json:"status"`
	URL     string `json:"url"`
	Address string `json:"address"`

	Unit *string  `json:"unit,omitempty"`
	City *string  `json:"city,omitempty"`
	Zip  *string  `json:"zip,omitempty"`
	Lat  *float64 `json:"lat,omitempty"`
	Lon  *float64 `json:"lon,omitempty"`

	Price  int     `json:"price"`
	SoldAt *string `json:"soldAt,omitempty"`

	Beds      *int     `json:"beds,omitempty"`
	Baths     *float64 `json:"baths,omitempty"`
	Sqft      *int     `json:"sqft,omitempty"`
	LotSqft   *int     `json:"lotSqft,omitempty"`
	YearBuilt *int     `json:"yearBuilt,omitempty"`
	HomeType  string   `json:"homeType"`

	Neighborhood *string `json:"neighborhood,omitempty"`
	Borough      *string `json:"borough,omitempty"`
	County       *string `json:"county,omitempty"`
	Area         *string `json:"area,omitempty"`

	Zestimate    *int    `json:"zestimate,omitempty"`
	DaysOnMarket *int    `json:"daysOnMarket,omitempty"`
	PhotoURL     *string `json:"photoUrl,omitempty"`

	// Water facts. A Michigan search card sets WaterType/WaterBody from the
	// map (WaterSource "map", crawler/internal/geo); the Worker stores them
	// only while the row has no detail read, so a description always wins.
	WaterType   *string `json:"waterType,omitempty"`
	WaterBody   *string `json:"waterBody,omitempty"`
	WaterSource *string `json:"waterSource,omitempty"`

	// Detail fields: search pages never set these, so a search push cannot
	// wipe what an earlier detail read stored.
	Description  *string `json:"description,omitempty"`
	FrontageFt   *int    `json:"frontageFt,omitempty"`
	Maintenance  *int    `json:"maintenance,omitempty"`
	Taxes        *int    `json:"taxes,omitempty"`
	DetailReadAt *string `json:"detailReadAt,omitempty"`

	CompOnly bool `json:"compOnly"`
}

// Detail is one entry of POST /api/listings/detail: what a detail page added.
type Detail struct {
	ID           string  `json:"id"`
	DetailReadAt string  `json:"detailReadAt"`
	Description  *string `json:"description,omitempty"`
	WaterType    *string `json:"waterType,omitempty"`
	WaterBody    *string `json:"waterBody,omitempty"`
	WaterSource  *string `json:"waterSource,omitempty"` // "description" when WaterType is set
	FrontageFt   *int    `json:"frontageFt,omitempty"`
	Maintenance  *int    `json:"maintenance,omitempty"`
	Taxes        *int    `json:"taxes,omitempty"`
	YearBuilt    *int    `json:"yearBuilt,omitempty"`
}

// Water sources (CONTRACT.md waterSource).
const (
	WaterFromMap         = "map"
	WaterFromDescription = "description"
)

// MaxDescription is the contract's cap on description length (characters).
const MaxDescription = 3000

// Truncate cuts s to at most n runes.
func Truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return string(r[:n])
}

// Timestamp formats t as the contract's ISO-8601 UTC string.
func Timestamp(t time.Time) string { return t.UTC().Format(time.RFC3339) }

// Ptr returns a pointer to v.
func Ptr[T any](v T) *T { return &v }

// Str returns a pointer to s, or nil when s is empty.
func Str(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}

// PosInt returns a pointer to n, or nil when n is not positive (sites use 0
// for "unknown").
func PosInt(n int) *int {
	if n <= 0 {
		return nil
	}
	return &n
}
