// Package crawl runs one crawl: the searches of a mode, a push per search,
// then the detail pages the Worker asks for.
//
// Every request goes through the one Fetcher it is given (the CLI wraps a
// single 1.5 s Throttle around it). A source stops at its first error that
// survived the fetcher's retries (a 403, a bot-check page, a page that
// changed shape); what it gathered before is still pushed, and Run reports
// the failure so the CLI exits non-zero.
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
	Out     io.Writer
	Log     *log.Logger
	Now     func() time.Time
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
		return 15
	case ModeFull:
		return 40
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
}

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
	r := &runner{cfg: cfg, seenAt: listing.Timestamp(cfg.Now())}
	r.res.Listings = []listing.Listing{}
	r.res.Details = []listing.Detail{}

	if r.has(listing.MarketNYC) && cfg.Mode != ModeSold {
		r.streetEasy(ctx)
	}
	if r.has(listing.MarketMI) {
		r.zillow(ctx)
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
		r.fail("%s: stopping %s at page %d: %v", name, source, pages+1, stopErr)
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
		if !r.seBand(ctx, streeteasy.Search{Area: area, MaxPrice: streeteasy.MaxSearchPrice}, seen, 0) {
			return
		}
	}
}

// seBand walks one search, or splits it when it reports too many results.
// Page 1 of a split search is still pushed: it costs nothing more.
// parentCount is the result count of the band this one was split from (0
// for a borough): a half that reports as many results as its parent means
// the price filter is not working, so it is walked, not split again.
func (r *runner) seBand(ctx context.Context, s streeteasy.Search, seen map[string]bool, parentCount int) bool {
	name := "streeteasy nyc " + s.Label()
	fetchPage := r.sePage(s)
	first, err := fetchPage(ctx, 1)
	if err != nil {
		r.fail("%s: stopping %s at page 1: %v", name, listing.SourceStreetEasy, err)
		return false
	}
	lo, hi, ok := s.Split()
	if parentCount > 0 && first.totalCount >= parentCount {
		r.cfg.Log.Printf("%s: WARNING %d results, no fewer than the whole band; price filter ignored? walking the first %d pages only",
			name, first.totalCount, streeteasy.PageCap)
		ok = false
	}
	if first.totalCount <= streeteasy.BandLimit || !ok {
		return r.search(ctx, name, listing.SourceStreetEasy, listing.MarketNYC, seen, &first, fetchPage)
	}
	r.cfg.Log.Printf("%s: %d results > %d, splitting at $%d", name, first.totalCount, streeteasy.BandLimit, lo.MaxPrice)
	first.totalPages = 1 // keep page 1, read the rest through the bands
	if !r.search(ctx, name+" (page 1 before split)", listing.SourceStreetEasy, listing.MarketNYC, seen, &first, fetchPage) {
		return false
	}
	return r.seBand(ctx, lo, seen, first.totalCount) && r.seBand(ctx, hi, seen, first.totalCount)
}

func (r *runner) zillow(ctx context.Context) {
	sold := r.cfg.Mode == ModeSold
	for _, c := range zillow.Counties {
		name := "zillow mi " + c.Slug
		if sold {
			name += " sold"
		}
		ok := r.search(ctx, name, listing.SourceZillow, listing.MarketMI, map[string]bool{}, nil,
			func(ctx context.Context, n int) (page, error) {
				body, err := r.cfg.Fetcher.Fetch(ctx, zillow.SearchURL(c, n, sold))
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
		body, err := r.cfg.Fetcher.Fetch(ctx, u)
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
	if stopErr != nil {
		r.fail("detail %s: stopping: %v", market, stopErr)
	}
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
