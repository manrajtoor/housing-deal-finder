// Command housedeals crawls StreetEasy (NYC) and Zillow (northern Michigan)
// and pushes the listings to the housedeals Worker.
//
//	housedeals --mode quick|full|sold [--market nyc|mi|all] [--push URL]
//	           [--dry-run] [--max-pages N] [--detail-limit N] [--delay 1.5s]
//
// The ingest token comes from HOUSEDEALS_INGEST_TOKEN. --dry-run prints the
// listings (and any detail reads) as JSON on stdout and pushes nothing.
// Logs go to stderr, one line per search. The exit status is 1 when a
// source, a push or a detail read failed (what was gathered is still pushed).
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	"housedeals/crawler/internal/crawl"
	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/listing"
	"housedeals/crawler/internal/store/apistore"
)

// throttleDelay spaces every request of a run (tests shorten it).
var throttleDelay = fetch.DefaultDelay

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	os.Exit(run(ctx, os.Args[1:], os.Getenv, os.Stdout, os.Stderr, nil))
}

type options struct {
	mode, market, push    string
	dryRun                bool
	maxPages, detailLimit int
	delay                 time.Duration
}

func parseFlags(args []string, stderr io.Writer) (options, error) {
	var o options
	fs := flag.NewFlagSet("housedeals", flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.StringVar(&o.mode, "mode", "", "quick (newest page of each search), full (every page) or sold (Michigan sold comps)")
	fs.StringVar(&o.market, "market", "all", "nyc, mi or all")
	fs.StringVar(&o.push, "push", "", "Worker base URL to push to (token from "+apistore.TokenEnv+")")
	fs.BoolVar(&o.dryRun, "dry-run", false, "print JSON to stdout, push nothing")
	fs.IntVar(&o.maxPages, "max-pages", 0, "pages per search (default: quick 1, full 60, sold 20)")
	fs.IntVar(&o.detailLimit, "detail-limit", -1, "detail pages per market (default: quick 15, full 40, sold 0)")
	fs.DurationVar(&o.delay, "delay", 0, "spacing between requests (default and minimum 1.5s)")
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
	markets := []string{listing.MarketNYC, listing.MarketMI}
	if o.market != "all" {
		markets = []string{o.market}
	}
	if fetcher == nil {
		fetcher = fetch.NewHTTPFetcher()
	}
	// One throttle for every request of the run, whatever the source.
	fetcher = fetch.NewThrottle(max(o.delay, throttleDelay)).Wrap(fetcher)

	res := crawl.Run(ctx, crawl.Config{
		Mode: o.mode, Markets: markets, MaxPages: o.maxPages, DetailLimit: o.detailLimit,
		DryRun: o.dryRun, Fetcher: fetcher, API: api, Out: stdout, Log: logger,
	})
	logger.Printf("done: %d listings, %d detail reads, %d failures", len(res.Listings), len(res.Details), len(res.Failures))
	if len(res.Failures) > 0 {
		return 1
	}
	return 0
}
