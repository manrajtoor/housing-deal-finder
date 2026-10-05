// Package water reads a Michigan listing description and says what water the
// home is on: waterType (great_lakes, inland, access, other), the water body
// and the private frontage in feet when the description states it. Zillow's
// structured facts rarely carry any of this; the agent's prose does.
//
// Rules, in order:
//
//  1. Find the water bodies the text names: Lake Michigan and its bays
//     (great_lakes), inland lakes (a known list plus any "<Name> Lake" or
//     "Lake <Name>"), rivers/creeks/ponds (other). Road and place names
//     ("Long Lake Rd", "Torch Lake Township", "the village of Suttons Bay")
//     are not water.
//  2. Find the claims. Strong private: a stated frontage length ("150 ft of
//     frontage", "125 feet on Torch Lake"), "private frontage/beach/dock",
//     "your own beach", or "frontage" not qualified as shared. Weak private:
//     "on Torch Lake", "lakefront home", "on the water". Access: shared /
//     deeded / association / community frontage or access, "lake access",
//     "access to the lake", "walk to the beach", "steps from the bay",
//     "across the street from", "near Torch Lake".
//  3. Any strong private claim wins. A weak private claim wins unless the
//     same sentence carries an unattached access claim or the body itself is
//     reached by access ("shared frontage on West Bay" is access). Otherwise
//     any access claim makes it access.
//  4. The type is access, else the kind of the body the home is on. When the
//     text only mentions a body as a view ("views of Grand Traverse Bay") and
//     claims neither frontage nor access, the type is other with that body:
//     a view is not frontage, and calling it great_lakes would put a view
//     home into the frontage comps. With no claim at all but a named body
//     (not a view), the body's kind is used: Zillow already tagged the home
//     waterfront and the text names the water. Private frontage on water we
//     cannot name is other.
//  5. frontageFt is only set for private frontage, and only between 10 and
//     5 000 ft.
package water

import (
	"regexp"
	"sort"
	"strconv"
	"strings"
)

// Water types (CONTRACT.md).
const (
	GreatLakes = "great_lakes"
	Inland     = "inland"
	Access     = "access"
	Other      = "other"
)

// Facts is what a description says about the water.
type Facts struct {
	Type       string
	Body       string // "" when not named
	FrontageFt *int
}

type kind int

const (
	kindGreat kind = iota
	kindInland
	kindOther
)

func (k kind) waterType() string {
	switch k {
	case kindGreat:
		return GreatLakes
	case kindInland:
		return Inland
	}
	return Other
}

type pattern struct {
	re   *regexp.Regexp
	name string
	kind kind
}

func ci(expr string) *regexp.Regexp { return regexp.MustCompile(`(?i)\b(?:` + expr + `)\b`) }

