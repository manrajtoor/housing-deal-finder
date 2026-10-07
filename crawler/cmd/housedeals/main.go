// Command housedeals crawls StreetEasy (NYC) and Zillow (northern Michigan)
// and pushes the listings to the housedeals Worker.
//
//	housedeals --mode quick|full|sold [--market nyc|mi|all] [--push URL]
//	           [--dry-run] [--max-pages N] [--detail-limit N] [--delay 1.5s]
//	           [--scorer PATH | --no-score] [--sold-window 6m] [--sold-budget 25]
//
// The ingest token comes from HOUSEDEALS_INGEST_TOKEN. --dry-run prints the
// listings (and any detail reads) as JSON on stdout and pushes nothing.
// After pushing, each market crawled is scored: its rows are read back from
// the Worker (GET /api/score-input), run through the housedeals-score
// binary (scorer/) and the scores posted (POST /api/scores).
// Logs go to stderr, one line per search, detail step and market scored.
// The exit status is 1 when a source, a push, a detail read or the scoring
// failed (what was gathered is still pushed). Being blocked is not a
// failure: a Zillow 403 / 429 / bot check stops every Zillow request of the
// run, and a blocked StreetEasy detail page the NYC detail reads, with a
// warning and exit status 0.
package main

import (
	"bytes"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"housedeals/crawler/internal/crawl"
	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/listing"
	"housedeals/crawler/internal/store/apistore"
)

// throttleDelay spaces every request of a run (tests shorten it).
var throttleDelay = fetch.DefaultDelay

// detailDelay also spaces detail pages: Zillow answered 403 after 14 detail
// pages 1.5 s apart (tests shorten it).
var detailDelay = 6 * time.Second

// zillowSoldDelay spaces Zillow search pages in sold mode: from a GitHub
// runner Zillow answered 429 after ~22 search pages 1.5 s apart (tests
// shorten it).
var zillowSoldDelay = 8 * time.Second

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	os.Exit(run(ctx, os.Args[1:], os.Getenv, os.Stdout, os.Stderr, nil))
}

type options struct {
	mode, market, push, scorer string
	soldWindow                 string
	dryRun, noScore            bool
	maxPages, detailLimit      int
	soldBudget                 int
	delay                      time.Duration
}

