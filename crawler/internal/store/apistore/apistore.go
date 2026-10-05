// Package apistore talks to the housedeals Worker (cloudflare/worker, see
// CONTRACT.md): it pushes listings (POST /api/listings), asks which listings
// need a detail read (POST /api/listings/needs-detail) and pushes what the
// detail pages said (POST /api/listings/detail). Every route needs the
// Worker's INGEST_TOKEN as a bearer token.
package apistore

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"

	"housedeals/crawler/internal/listing"
)

// Worker routes.
const (
	IngestPath      = "/api/listings"
	NeedsDetailPath = "/api/listings/needs-detail"
	DetailPath      = "/api/listings/detail"
)

// DefaultBatchSize keeps each request (and the D1 batch it becomes) small:
// the Worker has ~10 ms of CPU per request on the Free plan.
const DefaultBatchSize = 25

// TokenEnv is the environment variable holding the ingest token.
const TokenEnv = "HOUSEDEALS_INGEST_TOKEN"

// Scope tells the Worker what a push covers.
type Scope struct {
	Source string `json:"source"`
	Market string `json:"market"`
	Mode   string `json:"mode"` // quick, full or sold
	SeenAt string `json:"seenAt"`
}

// Stats is the Worker's answer to POST /api/listings, summed over batches.
type Stats struct {
	Seen       int `json:"seen"`
	Added      int `json:"added"`
	Updated    int `json:"updated"`
	Unchanged  int `json:"unchanged"`
	PriceDrops int `json:"priceDrops"`
	PriceRises int `json:"priceRises"`
	Relisted   int `json:"relisted"`
	Scored     int `json:"scored"`
	NewAlerts  int `json:"newAlerts"`
}

// Add sums two answers.
func (a Stats) Add(b Stats) Stats {
	return Stats{
		Seen: a.Seen + b.Seen, Added: a.Added + b.Added, Updated: a.Updated + b.Updated,
		Unchanged: a.Unchanged + b.Unchanged, PriceDrops: a.PriceDrops + b.PriceDrops,
		PriceRises: a.PriceRises + b.PriceRises, Relisted: a.Relisted + b.Relisted,
		Scored: a.Scored + b.Scored, NewAlerts: a.NewAlerts + b.NewAlerts,
	}
}

func (a Stats) String() string {
	return fmt.Sprintf("seen=%d added=%d updated=%d unchanged=%d drops=%d rises=%d relisted=%d scored=%d alerts=%d",
		a.Seen, a.Added, a.Updated, a.Unchanged, a.PriceDrops, a.PriceRises, a.Relisted, a.Scored, a.NewAlerts)
}

// DetailStats is the Worker's answer to POST /api/listings/detail.
type DetailStats struct {
	Updated   int `json:"updated"`
	Scored    int `json:"scored"`
	NewAlerts int `json:"newAlerts"`
}

// Add sums two answers.
func (a DetailStats) Add(b DetailStats) DetailStats {
	return DetailStats{Updated: a.Updated + b.Updated, Scored: a.Scored + b.Scored, NewAlerts: a.NewAlerts + b.NewAlerts}
}

func (a DetailStats) String() string {
	return fmt.Sprintf("updated=%d scored=%d alerts=%d", a.Updated, a.Scored, a.NewAlerts)
}

// NeedsDetail is the Worker's list of listings whose detail page to read.
type NeedsDetail struct {
	IDs  []string `json:"ids"`
	URLs []string `json:"urls"`
}

// Client talks to the Worker.
type Client struct {
	Base      string // Worker base URL, no trailing slash
	Token     string
	HTTP      *http.Client
	BatchSize int
}

// New checks the API URL and token. apiURL may be the Worker's base URL
// (https://housedeals-api.example.workers.dev) or the full ingest URL. Plain
// http is only accepted for localhost, so the token never crosses the
// network in clear text.
func New(apiURL, token string) (*Client, error) {
	if strings.TrimSpace(token) == "" {
		return nil, errors.New("no ingest token: set " + TokenEnv)
	}
	u, err := url.Parse(strings.TrimSpace(apiURL))
	if err != nil || u.Host == "" || (u.Scheme != "https" && u.Scheme != "http") {
		return nil, fmt.Errorf("--push: %q is not an http(s) URL", apiURL)
	}
	if u.Scheme == "http" && !isLoopback(u.Hostname()) {
		return nil, fmt.Errorf("--push: refusing plain http to %s (the token would travel unencrypted); use https", u.Host)
	}
	u.Path = strings.TrimSuffix(strings.TrimRight(u.Path, "/"), IngestPath)
	u.RawQuery, u.Fragment = "", ""
	return &Client{
		Base:      strings.TrimRight(u.String(), "/"),
		Token:     strings.TrimSpace(token),
		HTTP:      &http.Client{Timeout: 60 * time.Second},
		BatchSize: DefaultBatchSize,
	}, nil
}