// Named bodies, most specific first. Matching is case-insensitive.
var named = []pattern{
	// Lake Michigan and its bays.
	{ci(`west(?:ern)?\s+(?:arm\s+of\s+(?:the\s+)?)?grand\s+traverse\s+bay`), "West Grand Traverse Bay", kindGreat},
	{ci(`east(?:ern)?\s+(?:arm\s+of\s+(?:the\s+)?)?grand\s+traverse\s+bay`), "East Grand Traverse Bay", kindGreat},
	{ci(`west\s+bay`), "West Grand Traverse Bay", kindGreat},
	{ci(`east\s+bay`), "East Grand Traverse Bay", kindGreat},
	{ci(`little\s+traverse\s+bay`), "Little Traverse Bay", kindGreat},
	{ci(`grand\s+traverse\s+bay|traverse\s+bay`), "Grand Traverse Bay", kindGreat},
	{ci(`suttons?\s+bay`), "Suttons Bay", kindGreat},
	{ci(`northport\s+bay`), "Northport Bay", kindGreat},
	{ci(`omena\s+bay`), "Omena Bay", kindGreat},
	{ci(`bowers\s+harbor`), "Bowers Harbor", kindGreat},
	{ci(`good\s+harbor\s+bay`), "Good Harbor Bay", kindGreat},
	{ci(`sleeping\s+bear\s+bay`), "Sleeping Bear Bay", kindGreat},
	{ci(`platte\s+bay`), "Platte Bay", kindGreat},
	{ci(`sturgeon\s+bay`), "Sturgeon Bay", kindGreat},
	{ci(`cathead\s+bay`), "Cathead Bay", kindGreat},
	{ci(`lake\s+michigan|the\s+big\s+lake`), "Lake Michigan", kindGreat},

	// Inland lakes of the six counties (and the canonical spelling of a few).
	{ci(`lake\s+charlevoix`), "Lake Charlevoix", kindInland}, // inland, despite the town
	{ci(`(?:north\s+|south\s+)?lake\s+leelanau`), "Lake Leelanau", kindInland},
	{ci(`little\s+glen\s+lake`), "Little Glen Lake", kindInland},
	{ci(`(?:big\s+)?glen\s+lake`), "Glen Lake", kindInland},
	{ci(`upper\s+herring\s+lake`), "Upper Herring Lake", kindInland},
	{ci(`lower\s+herring\s+lake`), "Lower Herring Lake", kindInland},
	{ci(`herring\s+lake`), "Herring Lake", kindInland},
	{ci(`(?:big\s+)?platte\s+lake`), "Platte Lake", kindInland},
	{ci(`little\s+platte\s+lake`), "Little Platte Lake", kindInland},
	{ci(`six\s+mile\s+lake`), "Six Mile Lake", kindInland},
	{ci(`lake\s+skegemog|skegemog\s+lake`), "Lake Skegemog", kindInland},
	{ci(`lake\s+bellaire`), "Lake Bellaire", kindInland},
	{ci(`lake\s+ann`), "Lake Ann", kindInland},
	{ci(`lake\s+dubonnet`), "Lake Dubonnet", kindInland},
	{ci(`torch\s+lake`), "Torch Lake", kindInland},
	{ci(`elk\s+lake`), "Elk Lake", kindInland},
	{ci(`crystal\s+lake`), "Crystal Lake", kindInland},
	{ci(`walloon\s+lake`), "Walloon Lake", kindInland},
	{ci(`burt\s+lake`), "Burt Lake", kindInland},
	{ci(`crooked\s+lake`), "Crooked Lake", kindInland},
	{ci(`pickerel\s+lake`), "Pickerel Lake", kindInland},
	{ci(`intermediate\s+lake`), "Intermediate Lake", kindInland},
	{ci(`clam\s+lake`), "Clam Lake", kindInland},
	{ci(`long\s+lake`), "Long Lake", kindInland},
	{ci(`silver\s+lake`), "Silver Lake", kindInland},
	{ci(`spider\s+lake`), "Spider Lake", kindInland},
	{ci(`duck\s+lake`), "Duck Lake", kindInland},
	{ci(`green\s+lake`), "Green Lake", kindInland},
	{ci(`arbutus\s+lake`), "Arbutus Lake", kindInland},
	{ci(`paradise\s+lake`), "Paradise Lake", kindInland},
	{ci(`thayer\s+lake`), "Thayer Lake", kindInland},
	{ci(`bass\s+lake`), "Bass Lake", kindInland},
	{ci(`lime\s+lake`), "Lime Lake", kindInland},
	{ci(`round\s+lake`), "Round Lake", kindInland},
	{ci(`boardman\s+lake`), "Boardman Lake", kindInland},
	{ci(`fife\s+lake`), "Fife Lake", kindInland},
	{ci(`island\s+lake`), "Island Lake", kindInland},
	{ci(`cedar\s+lake`), "Cedar Lake", kindInland},
	{ci(`mullett?\s+lake`), "Mullett Lake", kindInland},
	{ci(`lake\s+(?:of\s+the\s+woods)`), "Lake of the Woods", kindInland},

	// Rivers and the like.
	{ci(`boardman\s+river`), "Boardman River", kindOther},
	{ci(`betsie\s+river`), "Betsie River", kindOther},
	{ci(`platte\s+river`), "Platte River", kindOther},
	{ci(`jordan\s+river`), "Jordan River", kindOther},
	{ci(`torch\s+river`), "Torch River", kindOther},
	{ci(`elk\s+river`), "Elk River", kindOther},
	{ci(`rapid\s+river`), "Rapid River", kindOther},
	{ci(`bear\s+river`), "Bear River", kindOther},
	{ci(`crystal\s+river`), "Crystal River", kindOther},
	{ci(`leland\s+river`), "Leland River", kindOther},
}

