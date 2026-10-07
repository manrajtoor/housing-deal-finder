package crawl

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/url"
	"os"
	"regexp"
	"strconv"
	"strings"
	"testing"
	"time"

	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/listing"
	"housedeals/crawler/internal/store/apistore"
	"housedeals/crawler/internal/streeteasy"
	"housedeals/crawler/internal/zillow"
)

func fixture(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

const (
	seDetailURL = "https://streeteasy.com/building/166_25-powells-cove-blvd-whitestone/12f"
	zlDetailURL = "https://www.zillow.com/homedetails/10-Island-View-Dr-Traverse-City-MI-49696/94778208_zpid/"
)

// site serves the fixtures at the URLs the crawler builds: every county
// answers with the Grand Traverse page (one page of results each).
func site(t *testing.T, sold bool) fetch.Pages {
	p := fetch.Pages{
		streeteasy.SearchURL(1): fixture(t, "../streeteasy/testdata/search.html"),
		seDetailURL:             fixture(t, "../streeteasy/testdata/detail.html"),
		zlDetailURL:             fixture(t, "../zillow/testdata/detail.html"),
	}
	body := fixture(t, "../zillow/testdata/search.html")
	if sold {
		body = fixture(t, "../zillow/testdata/sold.html")
	}
	for _, c := range zillow.Counties {
		p[zillow.SearchURL(c, 1, sold)] = body
	}
	if sold {
		// Every borough answers with the Staten Island page (302 results).
		nyc := fixture(t, "../zillow/testdata/nyc_sold.html")
		for _, b := range zillow.NYCBoroughs {
			p[zillow.NewSoldSearch(b, "").URL(1)] = nyc
		}
	}
	return p
}

type pushCall struct {
	n     int
	scope apistore.Scope
}

type fakeAPI struct {
	pushes  []pushCall
	asked   map[string]int
	details []listing.Detail
	needs   map[string]apistore.NeedsDetail
	// rows per market served by ScoreInput (ids as strings), and what was posted.
	rows   map[string][]string
	inputs []string // "market after limit"
	scores []apistore.Scores
}

func (f *fakeAPI) ScoreInput(_ context.Context, market, after string, limit int) (apistore.ScorePage, error) {
	f.inputs = append(f.inputs, fmt.Sprintf("%s %q %d", market, after, limit))
	p := apistore.ScorePage{Market: market, Columns: json.RawMessage(`["id"]`), Rows: []json.RawMessage{}}
	for _, id := range f.rows[market] {
		if id > after && len(p.Rows) < limit {
			p.Rows = append(p.Rows, json.RawMessage(`["`+id+`"]`))
			last := id
			p.Last = &last
		}
	}
	p.N = len(p.Rows)
	return p, nil
}

func (f *fakeAPI) PostScores(_ context.Context, s apistore.Scores) (apistore.ScoresStats, error) {
	f.scores = append(f.scores, s)
	return apistore.ScoresStats{Upserted: len(s.Upserts), Deleted: len(s.Deletes), NewAlerts: len(s.Alerts)}, nil
}

// fakeScorer records its input and answers with fixed writes.
type fakeScorer struct {
	inputs []map[string]any
	out    string
}

func (f *fakeScorer) Score(_ context.Context, input []byte) ([]byte, error) {
	var m map[string]any
	if err := json.Unmarshal(input, &m); err != nil {
		return nil, err
	}
	f.inputs = append(f.inputs, m)
	return []byte(f.out), nil
}

func (f *fakeAPI) Push(_ context.Context, ls []listing.Listing, s apistore.Scope) (apistore.Stats, error) {
	f.pushes = append(f.pushes, pushCall{len(ls), s})
	return apistore.Stats{Seen: len(ls), Added: len(ls)}, nil
}

func (f *fakeAPI) NeedsDetail(_ context.Context, market string, limit int) (apistore.NeedsDetail, error) {
	if f.asked == nil {
		f.asked = map[string]int{}
	}
	f.asked[market] = limit
	return f.needs[market], nil
}

func (f *fakeAPI) PushDetails(_ context.Context, ds []listing.Detail) (apistore.DetailStats, error) {
	f.details = append(f.details, ds...)
	return apistore.DetailStats{Updated: len(ds)}, nil
}

var now = func() time.Time { return time.Date(2026, 10, 5, 12, 0, 0, 0, time.UTC) }

func TestQuickPushesEverySearchThenReadsDetails(t *testing.T) {
	api := &fakeAPI{needs: map[string]apistore.NeedsDetail{
		"nyc": {IDs: []string{"se:1851487"}, URLs: []string{seDetailURL}},
		"mi":  {IDs: []string{"zl:94778208", "zl:gone", "zl:evil"}, URLs: []string{zlDetailURL, "https://www.zillow.com/homedetails/gone/", "https://evil.example/x"}},
	}}
	var logs bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeQuick, Markets: []string{"nyc", "mi"}, DetailLimit: -1,
		Fetcher: site(t, false), API: api, Log: log.New(&logs, "", 0), Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatalf("failures: %v\n%s", res.Failures, logs.String())
	}
	if len(api.pushes) != 7 {
		t.Fatalf("pushes = %d, want 1 StreetEasy + 6 counties", len(api.pushes))
	}
	if s := api.pushes[0].scope; s.Source != "streeteasy" || s.Market != "nyc" || s.Mode != "quick" || s.SeenAt != "2026-10-05T12:00:00Z" {
		t.Errorf("scope = %+v", s)
	}
	if api.pushes[0].n != 7 || api.pushes[1].n != 6 || api.pushes[1].scope.Source != "zillow" {
		t.Errorf("pushes = %+v", api.pushes)
	}
	if api.asked["nyc"] != 6 || api.asked["mi"] != 6 {
		t.Errorf("needs-detail limits = %v", api.asked)
	}
	if len(api.details) != 2 {
		t.Fatalf("details = %+v", api.details)
	}
	se, zl := api.details[0], api.details[1]
	if se.ID != "se:1851487" || deref(se.Maintenance) != 3108 || se.Taxes != nil || se.DetailReadAt != "2026-10-05T12:00:00Z" {
		t.Errorf("streeteasy detail = %+v", se)
	}
	if zl.ID != "zl:94778208" || deref(zl.WaterType) != "inland" || deref(zl.WaterBody) != "Island Lake" || deref(zl.WaterSource) != "description" ||
		deref(zl.FrontageFt) != 200 || deref(zl.YearBuilt) != 1988 || !strings.HasPrefix(deref(zl.Description), "Welcome") {
		t.Errorf("zillow detail = %+v", zl)
	}
	for _, want := range []string{
		"streeteasy nyc quick: pages 1/37, listings 7 (dropped 1), push seen=7",
		"zillow mi grand_traverse quick: pages 1/2, listings 6",
		"detail mi quick: asked 3, read 1, gone 1",
		"not on zillow.com",
	} {
		if !strings.Contains(logs.String(), want) {
			t.Errorf("log lacks %q:\n%s", want, logs.String())
		}
	}
}

