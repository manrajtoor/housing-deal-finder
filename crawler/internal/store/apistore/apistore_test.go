package apistore

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"housedeals/crawler/internal/listing"
)

func init() { RetryDelay = time.Millisecond }

type recorded struct {
	path, auth string
	body       map[string]any
}

// fakeWorker answers like the Worker: per path, a handler returns the status
// and JSON body.
func fakeWorker(t *testing.T, handle func(path string, body map[string]any, hit int) (int, any)) (*Client, *[]recorded) {
	t.Helper()
	var mu sync.Mutex
	var calls []recorded
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw, _ := io.ReadAll(r.Body)
		var body map[string]any
		_ = json.Unmarshal(raw, &body)
		mu.Lock()
		calls = append(calls, recorded{r.URL.Path, r.Header.Get("Authorization"), body})
		hit := len(calls)
		mu.Unlock()
		if r.Method != http.MethodPost || r.Header.Get("Content-Type") != "application/json" {
			w.WriteHeader(400)
			return
		}
		status, out := handle(r.URL.Path, body, hit)
		w.WriteHeader(status)
		switch o := out.(type) {
		case string:
			fmt.Fprint(w, o)
		default:
			_ = json.NewEncoder(w).Encode(o)
		}
	}))
	t.Cleanup(srv.Close)
	c, err := New(srv.URL, "secret")
	if err != nil {
		t.Fatal(err)
	}
	return c, &calls
}

func listings(n int) []listing.Listing {
	out := make([]listing.Listing, n)
	for i := range out {
		out[i] = listing.Listing{ID: fmt.Sprintf("zl:%d", i), Source: "zillow", Market: "mi", Status: "active",
			URL: "https://www.zillow.com/x", Address: "1 Main St", Price: 100000 + i, HomeType: "single_family"}
	}
	return out
}

func TestPushBatchesAndSums(t *testing.T) {
	c, calls := fakeWorker(t, func(path string, body map[string]any, _ int) (int, any) {
		n := len(body["listings"].([]any))
		return 200, Stats{Seen: n, Added: n - 1, Unchanged: 1, Scored: 2, NewAlerts: 1}
	})
	scope := Scope{Source: "zillow", Market: "mi", Mode: "quick", SeenAt: "2026-10-05T12:00:00Z"}
	s, err := c.Push(context.Background(), listings(60), scope)
	if err != nil {
		t.Fatal(err)
	}
	if len(*calls) != 3 {
		t.Fatalf("calls = %d, want 3 batches of <= 25", len(*calls))
	}
	if s.Seen != 60 || s.Added != 57 || s.Unchanged != 3 || s.Scored != 6 || s.NewAlerts != 3 {
		t.Errorf("stats = %+v", s)
	}
	first := (*calls)[0]
	if first.path != "/api/listings" || first.auth != "Bearer secret" {
		t.Errorf("call = %+v", first)
	}
	if got := len(first.body["listings"].([]any)); got != 25 {
		t.Errorf("first batch = %d", got)
	}
	sc := first.body["scope"].(map[string]any)
	if sc["source"] != "zillow" || sc["market"] != "mi" || sc["mode"] != "quick" || sc["seenAt"] != "2026-10-05T12:00:00Z" {
		t.Errorf("scope = %v", sc)
	}
	l := first.body["listings"].([]any)[0].(map[string]any)
	if l["compOnly"] != false || l["price"] != float64(100000) {
		t.Errorf("listing json = %v", l)
	}
	for _, k := range []string{"description", "waterType", "detailReadAt", "sqft"} {
		if _, ok := l[k]; ok {
			t.Errorf("unknown %s must be omitted, got %v", k, l[k])
		}
	}
}

func TestPushRetries503Once(t *testing.T) {
	c, calls := fakeWorker(t, func(_ string, body map[string]any, hit int) (int, any) {
		if hit == 1 {
			return 503, "error code: 1102"
		}
		return 200, Stats{Seen: len(body["listings"].([]any))}
	})
	s, err := c.Push(context.Background(), listings(3), Scope{})
	if err != nil || s.Seen != 3 || len(*calls) != 2 {
		t.Fatalf("stats %+v err %v calls %d", s, err, len(*calls))
	}
}

func TestPushGivesUpAfterSecond503(t *testing.T) {
	c, calls := fakeWorker(t, func(string, map[string]any, int) (int, any) { return 503, "busy" })
	_, err := c.Push(context.Background(), listings(3), Scope{})
	if err == nil || !strings.Contains(err.Error(), "HTTP 503") || len(*calls) != 2 {
		t.Fatalf("err %v calls %d", err, len(*calls))
	}
}