// Generic names. Case-sensitive: a capitalised name next to "Lake".
var (
	genericLakeBefore = regexp.MustCompile(`\b((?:(?:Upper|Lower|Big|Little|North|South|East|West|Middle)\s+)?[A-Z][a-z'.]+(?:\s+Mile)?)\s+Lake\b`)
	genericLakeAfter  = regexp.MustCompile(`\bLake\s+([A-Z][a-z]+)\b`)
	genericRiver      = regexp.MustCompile(`\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+)?)\s+(River|Creek|Pond)\b`)
	plainOther        = ci(`river|creek|pond|stream`)
)

var nameStop = map[string]bool{}

func init() {
	for _, w := range strings.Fields(`the this that these our your its their a an on of to at in and or with from by for
		private beautiful gorgeous stunning pristine sandy inland sports all great spring fed quiet peaceful serene
		clear small front fresh lovely charming amazing spectacular nearby area local popular famous same main best
		big upper lower north south east west middle little sparkling crystal-clear deep shallow nice wonderful
		enjoy welcome private own shared deeded association community view views overlooking stunning gorgeous
		michigan superior huron erie ontario access shore shores life living home house cottage frontage property
		lot road rd drive dr street st township twp privileges rights bottom side sunsets sunset fun activities days
		is has was are be water`) {
		nameStop[w] = true
	}
}

// Words after a name that make it a road or a place, not water.
var placeAfter = regexp.MustCompile(`(?i)^(?:\s+shore)?\s+(?:rd|road|dr|drive|st|street|ave|avenue|hwy|highway|blvd|boulevard|ln|lane|ct|court|trail|trl|way|pkwy|parkway|cir|circle|township|twp|schools?|village|golf|estates|terrace|place|pl|hills|heights|woods|commons|plaza|condominiums?|elementary|middle school|high school|public|area schools|state park|resort)\b`)

// Before a town-like name: "in Suttons Bay", "village of Lake Leelanau".
var townBefore = regexp.MustCompile(`(?i)(?:\bin|\bdowntown|\bvillage\s+of|\btown\s+of|\bcity\s+of|\btownship\s+of)\s+$`)
var townAfter = regexp.MustCompile(`(?i)^,?\s+(?:mi|michigan)\b`)

// Names that are also towns.
var townish = map[string]bool{"Suttons Bay": true, "Lake Leelanau": true, "Lake Ann": true, "Glen Lake": false}

type mention struct {
	start, end int
	name       string
	kind       kind
	ctx        context
}

type context int

const (
	ctxNeutral context = iota
	ctxView
	ctxAccess
	ctxOn
)

// Contexts read from the words just before (or after) a body's name.
var (
	accessQual = regexp.MustCompile(`\b(?:shared|deeded|association|community|common|neighborhood|subdivision|easement|access)\b`)
	viewBefore = regexp.MustCompile(`\b(?:views?|viewing|overlook(?:s|ing)?|glimpses?|vistas?|sunsets?|panoram(?:a|ic))\s+(?:of|over|across|on|to|toward|towards)?\s*(?:the\s+)?(?:[a-z'-]+\s+){0,2}$`)
	nearBefore = regexp.MustCompile(`\b(?:near|nearby|close\s+to|minutes?\s+(?:from|to)|mins?\s+(?:from|to)|short\s+(?:walk|drive|bike\s+ride)\s+(?:from|to)|walk(?:ing)?\s+(?:distance\s+)?(?:to|from)|steps\s+(?:from|to|away\s+from)|blocks?\s+(?:from|to)|across\s+(?:the\s+)?(?:street|road)\s+from|access\s+(?:to|on)|rights\s+(?:to|on)|privileges\s+(?:to|on)|launch\s+(?:on|to|into))\s+(?:the\s+)?(?:[a-z'-]+\s+){0,2}$`)
	onBefore   = regexp.MustCompile(`(?:\bon|\balong|\bfronting|\bshores?\s+of|\bbanks?\s+of|\bedge\s+of|\binto)\s+(?:the\s+)?(?:[a-z'-]+\s+){0,3}$`)
	accessNext = regexp.MustCompile(`(?i)^\s+(?:access|privileges|rights|beach\s+access|boat\s+launch)\b`)
	frontNext  = regexp.MustCompile(`(?i)^\s*(?:-\s*)?(?:frontage|front(?:age)?\b|shoreline|waterfront)`)
)

