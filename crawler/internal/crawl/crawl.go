// Package crawl runs one crawl: the searches of a mode, a push per search,
// then the detail pages the Worker asks for, then the scoring of each
// market crawled (the Worker only stores; see Scorer).
//
// Every request goes through the one Fetcher it is given (the CLI wraps a
// single 1.5 s Throttle around it); detail pages are also spaced by
// DetailDelay, and in sold mode Zillow search pages by ZillowDelay, within a
// budget of SoldBudget Zillow requests. A source stops at its first error
// that survived the fetcher's retries (a 404, a page that changed shape);
// what it gathered before is still pushed, and Run reports the failure so
// the CLI exits non-zero. Being blocked is not a failure: a Zillow page that
// answers 403 / 429 or a bot check ends every Zillow request of the run
// (search and detail) with a warning, and a blocked StreetEasy detail page
// ends NYC's detail reads with a warning.
package crawl

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"strings"
	"time"

	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/listing"
	"housedeals/crawler/internal/store/apistore"
	"housedeals/crawler/internal/streeteasy"
	"housedeals/crawler/internal/zillow"
)

// Modes.
const (
	ModeQuick = "quick"
	ModeFull  = "full"
	ModeSold  = "sold"
)

// API is the part of the Worker client a crawl uses.
type API interface {
	Push(ctx context.Context, listings []listing.Listing, scope apistore.Scope) (apistore.Stats, error)
	NeedsDetail(ctx context.Context, market string, limit int) (apistore.NeedsDetail, error)
	PushDetails(ctx context.Context, details []listing.Detail) (apistore.DetailStats, error)
	ScoreInput(ctx context.Context, market, after string, limit int) (apistore.ScorePage, error)
	PostScores(ctx context.Context, s apistore.Scores) (apistore.ScoresStats, error)
}

// Scorer turns one market's scoring input into the writes for POST
// /api/scores (the housedeals-score binary; see scorer/src/batch.rs).
type Scorer interface {
	Score(ctx context.Context, input []byte) ([]byte, error)
}

// Config is one run.
type Config struct {
	Mode    string
	Markets []string // nyc, mi
	// MaxPages caps pages per search; 0 means the mode's default.
	MaxPages int
	// DetailLimit is how many detail pages to ask for per market; negative
	// means the mode's default.
	DetailLimit int
	// DryRun prints JSON to Out and pushes nothing. With an API it still asks
	// needs-detail (read only) and reads those pages.
	DryRun  bool
	Fetcher fetch.Fetcher
	API     API // nil: no Worker
	// Scorer scores each market crawled after the searches and detail
	// reads; nil skips scoring.
	Scorer Scorer
	// SoldWindow is the NYC sold search's window (Zillow "doz"); "" means
	// zillow.NYCSoldWindow. Michigan sold searches always cover 12 months.
	SoldWindow string
	// DetailDelay spaces detail pages, on top of the Fetcher's own throttle.
	DetailDelay time.Duration
	// ZillowDelay spaces Zillow search pages in sold mode, on top of the
	// Fetcher's own throttle (quick and full read ~7-10 Zillow pages and
	// keep the Fetcher's spacing).
	ZillowDelay time.Duration
	// SoldBudget is the most Zillow requests a sold run makes, Michigan
	// first, then NYC (NYCSoldSlots); 0 means DefaultSoldBudget.
	SoldBudget int
	Out        io.Writer
	Log        *log.Logger
	Now        func() time.Time
}

// DefaultMaxPages is the per-search page cap of each mode.
func DefaultMaxPages(mode string) int {
	switch mode {
	case ModeQuick:
		return 1
	case ModeSold:
		return 20
	}
	return 60 // full: StreetEasy reports ~37 pages, a Michigan county 1-3
}

// DefaultDetailLimit is the per-market detail budget of each mode.
func DefaultDetailLimit(mode string) int {
	switch mode {
	case ModeQuick:
		return 6
	case ModeFull:
		return 20
	}
	return 0
}

