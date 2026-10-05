// Package rsc reads the React Server Components ("flight") payload that a
// Next.js App Router page embeds as self.__next_f.push([1,"..."]) calls.
//
// The payload is a stream of rows, "<hex id>:<json>\n" (or "<hex id>:T<hex
// len>,<text>" for long strings). Objects refer to other rows with "$<hex id>"
// strings, so a listing's geoPoint may be "$24", defined by the row "24:{...}".
// Stream.Resolve follows those references.
package rsc

import (
	"bytes"
	"encoding/json"
	"errors"
	"regexp"
	"strconv"
	"strings"
)

var pushRe = regexp.MustCompile(`self\.__next_f\.push\(\[1,"((?:[^"\\]|\\.)*)"\]\)`)

// ErrNoPayload means the page has no flight chunks (a block page, a captcha,
// or a page that is no longer a Next.js App Router page).
var ErrNoPayload = errors.New("no Next.js flight payload (self.__next_f.push) in the page")

// Payload concatenates every self.__next_f.push([1,"..."]) string chunk of
// an HTML page, each JSON-unescaped.
func Payload(html string) (string, error) {
	var b strings.Builder
	n := 0
	for _, m := range pushRe.FindAllStringSubmatch(html, -1) {
		var s string
		if err := json.Unmarshal([]byte(`"`+m[1]+`"`), &s); err != nil {
			continue
		}
		b.WriteString(s)
		n++
	}
	if n == 0 {
		return "", ErrNoPayload
	}
	return b.String(), nil
}

// Stream is a parsed flight payload.
type Stream struct {
	Text string                     // the whole concatenated payload
	Rows map[string]json.RawMessage // JSON rows by hex id
}

// Parse splits a payload into rows. Rows that are not JSON (imports, hints,
// text rows) are left out of Rows but stay searchable in Text.
func Parse(payload string) *Stream {
	s := &Stream{Text: payload, Rows: map[string]json.RawMessage{}}
	i := 0
	for i < len(payload) {
		// Row id: hex digits up to ':'.
		j := i
		for j < len(payload) && isHex(payload[j]) {
			j++
		}
		if j == i || j >= len(payload) || payload[j] != ':' {
			i = nextLine(payload, i)
			continue
		}
		id := payload[i:j]
		j++
		if j < len(payload) && payload[j] == 'T' {
			// Text row: T<hex byte length>,<text>, no newline after it.
			k := strings.IndexByte(payload[j:], ',')
			if k < 0 {
				break
			}
			n, err := strconv.ParseInt(payload[j+1:j+k], 16, 64)
			if err != nil {
				i = nextLine(payload, j)
				continue
			}
			i = min(j+k+1+int(n), len(payload))
			continue
		}
		end := strings.IndexByte(payload[j:], '\n')
		if end < 0 {
			end = len(payload) - j
		}
		row := payload[j : j+end]
		if json.Valid([]byte(row)) {
			s.Rows[id] = json.RawMessage(row)
		}
		i = j + end + 1
	}
	return s
}

func nextLine(s string, i int) int {
	k := strings.IndexByte(s[i:], '\n')
	if k < 0 {
		return len(s)
	}
	return i + k + 1
}

func isHex(c byte) bool {
	return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f')
}

// IsRef reports whether v is a "$<hex id>" row reference.
func IsRef(v string) bool {
	if len(v) < 2 || v[0] != '$' {
		return false
	}
	for i := 1; i < len(v); i++ {
		if !isHex(v[i]) {
			return false
		}
	}
	return true
}

// Row decodes the row with the given id ("24" or "$24"), nil when absent.
func (s *Stream) Row(id string) any {
	raw, ok := s.Rows[strings.TrimPrefix(id, "$")]
	if !ok {
		return nil
	}
	v, err := DecodeAt(string(raw), 0)
	if err != nil {
		return nil
	}
	return v
}

// maxDepth bounds reference chasing (rows can refer to each other in cycles).
const maxDepth = 12

// Resolve replaces every "$<hex id>" string inside v with that row's value,
// recursively. References to unknown rows are left as they are.
func (s *Stream) Resolve(v any) any { return s.resolve(v, 0) }

func (s *Stream) resolve(v any, depth int) any {
	if depth > maxDepth {
		return v
	}
	switch t := v.(type) {
	case string:
		if IsRef(t) {
			if r := s.Row(t); r != nil {
				return s.resolve(r, depth+1)
			}
		}
		return t
	case []any:
		out := make([]any, len(t))
		for i, e := range t {
			out[i] = s.resolve(e, depth+1)
		}
		return out
	case map[string]any:
		out := make(map[string]any, len(t))
		for k, e := range t {
			out[k] = s.resolve(e, depth+1)
		}
		return out
	}
	return v
}

// ValueAt decodes the single JSON value that starts at offset in Text (for
// objects embedded inline in a bigger row).
func (s *Stream) ValueAt(offset int) (any, error) {
	return DecodeAt(s.Text, offset)
}

// DecodeAt decodes the JSON value that starts at offset in text, ignoring
// whatever follows it.
func DecodeAt(text string, offset int) (any, error) {
	if offset < 0 || offset >= len(text) {
		return nil, errors.New("offset out of range")
	}
	dec := json.NewDecoder(bytes.NewReader([]byte(text[offset:])))
	dec.UseNumber()
	var v any
	if err := dec.Decode(&v); err != nil {
		return nil, err
	}
	return v, nil
}

// ObjectsAfter decodes the JSON value that follows each match of key (for
// example `"listingData":`) in Text, in order.
func (s *Stream) ObjectsAfter(key string) []any {
	var out []any
	for i := 0; ; {
		k := strings.Index(s.Text[i:], key)
		if k < 0 {
			return out
		}
		at := i + k + len(key)
		if v, err := s.ValueAt(at); err == nil {
			out = append(out, v)
		}
		i = at
	}
}