// Claims anywhere in the text.
var (
	strongPrivateRes = []*regexp.Regexp{
		ci(`private\s+(?:(?:sandy|water|lake|bay|natural|hard[- ]bottom|level|all[- ]sports)\s+)*(?:frontage|beach|shoreline|waterfront|dock|shore)(?:\s+(?:access|rights|privileges))?`),
		ci(`your\s+own\s+(?:private\s+)?(?:sandy\s+)?(?:beach|dock|frontage|shoreline|waterfront|piece\s+of\s+(?:the\s+)?(?:lake|bay|shore))`),
		ci(`riparian`),
	}
	frontageWord   = ci(`frontage`)
	weakPrivateRes = []*regexp.Regexp{
		ci(`(?:lake|bay|water|river)\s?front(?:age)?(?:\s+(?:home|house|cottage|property|retreat|estate|living|lot|condo|getaway|parcel|setting|location|paradise))?`),
		ci(`on\s+the\s+water|right\s+on\s+the\s+(?:water|lake|bay|shore|beach)|directly\s+on|water'?s\s+edge`),
	}
	accessRes = []*regexp.Regexp{
		ci(`(?:shared|deeded|association|community|common|neighborhood|subdivision)\s+(?:[a-z'-]+\s+){0,2}?(?:access|frontage|beach|dock|docks|waterfront|park|shoreline|boat\s+slips?|slips?|launch|easement)`),
		ci(`private\s+(?:[a-z'-]+\s+){0,2}?access`),
		ci(`(?:lake|water|beach|bay|boat|river|waterfront)\s+(?:access|privileges|rights)`),
		ci(`access\s+to\s+(?:[a-z'-]+\s+){0,4}?(?:lake|bay|beach|water|shore|river|frontage)`),
		ci(`walk(?:ing)?\s+(?:distance\s+)?to\s+(?:the\s+)?(?:[a-z'-]+\s+){0,2}?(?:beach|lake|bay|water|shore)`),
		ci(`steps\s+(?:from|to|away\s+from)\s+(?:the\s+)?(?:[a-z'-]+\s+){0,2}?(?:beach|lake|bay|water|shore)`),
		ci(`across\s+the\s+(?:street|road)\s+from`),
		ci(`beach\s+rights|lake\s+privileges`),
	}
	frontageRoad   = regexp.MustCompile(`(?i)^\s+(?:road|rd)\b`)
	notPrivateNext = regexp.MustCompile(`^\s*(?:access|community|association|park|subdivision|development|neighborhood|privileges|rights)\b`)
	onAfter        = regexp.MustCompile(`^\s+(?:on|along|of)\s+(?:the\s+)?`)
)

// Frontage lengths.
const (
	num   = `(\d{1,3}(?:,\d{3})+|\d+)(?:\.\d+)?`
	feet  = `\s*(?:\+/?-?\s*)?(?:-|\s)?(?:(?:linear|lineal)\s+)?(?:feet|foot|ft\.?|')`
	about = `(?:approximately|approx\.?|about|over|nearly|roughly|almost|just\s+over|more\s+than|\+/-|~)?\s*`
)

var (
	// "150 ft of frontage", "100' of private sandy frontage", "200 feet of private water frontage".
	ftOfFrontage = regexp.MustCompile(`(?i)\b` + num + feet + `\s*(?:\+/-\s*)?(?:of\s+)?((?:[a-z'-]+\s+){0,4}?)(?:frontage|shoreline|shore\s+line|waterfront(?:age)?|lake\s?front(?:age)?|bay\s?front(?:age)?|water\s?front(?:age)?|beach\s?front(?:age)?|(?:sandy\s+)?beach|on\s+the\s+water|of\s+water)\b`)
	// "125 feet on Torch Lake", "90 ft along West Bay".
	ftOnBody = regexp.MustCompile(`(?i)\b` + num + feet + `\s+(?:of\s+(?:[a-z'-]+\s+){0,3}?)?(?:on|along|fronting|of)\s+(?:the\s+)?`)
	// "frontage: 90 ft", "frontage of approximately 150 feet", "shoreline is 75'".
	frontageIs = regexp.MustCompile(`(?i)\b(?:frontage|shoreline)\s*(?::|-|of|is|measures|totals|with|=)?\s*` + about + num + feet)
)