// Result is what a run gathered.
type Result struct {
	Listings []listing.Listing `json:"listings"`
	Details  []listing.Detail  `json:"details"`
	Failures []string          `json:"-"`
}

type runner struct {
	cfg    Config
	seenAt string
	res    Result
	// detail fetches pages through an extra DetailDelay throttle.
	detail fetch.Fetcher
	// zl fetches every Zillow search page.
	zl *zillowGate
}

// DefaultSoldBudget is the sold run's Zillow request budget. From a GitHub
// runner Zillow answered 429 after ~22 search pages in under a minute
// (2026-10-07); Michigan sold takes ~14, leaving ~11 for NYC.
const DefaultSoldBudget = 25

// Run crawls. It returns what it gathered; Failures lists the errors that
// stopped a source, a push or a detail read.
func Run(ctx context.Context, cfg Config) Result {
	if cfg.Now == nil {
		cfg.Now = time.Now
	}
	if cfg.Log == nil {
		cfg.Log = log.New(io.Discard, "", 0)
	}
	if cfg.MaxPages <= 0 {
		cfg.MaxPages = DefaultMaxPages(cfg.Mode)
	}
	if cfg.DetailLimit < 0 {
		cfg.DetailLimit = DefaultDetailLimit(cfg.Mode)
	}
	r := &runner{cfg: cfg, seenAt: listing.Timestamp(cfg.Now()), detail: cfg.Fetcher}
	if cfg.DetailDelay > 0 {
		r.detail = fetch.NewThrottle(cfg.DetailDelay).Wrap(cfg.Fetcher)
	}
	r.zl = &zillowGate{f: cfg.Fetcher, budget: -1}
	if cfg.Mode == ModeSold {
		if cfg.ZillowDelay > 0 {
			r.zl.f = fetch.NewThrottle(cfg.ZillowDelay).Wrap(cfg.Fetcher)
		}
		r.zl.budget = cfg.SoldBudget
		if r.zl.budget <= 0 {
			r.zl.budget = DefaultSoldBudget
		}
	}
	r.res.Listings = []listing.Listing{}
	r.res.Details = []listing.Detail{}

	var crawled []string
	if cfg.Mode == ModeSold {
		// Michigan first: its ~14 pages are the whole market, NYC's
		// rotation takes what is left of the budget.
		if r.has(listing.MarketMI) {
			r.zillow(ctx)
			crawled = append(crawled, listing.MarketMI)
		}
		if r.has(listing.MarketNYC) {
			r.zillowNYCSold(ctx)
			crawled = append(crawled, listing.MarketNYC)
		}
	} else {
		if r.has(listing.MarketNYC) {
			r.streetEasy(ctx)
			crawled = append(crawled, listing.MarketNYC)
		}
		if r.has(listing.MarketMI) {
			r.zillow(ctx)
			crawled = append(crawled, listing.MarketMI)
		}
	}
	if cfg.Mode != ModeSold && cfg.DetailLimit > 0 {
		if cfg.API == nil {
			cfg.Log.Printf("detail: skipped (no Worker to ask which listings need it)")
		} else {
			for _, m := range []string{listing.MarketNYC, listing.MarketMI} {
				if r.has(m) {
					r.details(ctx, m)
				}
			}
		}
	}
	switch {
	case cfg.DryRun || cfg.API == nil:
	case cfg.Scorer == nil:
		cfg.Log.Printf("score: skipped (no scorer)")
	default:
		for _, m := range crawled {
			r.score(ctx, m)
		}
	}
	if cfg.DryRun && cfg.Out != nil {
		enc := json.NewEncoder(cfg.Out)
		enc.SetIndent("", "  ")
		if err := enc.Encode(r.res); err != nil {
			r.fail("write JSON: %v", err)
		}
	}
	return r.res
}

func (r *runner) has(market string) bool {
	for _, m := range r.cfg.Markets {
		if m == market {
			return true
		}
	}
	return false
}