func TestSourceStopsAtFirstErrorButOthersRun(t *testing.T) {
	pages := site(t, false)
	delete(pages, zillow.SearchURL(zillow.Counties[2], 1, false)) // antrim answers 404
	api := &fakeAPI{}
	var logs bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeQuick, Markets: []string{"nyc", "mi"}, DetailLimit: 0,
		Fetcher: pages, API: api, Log: log.New(&logs, "", 0), Now: now,
	})
	if len(res.Failures) != 1 || !strings.Contains(res.Failures[0], "zillow mi antrim: stopping zillow at page 1: HTTP 404") {
		t.Fatalf("failures = %v", res.Failures)
	}
	// StreetEasy, Grand Traverse and Leelanau were pushed; Benzie onwards never fetched.
	if len(api.pushes) != 3 {
		t.Errorf("pushes = %+v", api.pushes)
	}
	if strings.Contains(logs.String(), "benzie") {
		t.Errorf("zillow must stop after antrim:\n%s", logs.String())
	}
}

func TestBlockedPageStopsSource(t *testing.T) {
	pages := site(t, false)
	pages[streeteasy.SearchURL(1)] = `<html><div id="px-captcha"></div></html>`
	res := Run(context.Background(), Config{Mode: ModeQuick, Markets: []string{"nyc"}, Fetcher: pages, DryRun: true, Now: now, DetailLimit: -1})
	if len(res.Failures) != 1 || !strings.Contains(res.Failures[0], "bot check") {
		t.Fatalf("failures = %v", res.Failures)
	}
}