// Classify reads a description.
func Classify(desc string) Facts {
	text := normalize(desc)
	if strings.TrimSpace(text) == "" {
		return Facts{Type: Other}
	}
	lower := asciiLower(text)
	sents := sentenceStarts(text)
	sentOf := func(pos int) int { return sort.SearchInts(sents, pos+1) - 1 }

	ms := findMentions(text, lower)
	for i := range ms {
		ms[i].ctx = mentionContext(lower, ms[i], sents)
	}

	// Claims.
	type claim struct {
		pos  int
		body int // index into ms, -1 when not attached to a body
	}
	var strong, weak, access []claim

	frontFt, frontBody, frontPos := frontage(lower, ms)
	if frontFt != nil {
		strong = append(strong, claim{frontPos, frontBody})
	}
	for _, re := range strongPrivateRes {
		for _, loc := range re.FindAllStringIndex(lower, -1) {
			m := lower[loc[0]:loc[1]]
			if strings.HasSuffix(m, "access") || strings.HasSuffix(m, "rights") || strings.HasSuffix(m, "privileges") {
				continue // "private beach access" is access, found below
			}
			strong = append(strong, claim{loc[0], -1})
		}
	}
	for _, loc := range frontageWord.FindAllStringIndex(lower, -1) {
		if frontageRoad.MatchString(lower[loc[1]:]) {
			continue
		}
		win := window(lower, loc[0], 40, sents)
		if accessQual.MatchString(win) {
			continue
		}
		strong = append(strong, claim{loc[0], -1})
	}
	for _, re := range weakPrivateRes {
		for _, loc := range re.FindAllStringIndex(lower, -1) {
			after := lower[loc[1]:]
			if notPrivateNext.MatchString(after) {
				continue
			}
			if accessQual.MatchString(window(lower, loc[0], 25, sents)) {
				continue
			}
			weak = append(weak, claim{loc[0], -1})
		}
	}
	for _, re := range accessRes {
		for _, loc := range re.FindAllStringIndex(lower, -1) {
			access = append(access, claim{loc[0], -1})
		}
	}
	for i, m := range ms {
		switch m.ctx {
		case ctxOn:
			weak = append(weak, claim{m.start, i})
		case ctxAccess:
			access = append(access, claim{m.start, i})
		}
	}

	// Weak private claims fall to an access claim in the same sentence,
	// unless that access claim is about another named body.
	var keptWeak []claim
	for _, w := range weak {
		cancelled := false
		for _, a := range access {
			if sentOf(a.pos) != sentOf(w.pos) {
				continue
			}
			if a.body >= 0 && w.body >= 0 && ms[a.body].name != ms[w.body].name {
				continue
			}
			cancelled = true
			break
		}
		if !cancelled {
			keptWeak = append(keptWeak, w)
		}
	}

	pick := func(pred func(m mention) bool) int {
		for i, m := range ms {
			if pred(m) {
				return i
			}
		}
		return -1
	}
	nearest := func(pos int) int {
		best, bestD := -1, 1<<30
		for i, m := range ms {
			if m.ctx == ctxView || m.ctx == ctxAccess || sentOf(m.start) != sentOf(pos) {
				continue
			}
			d := m.start - pos
			if d < 0 {
				d = -d
			}
			if d < bestD {
				best, bestD = i, d
			}
		}
		return best
	}

	private := len(strong) > 0 || len(keptWeak) > 0
	var body = -1
	var f Facts
	switch {
	case private:
		body = frontBody
		if body < 0 {
			body = pick(func(m mention) bool { return m.ctx == ctxOn })
		}
		if body < 0 {
			for _, c := range append(append([]claim{}, strong...), keptWeak...) {
				if c.body >= 0 {
					body = c.body
					break
				}
				if b := nearest(c.pos); b >= 0 {
					body = b
					break
				}
			}
		}
		if body < 0 {
			body = pick(func(m mention) bool { return m.ctx == ctxNeutral })
		}
		f.FrontageFt = frontFt
		if body >= 0 {
			f.Type = ms[body].kind.waterType()
		} else {
			f.Type = Other
		}
	case len(access) > 0:
		f.Type = Access
		body = pick(func(m mention) bool { return m.ctx == ctxAccess })
		if body < 0 {
			body = pick(func(m mention) bool { return m.ctx != ctxView })
		}
		if body < 0 {
			body = pick(func(mention) bool { return true })
		}
	default:
		body = pick(func(m mention) bool { return m.ctx != ctxView })
		if body >= 0 {
			f.Type = ms[body].kind.waterType()
		} else {
			f.Type = Other
			body = pick(func(mention) bool { return true })
		}
	}
	if body >= 0 {
		f.Body = ms[body].name
	}
	return f
}

