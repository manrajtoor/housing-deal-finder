package main

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"strings"
	"testing"

	"housedeals/crawler/internal/fetch"
	"housedeals/crawler/internal/streeteasy"
	"housedeals/crawler/internal/zillow"
)

func init() { throttleDelay = 0 }

func noEnv(string) string { return "" }

func TestFlagErrors(t *testing.T) {
	cases := []struct {
		args []string
		want string
	}{
		{[]string{"--dry-run"}, "--mode must be quick, full or sold"},
		{[]string{"--mode", "weekly", "--dry-run"}, "--mode must be"},
		{[]string{"--mode", "quick", "--market", "la", "--dry-run"}, "--market must be nyc, mi or all"},
		{[]string{"--mode", "quick"}, "pass --push URL or --dry-run"},
		{[]string{"--mode", "quick", "--push", "https://api.example"}, "HOUSEDEALS_INGEST_TOKEN"},
		{[]string{"--mode", "quick", "--dry-run", "extra"}, "unexpected arguments"},
	}
	for _, c := range cases {
		var stderr bytes.Buffer
		code := run(context.Background(), c.args, noEnv, &bytes.Buffer{}, &stderr, fetch.Pages{})
		if code != 2 || !strings.Contains(stderr.String(), c.want) {
			t.Errorf("%v: code %d, stderr %q, want %q", c.args, code, stderr.String(), c.want)
		}
	}
}

func TestDryRunOffline(t *testing.T) {
	read := func(p string) string {
		b, err := os.ReadFile(p)
		if err != nil {
			t.Fatal(err)
		}
		return string(b)
	}
	pages := fetch.Pages{streeteasy.SearchURL(1): read("../../internal/streeteasy/testdata/search.html")}
	for _, c := range zillow.Counties {
		pages[zillow.SearchURL(c, 1, false)] = read("../../internal/zillow/testdata/search.html")
	}
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--mode", "quick", "--dry-run"}, noEnv, &stdout, &stderr, pages)
	if code != 0 {
		t.Fatalf("code %d\n%s", code, stderr.String())
	}
	var out struct {
		Listings []json.RawMessage `json:"listings"`
	}
	if err := json.Unmarshal(stdout.Bytes(), &out); err != nil || len(out.Listings) != 7+6*6 {
		t.Fatalf("listings %d, err %v", len(out.Listings), err)
	}
	if !strings.Contains(stderr.String(), "done: 43 listings, 0 detail reads, 0 failures") {
		t.Errorf("stderr:\n%s", stderr.String())
	}
}

func TestFailureExitsOne(t *testing.T) {
	var stderr bytes.Buffer
	code := run(context.Background(), []string{"--mode", "quick", "--market", "nyc", "--dry-run"}, noEnv, &bytes.Buffer{}, &stderr, fetch.Pages{})
	if code != 1 || !strings.Contains(stderr.String(), "HTTP 404") {
		t.Errorf("code %d\n%s", code, stderr.String())
	}
}