func TestDryRunPrintsJSONAndPushesNothing(t *testing.T) {
	var out bytes.Buffer
	var logs bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeQuick, Markets: []string{"mi"}, DetailLimit: -1, DryRun: true,
		Fetcher: site(t, false), Out: &out, Log: log.New(&logs, "", 0), Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatal(res.Failures)
	}
	var got struct {
		Listings []map[string]any `json:"listings"`
		Details  []map[string]any `json:"details"`
	}
	if err := json.Unmarshal(out.Bytes(), &got); err != nil {
		t.Fatalf("stdout is not JSON: %v\n%s", err, out.String())
	}
	if len(got.Listings) != 36 || len(got.Details) != 0 {
		t.Errorf("listings %d details %d", len(got.Listings), len(got.Details))
	}
	if !strings.Contains(logs.String(), "push skipped (dry run)") || !strings.Contains(logs.String(), "detail: skipped") {
		t.Errorf("logs:\n%s", logs.String())
	}
}

func TestSoldModeCrawlsZillowSoldForBothMarkets(t *testing.T) {
	api := &fakeAPI{}
	var out bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeSold, Markets: []string{"nyc", "mi"}, MaxPages: 1, DetailLimit: -1,
		Fetcher: site(t, true), API: api, Out: &out, Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatal(res.Failures)
	}
	// NYC: one push (the other boroughs answer the same, already seen, rows), then the six counties.
	if len(api.pushes) != 7 {
		t.Fatalf("pushes = %+v", api.pushes)
	}
	for i, p := range api.pushes {
		want := "mi"
		if i == 0 {
			want = "nyc"
		}
		if p.scope.Mode != "sold" || p.scope.Source != "zillow" || p.scope.Market != want {
			t.Errorf("push %d = %+v", i, p)
		}
	}
	if api.pushes[0].n != 8 {
		t.Errorf("nyc sold rows pushed = %d", api.pushes[0].n)
	}
	if api.asked != nil {
		t.Error("sold mode reads no detail pages")
	}
	markets := map[string]int{}
	for _, l := range res.Listings {
		markets[l.Market]++
		if l.Status != "sold" || !l.CompOnly || l.SoldAt == nil || l.Source != "zillow" {
			t.Fatalf("listing = %+v", l)
		}
		if l.Market == "nyc" && (l.Borough == nil || l.County != nil || l.Neighborhood != nil) {
			t.Fatalf("nyc listing = %+v", l)
		}
	}
	if markets["nyc"] != 8 || markets["mi"] != 24 {
		t.Errorf("markets = %v", markets)
	}
	if out.Len() != 0 {
		t.Error("only --dry-run writes to stdout")
	}
}

// zlSoldSite answers any Zillow NYC sold search with the NYC sold fixture,
// rewritten so each request has its own zpids, the given result count and
// 2 pages. It records "borough:min-max:page" per request.
type zlSoldSite struct {
	body  string
	count func(region, min, max int) int
	asked []string
}

func (z *zlSoldSite) Fetch(_ context.Context, u string) (string, error) {
	pu, err := url.Parse(u)
	if err != nil {
		return "", err
	}
	var q struct {
		Pagination struct {
			CurrentPage int `json:"currentPage"`
		} `json:"pagination"`
		RegionSelection []struct {
			RegionID int `json:"regionId"`
		} `json:"regionSelection"`
		FilterState struct {
			Price struct {
				Min int `json:"min"`
				Max int `json:"max"`
			} `json:"price"`
		} `json:"filterState"`
	}
	if err := json.Unmarshal([]byte(pu.Query().Get("searchQueryState")), &q); err != nil || len(q.RegionSelection) != 1 {
		return "", &fetch.HTTPError{Status: 404, URL: u}
	}
	region, lo, hi := q.RegionSelection[0].RegionID, q.FilterState.Price.Min, q.FilterState.Price.Max
	z.asked = append(z.asked, fmt.Sprintf("%d:%d-%d:%d", region, lo, hi, q.Pagination.CurrentPage))
	body := strings.ReplaceAll(z.body, `"zpid":"`, fmt.Sprintf(`"zpid":"%d0`, len(z.asked)))
	body = strings.Replace(body, `"totalResultCount":302`, fmt.Sprintf(`"totalResultCount":%d`, z.count(region, lo, hi)), 1)
	body = strings.Replace(body, `"totalPages":8`, `"totalPages":2`, 1)
	return body, nil
}