func (r *runner) fail(format string, args ...any) {
	msg := fmt.Sprintf(format, args...)
	r.cfg.Log.Print("ERROR " + msg)
	r.res.Failures = append(r.res.Failures, msg)
}

// page is one parsed search page: its listings, the page count and result
// count the site reports, and how many results were dropped.
type page struct {
	ls                     []listing.Listing
	totalPages, totalCount int
	dropped                int
}

// pageFunc fetches and parses page n of a search.
type pageFunc func(ctx context.Context, n int) (page, error)

// search walks the pages of one search, pushes what it found and logs one
// line. first, when not nil, is page 1 already fetched. seen dedupes
// listings across the searches of a source within the run. It reports false
// when the source must stop.
func (r *runner) search(ctx context.Context, name, source, market string, seen map[string]bool, first *page, fetchPage pageFunc) bool {
	var all []listing.Listing
	pages, total, dropped := 0, 0, 0
	var stopErr error
	for n := 1; n <= r.cfg.MaxPages && (total == 0 || n <= total); n++ {
		var p page
		if n == 1 && first != nil {
			p = *first
		} else {
			var err error
			if p, err = fetchPage(ctx, n); err != nil {
				stopErr = err
				break
			}
		}
		pages++
		total, dropped = p.totalPages, dropped+p.dropped
		fresh := 0
		for _, l := range p.ls {
			if !seen[l.ID] {
				seen[l.ID] = true
				all = append(all, l)
				fresh++
			}
		}
		if len(p.ls) == 0 {
			break // an empty page
		}
		if total == 0 {
			total = n // the site did not say; do not guess further
		}
	}
	r.res.Listings = append(r.res.Listings, all...)
	push := r.push(ctx, all, source, market)
	r.cfg.Log.Printf("%s %s: pages %d/%d, listings %d (dropped %d), %s", name, r.cfg.Mode, pages, total, len(all), dropped, push)
	if stopErr != nil {
		r.stopped(name, source, pages+1, stopErr)
		return false
	}
	return true
}

func (r *runner) push(ctx context.Context, ls []listing.Listing, source, market string) string {
	switch {
	case r.cfg.DryRun:
		return "push skipped (dry run)"
	case r.cfg.API == nil:
		return "push skipped (no --push)"
	case len(ls) == 0:
		return "nothing to push"
	}
	s, err := r.cfg.API.Push(ctx, ls, apistore.Scope{Source: source, Market: market, Mode: r.cfg.Mode, SeenAt: r.seenAt})
	if err != nil {
		r.fail("push %s %s: %v (pushed before failing: %s)", source, market, err, s)
		return "push FAILED"
	}
	return "push " + s.String()
}

func (r *runner) sePage(s streeteasy.Search) pageFunc {
	return func(ctx context.Context, n int) (page, error) {
		body, err := r.cfg.Fetcher.Fetch(ctx, s.URL(n))
		if err != nil {
			return page{}, err
		}
		p, err := streeteasy.ParseSearch(body)
		if err != nil {
			return page{}, err
		}
		total := min(p.TotalPages, streeteasy.PageCap)
		if !p.HasNextPage && total > n {
			total = n
		}
		return page{ls: p.Listings, totalPages: total, totalCount: p.TotalCount, dropped: p.Dropped}, nil
	}
}

// streetEasy reads StreetEasy. Quick mode reads the newest page of the
// city-wide search. Full mode cannot walk the city-wide search (StreetEasy
// serves at most 37 pages, ~1 100 of ~5 500 listings), so it searches each
// borough and splits any search reporting more than BandLimit results into
// price bands, halving until each fits.
func (r *runner) streetEasy(ctx context.Context) {
	seen := map[string]bool{}
	if r.cfg.Mode != ModeFull {
		r.search(ctx, "streeteasy nyc", listing.SourceStreetEasy, listing.MarketNYC, seen, nil, r.sePage(streeteasy.CityWide))
		return
	}
	for _, area := range streeteasy.BoroughAreas {
		if !r.walkBand(ctx, r.seBand(streeteasy.Search{Area: area, MaxPrice: streeteasy.MaxSearchPrice}), seen, 0) {
			return
		}
	}
}