func TestPushStopsAtFirstFailedBatch(t *testing.T) {
	c, calls := fakeWorker(t, func(_ string, body map[string]any, hit int) (int, any) {
		if hit == 2 {
			return 401, map[string]string{"error": "bad token"}
		}
		return 200, Stats{Seen: len(body["listings"].([]any))}
	})
	s, err := c.Push(context.Background(), listings(60), Scope{})
	if err == nil || !strings.Contains(err.Error(), "bad token") || !strings.Contains(err.Error(), "26-50 of 60") {
		t.Fatalf("err = %v", err)
	}
	if s.Seen != 25 || len(*calls) != 2 {
		t.Errorf("stats %+v calls %d (401 is not retried)", s, len(*calls))
	}
}

func TestAccessLoginPage(t *testing.T) {
	c, _ := fakeWorker(t, func(string, map[string]any, int) (int, any) {
		return 403, "<!DOCTYPE html><html>Sign in</html>"
	})
	_, err := c.Push(context.Background(), listings(1), Scope{})
	if err == nil || !strings.Contains(err.Error(), "Cloudflare Access") {
		t.Fatalf("err = %v", err)
	}
}

func TestNeedsDetailAndPushDetails(t *testing.T) {
	c, calls := fakeWorker(t, func(path string, body map[string]any, _ int) (int, any) {
		switch path {
		case "/api/listings/needs-detail":
			return 200, NeedsDetail{IDs: []string{"zl:1", "zl:2"}, URLs: []string{"https://www.zillow.com/a", "https://www.zillow.com/b"}}
		case "/api/listings/detail":
			n := len(body["listings"].([]any))
			return 200, DetailStats{Updated: n, Scored: n, NewAlerts: 0}
		}
		return 404, "no route"
	})
	nd, err := c.NeedsDetail(context.Background(), "mi", 15)
	if err != nil || len(nd.IDs) != 2 || nd.URLs[1] != "https://www.zillow.com/b" {
		t.Fatalf("needs-detail = %+v, %v", nd, err)
	}
	if b := (*calls)[0].body; b["market"] != "mi" || b["limit"] != float64(15) {
		t.Errorf("needs-detail body = %v", b)
	}
	details := []listing.Detail{
		{ID: "zl:1", DetailReadAt: "2026-10-05T12:00:00Z", WaterType: listing.Ptr("inland"), WaterBody: listing.Ptr("Torch Lake"), FrontageFt: listing.Ptr(100)},
		{ID: "se:2", DetailReadAt: "2026-10-05T12:00:00Z", Maintenance: listing.Ptr(1500)},
	}
	ds, err := c.PushDetails(context.Background(), details)
	if err != nil || ds.Updated != 2 {
		t.Fatalf("details = %+v, %v", ds, err)
	}
	sent := (*calls)[1].body["listings"].([]any)
	d0, d1 := sent[0].(map[string]any), sent[1].(map[string]any)
	if d0["waterBody"] != "Torch Lake" || d0["frontageFt"] != float64(100) || d0["detailReadAt"] != "2026-10-05T12:00:00Z" {
		t.Errorf("detail 0 = %v", d0)
	}
	if _, ok := d1["waterType"]; ok || d1["maintenance"] != float64(1500) {
		t.Errorf("detail 1 = %v", d1)
	}
}

func TestNew(t *testing.T) {
	cases := []struct {
		url, token, wantBase, wantErr string
	}{
		{"https://housedeals-api.x.workers.dev", "t", "https://housedeals-api.x.workers.dev", ""},
		{"https://housedeals-api.x.workers.dev/", "t", "https://housedeals-api.x.workers.dev", ""},
		{"https://housedeals-api.x.workers.dev/api/listings", "t", "https://housedeals-api.x.workers.dev", ""},
		{"http://localhost:8787", "t", "http://localhost:8787", ""},
		{"http://example.com", "t", "", "refusing plain http"},
		{"ftp://example.com", "t", "", "not an http(s) URL"},
		{"https://example.com", " ", "", "HOUSEDEALS_INGEST_TOKEN"},
	}
	for _, c := range cases {
		got, err := New(c.url, c.token)
		if c.wantErr != "" {
			if err == nil || !strings.Contains(err.Error(), c.wantErr) {
				t.Errorf("New(%q) err = %v, want %q", c.url, err, c.wantErr)
			}
			continue
		}
		if err != nil || got.Base != c.wantBase {
			t.Errorf("New(%q) = %+v, %v", c.url, got, err)
		}
	}
}