func TestSoldModeSplitsZillowNYCIntoPriceBands(t *testing.T) {
	site := &zlSoldSite{
		body: fixture(t, "../zillow/testdata/nyc_sold.html"),
		count: func(region, lo, hi int) int {
			switch {
			case region != 270915: // Queens only is too big
				return 300
			case lo == 0 && hi == 1500000:
				return 2600
			case lo == 0 && hi == 750000:
				return 1300
			}
			return 650
		},
	}
	var logs bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeSold, Markets: []string{"nyc"}, DetailLimit: -1, DryRun: true,
		Fetcher: site, Log: log.New(&logs, "", 0), Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatalf("failures %v\n%s", res.Failures, logs.String())
	}
	want := []string{
		"12530:0-1500000:1", "12530:0-1500000:2",
		"37607:0-1500000:1", "37607:0-1500000:2",
		"270915:0-1500000:1", // probe, split
		"270915:0-750000:1",  // probe, split again
		"270915:0-375000:1", "270915:0-375000:2",
		"270915:375001-750000:1", "270915:375001-750000:2",
		"270915:750001-1500000:1", "270915:750001-1500000:2",
		"17182:0-1500000:1", "17182:0-1500000:2",
		"27252:0-1500000:1", "27252:0-1500000:2",
	}
	if fmt.Sprint(site.asked) != fmt.Sprint(want) {
		t.Errorf("asked\n %v\nwant\n %v", site.asked, want)
	}
	if len(res.Listings) != len(want)*8 {
		t.Errorf("listings = %d, want %d pages x 8", len(res.Listings), len(want))
	}
	for _, s := range []string{
		"zillow nyc queens: 2600 results > 780, splitting at $750000",
		"zillow nyc queens $0-750,000: 1300 results > 780, splitting at $375000",
		"zillow nyc queens $375,001-750,000 sold: pages 2/2, listings 16",
		"zillow nyc sold queens: 64 sold rows (window 6m), 138 of 150 requests left",
	} {
		if !strings.Contains(logs.String(), s) {
			t.Errorf("log lacks %q\n%s", s, logs.String())
		}
	}
}

func TestNYCSoldStopsAtTheRequestBudgetWithAWarning(t *testing.T) {
	defer func(n int) { NYCSoldBudget = n }(NYCSoldBudget)
	NYCSoldBudget = 5
	site := &zlSoldSite{body: fixture(t, "../zillow/testdata/nyc_sold.html"), count: func(int, int, int) int { return 300 }}
	var logs bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeSold, Markets: []string{"nyc"}, DetailLimit: -1, DryRun: true,
		Fetcher: site, Log: log.New(&logs, "", 0), Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatalf("a spent budget is not a failure: %v", res.Failures)
	}
	if len(site.asked) != 5 || len(res.Listings) != 5*8 {
		t.Errorf("asked %d, listings %d", len(site.asked), len(res.Listings))
	}
	if !strings.Contains(logs.String(), "WARNING zillow nyc queens: stopping zillow at page 2: request budget spent") {
		t.Errorf("logs:\n%s", logs.String())
	}
}