// band is one search the band walker may split in two by price.
type band struct {
	name           string
	source, market string
	limit          int // most results a search may report and still be walked whole
	pageCap        int // most pages the site serves for one search
	maxPrice       int // the band's ceiling (logged where it splits)
	fetch          pageFunc
	split          func() (lo, hi band, ok bool)
}

func (r *runner) seBand(s streeteasy.Search) band {
	return band{
		name: "streeteasy nyc " + s.Label(), source: listing.SourceStreetEasy, market: listing.MarketNYC,
		limit: streeteasy.BandLimit, pageCap: streeteasy.PageCap, maxPrice: s.MaxPrice, fetch: r.sePage(s),
		split: func() (band, band, bool) {
			lo, hi, ok := s.Split()
			return r.seBand(lo), r.seBand(hi), ok
		},
	}
}

func (r *runner) zlSoldBand(s zillow.SoldSearch, f fetch.Fetcher) band {
	return band{
		name: "zillow nyc " + s.Label(), source: listing.SourceZillow, market: listing.MarketNYC,
		limit: zillow.SoldBandLimit, pageCap: zillow.SoldPageCap, maxPrice: s.MaxPrice,
		fetch: func(ctx context.Context, n int) (page, error) {
			body, err := f.Fetch(ctx, s.URL(n))
			if err != nil {
				return page{}, err
			}
			p, err := zillow.ParseNYCSold(body, s.Borough)
			if err != nil {
				return page{}, err
			}
			return page{ls: p.Listings, totalPages: min(p.TotalPages, zillow.SoldPageCap), totalCount: p.TotalResults, dropped: p.Dropped}, nil
		},
		split: func() (band, band, bool) {
			lo, hi, ok := s.Split()
			return r.zlSoldBand(lo, f), r.zlSoldBand(hi, f), ok
		},
	}
}

// walkBand walks one search, or splits it when it reports more than its
// limit of results. Page 1 of a split search is still pushed: it costs
// nothing more. parentCount is the result count of the band this one was
// split from (0 for a whole borough): a half that reports as many results as
// its parent means the price filter is not working, so it is walked, not
// split again.
func (r *runner) walkBand(ctx context.Context, b band, seen map[string]bool, parentCount int) bool {
	first, err := b.fetch(ctx, 1)
	if err != nil {
		r.stopped(b.name, b.source, 1, err)
		return false
	}
	lo, hi, ok := b.split()
	if parentCount > 0 && first.totalCount >= parentCount {
		r.cfg.Log.Printf("%s: WARNING %d results, no fewer than the whole band; price filter ignored? walking the first %d pages only",
			b.name, first.totalCount, b.pageCap)
		ok = false
	}
	if first.totalCount <= b.limit || !ok {
		return r.search(ctx, b.name, b.source, b.market, seen, &first, b.fetch)
	}
	r.cfg.Log.Printf("%s: %d results > %d, splitting at $%d", b.name, first.totalCount, b.limit, lo.maxPrice)
	first.totalPages = 1 // keep page 1, read the rest through the bands
	if !r.search(ctx, b.name+" (page 1 before split)", b.source, b.market, seen, &first, b.fetch) {
		return false
	}
	return r.walkBand(ctx, lo, seen, first.totalCount) && r.walkBand(ctx, hi, seen, first.totalCount)
}

var (
	// errBudget: the sold run's Zillow request budget is spent.
	errBudget = errors.New("request budget spent")
	// errZillowBlocked: Zillow blocked an earlier request of this run.
	errZillowBlocked = errors.New("zillow blocked an earlier request of this run")
)

// zillowGate is the way every Zillow search page goes: it lets at most
// budget requests through (budget < 0: no limit) and none once Zillow has
// blocked one (see stopped).
type zillowGate struct {
	f       fetch.Fetcher
	budget  int
	used    int
	blocked error
}

