package rsc

import (
	"testing"

	"housedeals/crawler/internal/jsonx"
)

func TestPayloadAndRows(t *testing.T) {
	// A text row (T, byte length in hex) with newlines inside, then a JSON
	// row right after it on the same line, split across two push calls.
	html := `<script>(self.__next_f=self.__next_f||[]).push([0])</script>` +
		`<script>self.__next_f.push([1,"0:[\"$\",\"html\"]\n5:T6,a\nb\nc\n7:{\"geo\":\"$8\",\"photos\":\"$9\",\"lit\":\"$L1\",\"und\":\"$undefined\"}\n8:{\"latitude\":40.7"])</script>` +
		`<script>self.__next_f.push([1,"1}\n9:[\"$a\",\"$b\"]\na:{\"url\":\"https://x/1\"}\nb:{\"url\":\"https://x/2\"}\n"])</script>`
	p, err := Payload(html)
	if err != nil {
		t.Fatal(err)
	}
	st := Parse(p)
	if _, ok := st.Rows["5"]; ok {
		t.Error("text rows are not JSON rows")
	}
	v := st.Resolve(st.Row("7"))
	if lat, _ := jsonx.Float(jsonx.Get(v, "geo", "latitude")); lat != 40.71 {
		t.Errorf("geo = %v", jsonx.Get(v, "geo"))
	}
	if u := jsonx.String(jsonx.Get(v, "photos", 1, "url")); u != "https://x/2" {
		t.Errorf("photos = %v", jsonx.Get(v, "photos"))
	}
	if jsonx.String(jsonx.Get(v, "lit")) != "$L1" || jsonx.String(jsonx.Get(v, "und")) != "$undefined" {
		t.Errorf("non-row references must stay: %v", v)
	}
	objs := st.ObjectsAfter(`"geo":`)
	if len(objs) != 1 || objs[0] != "$8" {
		t.Errorf("ObjectsAfter = %v", objs)
	}
}

func TestNoPayload(t *testing.T) {
	if _, err := Payload("<html>blocked</html>"); err != ErrNoPayload {
		t.Errorf("err = %v", err)
	}
}

func TestSelfReferenceTerminates(t *testing.T) {
	st := Parse("1:{\"me\":\"$1\"}\n")
	_ = st.Resolve(st.Row("1")) // must not loop forever
}