func TestFullModeWalksPages(t *testing.T) {
	pages := fetch.Pages{}
	gt := zillow.Counties[0]
	body := fixture(t, "../zillow/testdata/search.html") // says totalPages 2
	pages[zillow.SearchURL(gt, 1, false)] = body
	// Page 2: the same document with other zpids.
	pages[zillow.SearchURL(gt, 2, false)] = strings.ReplaceAll(body, `"zpid":"`, `"zpid":"9`)
	res := Run(context.Background(), Config{
		Mode: ModeFull, Markets: []string{"mi"}, DetailLimit: 0, DryRun: true, Fetcher: stopAfter{pages, gt}, Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatal(res.Failures)
	}
	if len(res.Listings) != 12 {
		t.Errorf("listings = %d, want 2 pages of 6", len(res.Listings))
	}
}

// stopAfter serves only county gt and answers the others with an empty page.
type stopAfter struct {
	pages fetch.Pages
	gt    zillow.County
}

func (s stopAfter) Fetch(ctx context.Context, u string) (string, error) {
	if b, ok := s.pages[u]; ok {
		return b, nil
	}
	return `<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"searchPageState":{"cat1":{"searchList":{"totalPages":0},"searchResults":{"listResults":[]}}}}}}</script>`, nil
}

func deref[T any](p *T) T {
	var zero T
	if p == nil {
		return zero
	}
	return *p
}

// bandSite answers any StreetEasy search URL with the search fixture,
// rewritten so each (area, band, page) has its own ids, the given result
// count and 2 pages. It records the URLs asked for.
type bandSite struct {
	body  string
	count func(area string, min, max int) int
	asked []string
}

var bandURL = regexp.MustCompile(`/for-sale/([a-z-]+)/price:(\d*)-(\d+)%7Cbeds:2-\?(?:page=(\d+)&)?sort_by=listed_desc$`)

func (b *bandSite) Fetch(_ context.Context, u string) (string, error) {
	b.asked = append(b.asked, u)
	m := bandURL.FindStringSubmatch(u)
	if m == nil {
		return "", &fetch.HTTPError{Status: 404, URL: u}
	}
	lo, _ := strconv.Atoi(m[2])
	hi, _ := strconv.Atoi(m[3])
	pg := m[4]
	if pg == "" {
		pg = "1"
	}
	prefix := fmt.Sprintf("%d%s", len(b.asked), pg) // unique per request
	body := strings.ReplaceAll(b.body, `\"id\":\"18`, `\"id\":\"`+prefix+`18`)
	body = strings.Replace(body, `\"totalCount\":5496`, fmt.Sprintf(`\"totalCount\":%d`, b.count(m[1], lo, hi)), 1)
	body = strings.Replace(body, `\"totalPages\":37`, `\"totalPages\":2`, 1)
	return body, nil
}

func TestFullModeSplitsStreetEasyIntoBoroughsAndBands(t *testing.T) {
	site := &bandSite{
		body: fixture(t, "../streeteasy/testdata/search.html"),
		count: func(area string, lo, hi int) int {
			if area == "brooklyn" && lo == 0 && hi == 1500000 {
				return 1800 // too many: split once
			}
			return 500
		},
	}
	var logs bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeFull, Markets: []string{"nyc"}, DetailLimit: 0, DryRun: true,
		Fetcher: site, Log: log.New(&logs, "", 0), Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatalf("failures %v\n%s", res.Failures, logs.String())
	}
	var areas []string
	for _, u := range site.asked {
		m := bandURL.FindStringSubmatch(u)
		areas = append(areas, m[1]+":"+m[2]+"-"+m[3]+":"+m[4])
	}
	want := []string{
		"manhattan:-1500000:", "manhattan:-1500000:2",
		"brooklyn:-1500000:", // probe, then split
		"brooklyn:-750000:", "brooklyn:-750000:2",
		"brooklyn:750001-1500000:", "brooklyn:750001-1500000:2",
		"queens:-1500000:", "queens:-1500000:2",
		"bronx:-1500000:", "bronx:-1500000:2",
		"staten-island:-1500000:", "staten-island:-1500000:2",
	}
	if fmt.Sprint(areas) != fmt.Sprint(want) {
		t.Errorf("asked\n %v\nwant\n %v", areas, want)
	}
	if len(res.Listings) != 13*7 {
		t.Errorf("listings = %d, want 13 pages x 7", len(res.Listings))
	}
	ids := map[string]bool{}
	for _, l := range res.Listings {
		if ids[l.ID] {
			t.Fatalf("duplicate %s", l.ID)
		}
		ids[l.ID] = true
	}
	for _, s := range []string{"streeteasy nyc brooklyn: 1800 results > 1000, splitting at $750000",
		"streeteasy nyc brooklyn $750,001-1,500,000 full: pages 2/2, listings 14"} {
		if !strings.Contains(logs.String(), s) {
			t.Errorf("log lacks %q\n%s", s, logs.String())
		}
	}
}