func (g *zillowGate) Fetch(ctx context.Context, u string) (string, error) {
	if g.blocked != nil {
		return "", errZillowBlocked
	}
	if g.left() == 0 {
		return "", errBudget
	}
	g.used++
	return g.f.Fetch(ctx, u)
}

// left is how many requests the budget still allows (-1: no limit).
func (g *zillowGate) left() int {
	if g.budget < 0 {
		return -1
	}
	return max(g.budget-g.used, 0)
}

// SoldSlotStart is the NYCSoldSlots slot a sold run starts at: the UTC day
// number modulo the slot count, so daily runs move on one slot a day and
// cycle forever without keeping a cursor anywhere.
func SoldSlotStart(now time.Time, slots int) int {
	day := now.UTC().Unix() / 86400
	return int(day % int64(slots))
}

// zillowNYCSold reads Zillow's recently-sold search for NYC: the slots of
// zillow.NYCSoldSlots (borough x price band), from today's SoldSlotStart on,
// until the run's Zillow budget is spent. A slot reporting more than
// SoldBandLimit results is split further (Zillow serves 20 pages of 41).
// The rows are comps only: status sold, compOnly, soldAt. Building type and
// neighbourhood are filled in by the scorer (scorer/src/nyc_sold.rs).
func (r *runner) zillowNYCSold(ctx context.Context) {
	if r.zl.blocked != nil {
		r.cfg.Log.Printf("zillow nyc sold: skipped (Zillow blocked this run)")
		return
	}
	slots := zillow.NYCSoldSlots(r.cfg.SoldWindow)
	start := SoldSlotStart(r.cfg.Now(), len(slots))
	r.cfg.Log.Printf("zillow nyc sold: starting at slot %d/%d (%s), %d of %d Zillow requests left",
		start+1, len(slots), slots[start].Label(), r.zl.left(), r.zl.budget)
	seen := map[string]bool{}
	for i := range slots {
		if r.zl.left() == 0 {
			r.cfg.Log.Printf("zillow nyc sold: budget spent after %d slots; the next run starts at the next slot", i)
			return
		}
		k := (start + i) % len(slots)
		s := slots[k]
		before := len(r.res.Listings)
		ok := r.walkBand(ctx, r.zlSoldBand(s, r.zl), seen, 0)
		r.cfg.Log.Printf("zillow nyc sold slot %d/%d %s: %d sold rows (window %s), %d of %d requests left",
			k+1, len(slots), s.Label(), len(r.res.Listings)-before, s.Window, r.zl.left(), r.zl.budget)
		if !ok {
			return
		}
	}
}

// stopped records why a source stopped at page n: a failure; a log line
// when only the sold run's budget ran out; a warning, and no more Zillow
// requests this run, when Zillow blocked us (403 / 429 / bot check).
func (r *runner) stopped(name, source string, n int, err error) {
	switch {
	case errors.Is(err, errBudget):
		r.cfg.Log.Printf("%s: stopping %s at page %d: %v (the rest waits for the next run)", name, source, n, err)
	case errors.Is(err, errZillowBlocked):
		r.cfg.Log.Printf("%s: skipped (Zillow blocked this run)", name)
	case source == listing.SourceZillow && blocked(err):
		r.zl.blocked = err
		r.cfg.Log.Printf("WARNING %s: Zillow is blocking us at page %d: %v; no more Zillow requests this run", name, n, err)
	default:
		r.fail("%s: stopping %s at page %d: %v", name, source, n, err)
	}
}