// findMentions lists the water bodies named in text, without overlaps
// (earlier patterns win), in text order.
func findMentions(text, lower string) []mention {
	var ms []mention
	overlaps := func(s, e int) bool {
		for _, m := range ms {
			if s < m.end && e > m.start {
				return true
			}
		}
		return false
	}
	add := func(s, e int, name string, k kind) {
		if overlaps(s, e) || isPlace(text, s, e, name) {
			return
		}
		ms = append(ms, mention{start: s, end: e, name: name, kind: k})
	}
	for _, p := range named {
		for _, loc := range p.re.FindAllStringIndex(lower, -1) {
			add(loc[0], loc[1], p.name, p.kind)
		}
	}
	gtext := text
	if mostlyUpper(text) {
		gtext = titleCase(text)
	}
	for _, sm := range genericLakeBefore.FindAllStringSubmatchIndex(gtext, -1) {
		words := strings.Fields(gtext[sm[2]:sm[3]])
		if nameStop[asciiLower(words[len(words)-1])] && !(len(words) == 2 && words[1] == "Mile") {
			continue
		}
		add(sm[0], sm[1], strings.Join(words, " ")+" Lake", kindInland)
	}
	for _, sm := range genericLakeAfter.FindAllStringSubmatchIndex(gtext, -1) {
		w := gtext[sm[2]:sm[3]]
		if nameStop[asciiLower(w)] {
			continue
		}
		add(sm[0], sm[1], "Lake "+w, kindInland)
	}
	for _, sm := range genericRiver.FindAllStringSubmatchIndex(gtext, -1) {
		words := strings.Fields(gtext[sm[2]:sm[3]])
		for len(words) > 0 && nameStop[asciiLower(words[0])] {
			words = words[1:]
		}
		if len(words) == 0 {
			add(sm[4], sm[5], "", kindOther)
			continue
		}
		add(sm[2]+strings.Index(gtext[sm[2]:sm[3]], words[0]), sm[1], strings.Join(words, " ")+" "+gtext[sm[4]:sm[5]], kindOther)
	}
	for _, loc := range plainOther.FindAllStringIndex(lower, -1) {
		add(loc[0], loc[1], "", kindOther)
	}
	sort.Slice(ms, func(i, j int) bool { return ms[i].start < ms[j].start })
	return ms
}

func isPlace(text string, s, e int, name string) bool {
	if placeAfter.MatchString(text[e:]) {
		return true
	}
	if townish[name] && (townBefore.MatchString(text[:s]) || townAfter.MatchString(text[e:])) {
		return true
	}
	return false
}

func mentionContext(lower string, m mention, sents []int) context {
	after := lower[m.end:]
	if accessNext.MatchString(after) {
		return ctxAccess
	}
	win := window(lower, m.start, 45, sents)
	if frontNext.MatchString(after) {
		// "Torch Lake frontage"; "shared West Bay frontage" is access.
		if accessQual.MatchString(win) {
			return ctxAccess
		}
		return ctxOn
	}
	switch {
	case viewBefore.MatchString(win):
		return ctxView
	case nearBefore.MatchString(win):
		return ctxAccess
	case onBefore.MatchString(win):
		// "shared frontage on West Bay", "deeded access on Torch Lake".
		if accessQual.MatchString(win) {
			return ctxAccess
		}
		return ctxOn
	}
	return ctxNeutral
}