func TestStreetEasyDedupesAcrossBands(t *testing.T) {
	// Every band answers the very same page: only the first search keeps it.
	site := fetchAll{fixture(t, "../streeteasy/testdata/search.html")}
	res := Run(context.Background(), Config{
		Mode: ModeFull, Markets: []string{"nyc"}, MaxPages: 1, DetailLimit: 0, DryRun: true, Fetcher: site, Now: now,
	})
	if len(res.Failures) != 0 || len(res.Listings) != 7 {
		t.Errorf("listings %d failures %v", len(res.Listings), res.Failures)
	}
}

type fetchAll struct{ body string }

func (f fetchAll) Fetch(context.Context, string) (string, error) {
	return strings.Replace(f.body, `\"totalCount\":5496`, `\"totalCount\":900`, 1), nil
}

func TestBandSplitStopsWhenThePriceFilterIsIgnored(t *testing.T) {
	// The site reports 5 496 results whatever the band: split once, see the
	// count did not drop, and walk instead of splitting down to $25k bands.
	site := &bandSite{body: fixture(t, "../streeteasy/testdata/search.html"), count: func(string, int, int) int { return 5496 }}
	res := Run(context.Background(), Config{
		Mode: ModeFull, Markets: []string{"nyc"}, MaxPages: 1, DetailLimit: 0, DryRun: true, Fetcher: site, Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatal(res.Failures)
	}
	if len(site.asked) != 5*3 {
		t.Errorf("asked %d pages, want 3 per borough", len(site.asked))
	}
}

func TestBlockedDetailPageIsAWarningNotAFailure(t *testing.T) {
	pages := site(t, false)
	pages[zlDetailURL] = `<html><div id="px-captcha"></div></html>`
	api := &fakeAPI{needs: map[string]apistore.NeedsDetail{
		"mi": {IDs: []string{"zl:94778208", "zl:2"}, URLs: []string{zlDetailURL, zlDetailURL}},
	}}
	var logs bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeQuick, Markets: []string{"mi"}, DetailLimit: -1, Fetcher: countFetches{pages, new(int)},
		API: api, Log: log.New(&logs, "", 0), Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatalf("failures = %v", res.Failures)
	}
	if !strings.Contains(logs.String(), "WARNING detail mi: blocked, no more detail reads this run") {
		t.Errorf("logs:\n%s", logs.String())
	}
	pages[zlDetailURL] = "<html>a page that changed shape</html>"
	res = Run(context.Background(), Config{Mode: ModeQuick, Markets: []string{"mi"}, DetailLimit: -1, Fetcher: pages, API: api, Now: now})
	if len(res.Failures) != 1 || !strings.Contains(res.Failures[0], "detail mi: stopping") {
		t.Errorf("a parse error still fails the run: %v", res.Failures)
	}
}

type countFetches struct {
	pages fetch.Pages
	n     *int
}

func (c countFetches) Fetch(ctx context.Context, u string) (string, error) {
	*c.n++
	return c.pages.Fetch(ctx, u)
}

func TestDetailPagesAreSpacedByDetailDelay(t *testing.T) {
	api := &fakeAPI{needs: map[string]apistore.NeedsDetail{
		"mi": {IDs: []string{"zl:1", "zl:2", "zl:3"}, URLs: []string{zlDetailURL, zlDetailURL, zlDetailURL}},
	}}
	start := time.Now()
	res := Run(context.Background(), Config{
		Mode: ModeQuick, Markets: []string{"mi"}, MaxPages: 1, DetailLimit: -1, Fetcher: site(t, false),
		API: api, Now: now, DetailDelay: 40 * time.Millisecond,
	})
	if len(res.Failures) != 0 || len(api.details) != 3 {
		t.Fatalf("failures %v details %d", res.Failures, len(api.details))
	}
	if d := time.Since(start); d < 80*time.Millisecond {
		t.Errorf("3 detail pages took %v, want >= 2 x 40ms", d)
	}
}