func (r *runner) zillow(ctx context.Context) {
	sold := r.cfg.Mode == ModeSold
	for _, c := range zillow.Counties {
		if r.zl.blocked != nil {
			return
		}
		name := "zillow mi " + c.Slug
		if sold {
			name += " sold"
		}
		ok := r.search(ctx, name, listing.SourceZillow, listing.MarketMI, map[string]bool{}, nil,
			func(ctx context.Context, n int) (page, error) {
				body, err := r.zl.Fetch(ctx, zillow.SearchURL(c, n, sold))
				if err != nil {
					return page{}, err
				}
				p, err := zillow.ParseSearch(body, c, sold)
				if err != nil {
					return page{}, err
				}
				return page{ls: p.Listings, totalPages: p.TotalPages, totalCount: p.TotalResults, dropped: p.Dropped}, nil
			})
		if !ok {
			return
		}
	}
}

// detailHosts are the only hosts a needs-detail URL may point at.
var detailHosts = map[string]string{
	listing.MarketNYC: "streeteasy.com",
	listing.MarketMI:  "zillow.com",
}

func (r *runner) details(ctx context.Context, market string) {
	if market == listing.MarketMI && r.zl.blocked != nil {
		r.cfg.Log.Printf("detail %s: skipped (Zillow blocked this run)", market)
		return
	}
	nd, err := r.cfg.API.NeedsDetail(ctx, market, r.cfg.DetailLimit)
	if err != nil {
		r.fail("detail %s: needs-detail: %v", market, err)
		return
	}
	var out []listing.Detail
	read, gone := 0, 0
	var stopErr error
	for i, id := range nd.IDs {
		u := nd.URLs[i]
		if !allowedURL(u, detailHosts[market]) {
			r.cfg.Log.Printf("detail %s: skipping %s: URL %q is not on %s", market, id, u, detailHosts[market])
			continue
		}
		body, err := r.detail.Fetch(ctx, u)
		var he *fetch.HTTPError
		if errors.As(err, &he) && (he.Status == http.StatusNotFound || he.Status == http.StatusGone) {
			gone++
			continue // delisted; the daily sweep expires it
		}
		if err != nil {
			stopErr = err
			break
		}
		d, err := r.parseDetail(market, id, body)
		if err != nil {
			stopErr = err
			break
		}
		read++
		out = append(out, d)
	}
	r.res.Details = append(r.res.Details, out...)
	push := "push skipped (dry run)"
	if !r.cfg.DryRun && len(out) > 0 {
		s, err := r.cfg.API.PushDetails(ctx, out)
		if err != nil {
			r.fail("detail %s: push: %v", market, err)
			push = "push FAILED"
		} else {
			push = "push " + s.String()
		}
	} else if len(out) == 0 {
		push = "nothing to push"
	}
	r.cfg.Log.Printf("detail %s %s: asked %d, read %d, gone %d, %s", market, r.cfg.Mode, len(nd.IDs), read, gone, push)
	switch {
	case stopErr == nil:
	case blocked(stopErr):
		// The site wants a break: no more detail pages this run, but the
		// listings and scores are fine, so the run does not fail for it.
		r.cfg.Log.Printf("WARNING detail %s: blocked, no more detail reads this run: %v", market, stopErr)
		if market == listing.MarketMI {
			r.zl.blocked = stopErr
		}
	default:
		r.fail("detail %s: stopping: %v", market, stopErr)
	}
}

// blocked reports a 403 / 429 or a bot-check page.
func blocked(err error) bool {
	var he *fetch.HTTPError
	if errors.As(err, &he) {
		return he.Status == http.StatusForbidden || he.Status == http.StatusTooManyRequests
	}
	return strings.Contains(err.Error(), "bot check")
}

// scorePageSize is how many rows each score-input request asks for (tests shrink it).
var scorePageSize = apistore.ScoreInputPageSize

// scoreChunk is the most "statements" one POST /api/scores carries: an
// upsert or a delete is one, an alert two (insert, refresh).
const scoreChunk = 50

