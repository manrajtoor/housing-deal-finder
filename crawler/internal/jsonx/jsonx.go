// Package jsonx reads loosely typed JSON (decoded into any, with or without
// UseNumber) without a struct per page shape.
package jsonx

import (
	"encoding/json"
	"math"
	"strconv"
	"strings"
)

// Get walks map keys (string) and slice indexes (int) from v; nil when any
// step is missing.
func Get(v any, path ...any) any {
	for _, p := range path {
		switch k := p.(type) {
		case string:
			m, ok := v.(map[string]any)
			if !ok {
				return nil
			}
			v = m[k]
		case int:
			a, ok := v.([]any)
			if !ok || k < 0 || k >= len(a) {
				return nil
			}
			v = a[k]
		default:
			return nil
		}
	}
	return v
}

// Map returns v as an object, or nil.
func Map(v any) map[string]any {
	m, _ := v.(map[string]any)
	return m
}

// Slice returns v as an array, or nil.
func Slice(v any) []any {
	a, _ := v.([]any)
	return a
}

// String returns v as a string ("" when it is not one).
func String(v any) string {
	s, _ := v.(string)
	return s
}

// Float returns v as a number. Numeric strings count; anything else is
// (0, false).
func Float(v any) (float64, bool) {
	switch t := v.(type) {
	case float64:
		return t, true
	case json.Number:
		f, err := t.Float64()
		return f, err == nil
	case int:
		return float64(t), true
	case int64:
		return float64(t), true
	case string:
		f, err := strconv.ParseFloat(strings.ReplaceAll(strings.TrimSpace(t), ",", ""), 64)
		return f, err == nil
	}
	return 0, false
}

// Int returns v rounded to an int.
func Int(v any) (int, bool) {
	f, ok := Float(v)
	if !ok || math.IsNaN(f) || math.IsInf(f, 0) {
		return 0, false
	}
	return int(math.Round(f)), true
}

// PosInt returns a pointer to v as an int when it is a positive number.
func PosInt(v any) *int {
	n, ok := Int(v)
	if !ok || n <= 0 {
		return nil
	}
	return &n
}

// PosFloat returns a pointer to v when it is a positive number.
func PosFloat(v any) *float64 {
	f, ok := Float(v)
	if !ok || f <= 0 {
		return nil
	}
	return &f
}

// NumPtr returns a pointer to v when it is a number (zero included).
func NumPtr(v any) *float64 {
	f, ok := Float(v)
	if !ok {
		return nil
	}
	return &f
}