// frontage finds the first stated private frontage length. It also returns
// the body named right after it ("125 feet on Torch Lake") and where it was.
func frontage(lower string, ms []mention) (*int, int, int) {
	type hit struct {
		pos, ft, body int
	}
	var hits []hit
	parse := func(s string) (int, bool) {
		n, err := strconv.Atoi(strings.ReplaceAll(s, ",", ""))
		return n, err == nil && n >= 10 && n <= 5000
	}
	bodyAt := func(pos int) int {
		for i, m := range ms {
			if m.start >= pos && m.start-pos <= 40 && m.ctx != ctxView {
				return i
			}
		}
		return -1
	}
	for _, sm := range ftOfFrontage.FindAllStringSubmatchIndex(lower, -1) {
		qual := lower[sm[4]:sm[5]]
		if accessQual.MatchString(qual) {
			continue
		}
		if n, ok := parse(lower[sm[2]:sm[3]]); ok {
			b := -1
			if on := onAfter.FindString(lower[sm[1]:]); on != "" {
				b = bodyAt(sm[1] + len(on))
			}
			hits = append(hits, hit{sm[0], n, b})
		}
	}
	for _, sm := range ftOnBody.FindAllStringSubmatchIndex(lower, -1) {
		b := bodyAt(sm[1])
		if b < 0 || ms[b].start != sm[1] {
			continue
		}
		if accessQual.MatchString(lower[sm[0]:sm[1]]) {
			continue
		}
		if n, ok := parse(lower[sm[2]:sm[3]]); ok {
			hits = append(hits, hit{sm[0], n, b})
		}
	}
	for _, sm := range frontageIs.FindAllStringSubmatchIndex(lower, -1) {
		if accessQual.MatchString(window(lower, sm[0], 30, nil)) {
			continue
		}
		if n, ok := parse(lower[sm[2]:sm[3]]); ok {
			hits = append(hits, hit{sm[0], n, -1})
		}
	}
	if len(hits) == 0 {
		return nil, -1, -1
	}
	sort.Slice(hits, func(i, j int) bool { return hits[i].pos < hits[j].pos })
	h := hits[0]
	ft := h.ft
	return &ft, h.body, h.pos
}

// window returns up to n bytes of lower before pos, not crossing the start
// of pos's sentence.
func window(lower string, pos, n int, sents []int) string {
	start := max(0, pos-n)
	if sents != nil {
		i := sort.SearchInts(sents, pos+1) - 1
		if i >= 0 && sents[i] > start {
			start = sents[i]
		}
	}
	return lower[start:pos]
}

var sentenceEnd = regexp.MustCompile(`[.!?;]+\s+|\n+|\s+[-–—•|]\s+`)
var abbrev = regexp.MustCompile(`(?i)\b(?:ft|approx|st|dr|rd|mr|mrs|ms|no|sq|ave|mt|twp|hwy|ln|ct|blvd|apx|appx)$`)

// sentenceStarts returns the byte offsets where sentences start (always 0 first).
func sentenceStarts(text string) []int {
	out := []int{0}
	for _, loc := range sentenceEnd.FindAllStringIndex(text, -1) {
		if text[loc[0]] == '.' && abbrev.MatchString(text[:loc[0]]) {
			continue
		}
		out = append(out, loc[1])
	}
	return out
}

func normalize(s string) string {
	r := strings.NewReplacer("’", "'", "‘", "'", "“", `"`, "”", `"`, "″", `"`, "′", "'", " ", " ", "&#39;", "'", "&amp;", "&", "&quot;", `"`)
	return r.Replace(s)
}

// asciiLower lowercases ASCII letters only, so byte offsets stay valid.
func asciiLower(s string) string {
	b := []byte(s)
	for i, c := range b {
		if c >= 'A' && c <= 'Z' {
			b[i] = c + 32
		}
	}
	return string(b)
}

func mostlyUpper(s string) bool {
	up, low := 0, 0
	for i := 0; i < len(s); i++ {
		switch c := s[i]; {
		case c >= 'A' && c <= 'Z':
			up++
		case c >= 'a' && c <= 'z':
			low++
		}
	}
	return up > 20 && up > 3*low
}

// titleCase turns "TORCH LAKE" into "Torch Lake" (ASCII, same length).
func titleCase(s string) string {
	b := []byte(asciiLower(s))
	prevLetter := false
	for i, c := range b {
		isLetter := c >= 'a' && c <= 'z'
		if isLetter && !prevLetter {
			b[i] = c - 32
		}
		prevLetter = isLetter || c == '\''
	}
	return string(b)
}