func TestScoringPagesRunsTheScorerAndPostsChunksAlertsFirst(t *testing.T) {
	defer func(n int) { scorePageSize = n }(scorePageSize)
	scorePageSize = 2
	api := &fakeAPI{rows: map[string][]string{"mi": {"zl:1", "zl:2", "zl:3", "zl:4", "zl:5"}, "nyc": {"se:1"}}}
	var ups, dels, alerts []string
	for i := 0; i < 60; i++ {
		ups = append(ups, fmt.Sprintf(`{"id":"zl:u%d"}`, i))
	}
	for i := 0; i < 5; i++ {
		dels = append(dels, fmt.Sprintf(`"zl:d%d"`, i))
	}
	for i := 0; i < 3; i++ {
		alerts = append(alerts, fmt.Sprintf(`{"id":"zl:a%d"}`, i))
	}
	sc := &fakeScorer{out: `{"market":"mi","upserts":[` + strings.Join(ups, ",") + `],"deletes":[` + strings.Join(dels, ",") +
		`],"alerts":[` + strings.Join(alerts, ",") + `],"stats":{}}`}
	var logs bytes.Buffer
	res := Run(context.Background(), Config{
		Mode: ModeSold, Markets: []string{"mi"}, MaxPages: 1, DetailLimit: -1,
		Fetcher: site(t, true), API: api, Scorer: sc, Log: log.New(&logs, "", 0), Now: now,
	})
	if len(res.Failures) != 0 {
		t.Fatalf("failures %v\n%s", res.Failures, logs.String())
	}
	if fmt.Sprint(api.inputs) != `[mi "" 2 mi "zl:2" 2 mi "zl:4" 2]` {
		t.Errorf("score-input calls %v", api.inputs)
	}
	if len(sc.inputs) != 1 || sc.inputs[0]["market"] != "mi" || sc.inputs[0]["now"] != "2026-10-05T12:00:00Z" ||
		len(sc.inputs[0]["rows"].([]any)) != 5 || fmt.Sprint(sc.inputs[0]["columns"]) != "[id]" {
		t.Errorf("scorer input %v", sc.inputs)
	}
	if len(api.scores) != 2 {
		t.Fatalf("posts %d", len(api.scores))
	}
	c0, c1 := api.scores[0], api.scores[1]
	if len(c0.Alerts) != 3 || len(c0.Upserts) != 44 || len(c0.Deletes) != 0 || c0.Market != "mi" {
		t.Errorf("chunk 1: %d alerts %d upserts %d deletes", len(c0.Alerts), len(c0.Upserts), len(c0.Deletes))
	}
	if len(c1.Alerts) != 0 || len(c1.Upserts) != 16 || len(c1.Deletes) != 5 {
		t.Errorf("chunk 2: %d alerts %d upserts %d deletes", len(c1.Alerts), len(c1.Upserts), len(c1.Deletes))
	}
	if !strings.Contains(logs.String(), "score mi: rows 5, upserts 60, deletes 5, alerts 3 (new 3, refreshed 0)") {
		t.Errorf("logs:\n%s", logs.String())
	}
}

func TestNoScoringOnADryRunOrWithoutAScorer(t *testing.T) {
	api := &fakeAPI{}
	sc := &fakeScorer{out: `{}`}
	Run(context.Background(), Config{Mode: ModeQuick, Markets: []string{"mi"}, DetailLimit: 0, DryRun: true,
		Fetcher: site(t, false), API: api, Scorer: sc, Out: io.Discard, Now: now})
	var logs bytes.Buffer
	Run(context.Background(), Config{Mode: ModeQuick, Markets: []string{"mi"}, DetailLimit: 0,
		Fetcher: site(t, false), API: api, Log: log.New(&logs, "", 0), Now: now})
	if len(sc.inputs) != 0 || len(api.inputs) != 0 || !strings.Contains(logs.String(), "score: skipped") {
		t.Errorf("scored: %v %v\n%s", sc.inputs, api.inputs, logs.String())
	}
}

func TestBlocked(t *testing.T) {
	for _, c := range []struct {
		err  error
		want bool
	}{
		{&fetch.HTTPError{Status: 403}, true},
		{&fetch.HTTPError{Status: 429}, true},
		{&fetch.HTTPError{Status: 500}, false},
		{&fetch.ParseError{Msg: "zillow detail: no __NEXT_DATA__ in the page (bot check page)"}, true},
		{&fetch.ParseError{Msg: "zillow detail: no property in gdpClientCache"}, false},
	} {
		if got := blocked(c.err); got != c.want {
			t.Errorf("blocked(%v) = %v", c.err, got)
		}
	}
}