// score reads market's scoring rows from the Worker, runs the scorer on
// them and posts its writes in chunks, alerts first (so a failed post never
// leaves a rewritten score whose alert was lost: the next run retries).
func (r *runner) score(ctx context.Context, market string) {
	var rows []json.RawMessage
	var columns json.RawMessage
	after := ""
	for {
		p, err := r.cfg.API.ScoreInput(ctx, market, after, scorePageSize)
		if err != nil {
			r.fail("score %s: score-input after %q: %v", market, after, err)
			return
		}
		columns = p.Columns
		rows = append(rows, p.Rows...)
		if p.Last == nil || p.N < scorePageSize {
			break
		}
		after = *p.Last
	}
	if rows == nil {
		rows = []json.RawMessage{}
	}
	input, err := json.Marshal(map[string]any{
		"market": market, "now": listing.Timestamp(r.cfg.Now()), "columns": columns, "rows": rows,
	})
	if err != nil {
		r.fail("score %s: %v", market, err)
		return
	}
	raw, err := r.cfg.Scorer.Score(ctx, input)
	if err != nil {
		r.fail("score %s: scorer: %v", market, err)
		return
	}
	var w apistore.Scores
	if err := json.Unmarshal(raw, &w); err != nil {
		r.fail("score %s: scorer output is not JSON: %v", market, err)
		return
	}
	var total apistore.ScoresStats
	posted := 0
	for _, c := range chunkScores(market, w) {
		s, err := r.cfg.API.PostScores(ctx, c)
		if err != nil {
			r.fail("score %s: post scores: %v (posted %d chunks before failing)", market, err, posted)
			return
		}
		posted++
		total = total.Add(s)
	}
	r.cfg.Log.Printf("score %s: rows %d, upserts %d, deletes %d, alerts %d (new %d, refreshed %d)",
		market, len(rows), total.Upserted, total.Deleted, len(w.Alerts), total.NewAlerts, total.PatchedAlerts)
}

// chunkScores cuts the writes into POST bodies of at most scoreChunk
// statements: alerts first, then upserts, then deletes.
func chunkScores(market string, w apistore.Scores) []apistore.Scores {
	var out []apistore.Scores
	cur := apistore.Scores{Market: market}
	used := 0
	add := func(cost int, put func(*apistore.Scores)) {
		if used+cost > scoreChunk {
			out = append(out, cur)
			cur, used = apistore.Scores{Market: market}, 0
		}
		put(&cur)
		used += cost
	}
	for _, a := range w.Alerts {
		add(2, func(s *apistore.Scores) { s.Alerts = append(s.Alerts, a) })
	}
	for _, u := range w.Upserts {
		add(1, func(s *apistore.Scores) { s.Upserts = append(s.Upserts, u) })
	}
	for _, d := range w.Deletes {
		add(1, func(s *apistore.Scores) { s.Deletes = append(s.Deletes, d) })
	}
	if used > 0 {
		out = append(out, cur)
	}
	return out
}

func (r *runner) parseDetail(market, id, body string) (listing.Detail, error) {
	d := listing.Detail{ID: id, DetailReadAt: listing.Timestamp(r.cfg.Now())}
	switch market {
	case listing.MarketNYC:
		sd, err := streeteasy.ParseDetail(body)
		if err != nil {
			return d, err
		}
		d.Maintenance, d.Taxes, d.YearBuilt = sd.Maintenance, sd.Taxes, sd.YearBuilt
	case listing.MarketMI:
		zd, err := zillow.ParseDetail(body)
		if err != nil {
			return d, err
		}
		d.Description = listing.Str(listing.Truncate(zd.Description, listing.MaxDescription))
		d.WaterType = listing.Str(zd.Water.Type)
		d.WaterBody = listing.Str(zd.Water.Body)
		if d.WaterType != nil {
			d.WaterSource = listing.Ptr(listing.WaterFromDescription)
		}
		d.FrontageFt = zd.Water.FrontageFt
		d.YearBuilt = zd.YearBuilt
	}
	return d, nil
}

func allowedURL(raw, host string) bool {
	u, err := url.Parse(raw)
	if err != nil || u.Scheme != "https" {
		return false
	}
	h := u.Hostname()
	return h == host || strings.HasSuffix(h, "."+host)
}