func parseFlags(args []string, stderr io.Writer) (options, error) {
	var o options
	fs := flag.NewFlagSet("housedeals", flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.StringVar(&o.mode, "mode", "", "quick (newest page of each search), full (every page) or sold (sold comps: Michigan, and NYC from Zillow)")
	fs.StringVar(&o.market, "market", "all", "nyc, mi or all")
	fs.StringVar(&o.push, "push", "", "Worker base URL to push to (token from "+apistore.TokenEnv+")")
	fs.BoolVar(&o.dryRun, "dry-run", false, "print JSON to stdout, push nothing")
	fs.IntVar(&o.maxPages, "max-pages", 0, "pages per search (default: quick 1, full 60, sold 20)")
	fs.IntVar(&o.detailLimit, "detail-limit", -1, "detail pages per market (default: quick 6, full 20, sold 0)")
	fs.DurationVar(&o.delay, "delay", 0, "spacing between requests (default and minimum 1.5s)")
	fs.StringVar(&o.scorer, "scorer", "housedeals-score", "path of the scorer binary (scorer/, cargo build --release)")
	fs.BoolVar(&o.noScore, "no-score", false, "push listings and details but do not score")
	fs.StringVar(&o.soldWindow, "sold-window", "", "NYC sold window, Zillow's: 7, 14, 30, 90, 6m or 12m (default 6m)")
	fs.IntVar(&o.soldBudget, "sold-budget", crawl.DefaultSoldBudget, "Zillow requests per sold run, Michigan first, then NYC's rotation")
	if err := fs.Parse(args); err != nil {
		return o, err
	}
	if fs.NArg() > 0 {
		return o, fmt.Errorf("unexpected arguments: %v", fs.Args())
	}
	switch o.mode {
	case crawl.ModeQuick, crawl.ModeFull, crawl.ModeSold:
	default:
		return o, fmt.Errorf("--mode must be quick, full or sold (got %q)", o.mode)
	}
	switch o.market {
	case "nyc", "mi", "all":
	default:
		return o, fmt.Errorf("--market must be nyc, mi or all (got %q)", o.market)
	}
	switch o.soldWindow {
	case "", "7", "14", "30", "90", "6m", "12m":
	default:
		return o, fmt.Errorf("--sold-window must be 7, 14, 30, 90, 6m or 12m (got %q)", o.soldWindow)
	}
	if o.soldBudget <= 0 {
		return o, errors.New("--sold-budget must be positive")
	}
	if o.maxPages < 0 {
		return o, errors.New("--max-pages must be positive")
	}
	if o.push == "" && !o.dryRun {
		return o, errors.New("nothing to do with the listings: pass --push URL or --dry-run")
	}
	return o, nil
}

// run is main without the process: testable, and it returns the exit code.
// fetcher replaces the network when not nil.
func run(ctx context.Context, args []string, getenv func(string) string, stdout, stderr io.Writer, fetcher fetch.Fetcher) int {
	logger := log.New(stderr, "", log.Ltime)
	o, err := parseFlags(args, stderr)
	if err != nil {
		if !errors.Is(err, flag.ErrHelp) {
			fmt.Fprintln(stderr, "housedeals:", err)
		}
		return 2
	}
	var api crawl.API
	if o.push != "" {
		c, err := apistore.New(o.push, getenv(apistore.TokenEnv))
		if err != nil {
			fmt.Fprintln(stderr, "housedeals:", err)
			return 2
		}
		api = c
	}
	var scorer crawl.Scorer
	if api != nil && !o.dryRun && !o.noScore {
		path, err := exec.LookPath(o.scorer)
		if err != nil {
			fmt.Fprintf(stderr, "housedeals: scorer %q not found (build scorer/ or pass --no-score): %v\n", o.scorer, err)
			return 2
		}
		scorer = execScorer{path: path}
	}
	markets := []string{listing.MarketNYC, listing.MarketMI}
	if o.market != "all" {
		markets = []string{o.market}
	}
	if fetcher == nil {
		hf := fetch.NewHTTPFetcher()
		hf.NoRetry429 = isZillow // Zillow's 429 lasts; retries would only add to it
		fetcher = hf
	}
	// One throttle for every request of the run, whatever the source.
	fetcher = fetch.NewThrottle(max(o.delay, throttleDelay)).Wrap(fetcher)

	res := crawl.Run(ctx, crawl.Config{
		Mode: o.mode, Markets: markets, MaxPages: o.maxPages, DetailLimit: o.detailLimit,
		DryRun: o.dryRun, Fetcher: fetcher, API: api, Scorer: scorer, DetailDelay: detailDelay, SoldWindow: o.soldWindow,
		ZillowDelay: zillowSoldDelay, SoldBudget: o.soldBudget,
		Out: stdout, Log: logger,
	})
	logger.Printf("done: %d listings, %d detail reads, %d failures", len(res.Listings), len(res.Details), len(res.Failures))
	if len(res.Failures) > 0 {
		return 1
	}
	return 0
}

func isZillow(raw string) bool {
	u, err := url.Parse(raw)
	if err != nil {
		return false
	}
	h := u.Hostname()
	return h == "zillow.com" || strings.HasSuffix(h, ".zillow.com")
}

// execScorer runs the housedeals-score binary: input on stdin, writes on stdout.
type execScorer struct{ path string }

func (e execScorer) Score(ctx context.Context, input []byte) ([]byte, error) {
	cmd := exec.CommandContext(ctx, e.path)
	cmd.Stdin = bytes.NewReader(input)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("%s: %v: %s", e.path, err, strings.TrimSpace(stderr.String()))
	}
	return stdout.Bytes(), nil
}