func isLoopback(host string) bool {
	if host == "localhost" {
		return true
	}
	ip := net.ParseIP(host)
	return ip != nil && ip.IsLoopback()
}

func (c *Client) batchSize() int {
	if c.BatchSize <= 0 {
		return DefaultBatchSize
	}
	return c.BatchSize
}

// Push posts listings in batches and sums the Worker's answers. It stops at
// the first failed batch and returns what the earlier batches did.
func (c *Client) Push(ctx context.Context, listings []listing.Listing, scope Scope) (Stats, error) {
	var total Stats
	size := c.batchSize()
	for start := 0; start < len(listings); start += size {
		end := min(start+size, len(listings))
		var s Stats
		body := struct {
			Listings []listing.Listing `json:"listings"`
			Scope    Scope             `json:"scope"`
		}{listings[start:end], scope}
		if err := c.post(ctx, IngestPath, body, &s); err != nil {
			return total, fmt.Errorf("push listings %d-%d of %d: %w", start+1, end, len(listings), err)
		}
		total = total.Add(s)
	}
	return total, nil
}

// NeedsDetail asks which listings of market need their detail page read.
func (c *Client) NeedsDetail(ctx context.Context, market string, limit int) (NeedsDetail, error) {
	var out NeedsDetail
	body := map[string]any{"market": market, "limit": limit}
	if err := c.post(ctx, NeedsDetailPath, body, &out); err != nil {
		return NeedsDetail{}, err
	}
	if len(out.URLs) != len(out.IDs) {
		return NeedsDetail{}, fmt.Errorf("%s: %d ids but %d urls", NeedsDetailPath, len(out.IDs), len(out.URLs))
	}
	return out, nil
}

// PushDetails posts detail reads in batches.
func (c *Client) PushDetails(ctx context.Context, details []listing.Detail) (DetailStats, error) {
	var total DetailStats
	size := c.batchSize()
	for start := 0; start < len(details); start += size {
		end := min(start+size, len(details))
		var s DetailStats
		body := struct {
			Listings []listing.Detail `json:"listings"`
		}{details[start:end]}
		if err := c.post(ctx, DetailPath, body, &s); err != nil {
			return total, fmt.Errorf("push details %d-%d of %d: %w", start+1, end, len(details), err)
		}
		total = total.Add(s)
	}
	return total, nil
}

// RetryDelay is the pause before resending a request the Worker answered 503.
var RetryDelay = 3 * time.Second

// post sends body to path and decodes a 200 answer into out, once more after
// a 503: a Worker request that ran out of CPU (Error 1102) has usually stored
// its rows already, so the resend finds them known and is cheap. Every route
// is idempotent.
func (c *Client) post(ctx context.Context, path string, body, out any) error {
	payload, err := json.Marshal(body)
	if err != nil {
		return err
	}
	status, err := c.postOnce(ctx, path, payload, out)
	if err == nil || status != http.StatusServiceUnavailable {
		return err
	}
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-time.After(RetryDelay):
	}
	_, err = c.postOnce(ctx, path, payload, out)
	return err
}

func (c *Client) postOnce(ctx context.Context, path string, payload []byte, out any) (int, error) {
	endpoint := c.Base + path
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(payload))
	if err != nil {
		return 0, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	req.Header.Set("Authorization", "Bearer "+c.Token)
	req.Header.Set("User-Agent", "housedeals-crawler")
	client := c.HTTP
	if client == nil {
		client = http.DefaultClient
	}
	resp, err := client.Do(req)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode != http.StatusOK {
		var e struct {
			Error string `json:"error"`
		}
		msg := strings.TrimSpace(string(raw))
		if json.Unmarshal(raw, &e) == nil && e.Error != "" {
			msg = e.Error
		}
		if len(msg) > 300 {
			msg = msg[:300] + "..."
		}
		// Cloudflare Access answers an unauthenticated request with a redirect
		// to its login page, which the client follows to an HTML page.
		if strings.Contains(msg, "<html") || strings.Contains(msg, "<!DOCTYPE") {
			msg = "got an HTML page, not the API (is Cloudflare Access blocking " + path + "?)"
		}
		return resp.StatusCode, fmt.Errorf("%s: HTTP %d: %s", endpoint, resp.StatusCode, msg)
	}
	if err := json.Unmarshal(raw, out); err != nil {
		return resp.StatusCode, fmt.Errorf("%s: answer is not the expected JSON: %w", endpoint, err)
	}
	return resp.StatusCode, nil
}
