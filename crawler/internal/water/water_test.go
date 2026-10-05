package water

import (
	"fmt"
	"testing"
)

func TestClassify(t *testing.T) {
	cases := []struct {
		name, desc string
		typ, body  string
		ft         int // 0 = nil
	}{
		// Private frontage with a stated length.
		{"ft of frontage on", "Enjoy 150 ft of frontage on Torch Lake.", Inland, "Torch Lake", 150},
		{"apostrophe feet, adjectives", "Offering 100' of private sandy frontage on Elk Lake and a new dock.", Inland, "Elk Lake", 100},
		{"feet on body", "This cottage sits on 125 feet on Torch Lake with a hard sandy bottom.", Inland, "Torch Lake", 125},
		{"frontage colon", "Frontage: 90 ft on North Lake Leelanau. Two docks.", Inland, "Lake Leelanau", 90},
		{"Lake Charlevoix is inland", "200 feet of private frontage on Lake Charlevoix with a permanent dock.", Inland, "Lake Charlevoix", 200},
		{"Lake Michigan shoreline", "Lake Michigan shoreline home with 120 feet of sandy beach.", GreatLakes, "Lake Michigan", 120},
		{"thousands separator", "Rare 1,200 ft of frontage on Lake Michigan.", GreatLakes, "Lake Michigan", 1200},
		{"East Bay", "Welcome to East Bay! 80 ft of frontage and a sugar sand beach.", GreatLakes, "East Grand Traverse Bay", 80},
		{"Suttons Bay as water", "Frontage on Suttons Bay with 75 ft of sandy beach.", GreatLakes, "Suttons Bay", 75},
		{"apostrophe beach", "Glen Arbor home on Lake Michigan with 150' of sandy beach.", GreatLakes, "Lake Michigan", 150},
		{"all caps", "TORCH LAKE FRONTAGE - 100 FT OF SUGAR SAND. NEW DOCK.", Inland, "Torch Lake", 100},
		{"curly apostrophe", "Boasting 110’ of frontage on Crystal Lake.", Inland, "Crystal Lake", 110},
		{"approximately", "The frontage of approximately 150 feet faces west on Glen Lake.", Inland, "Glen Lake", 150},
		{"live fixture text", "Welcome to your much-anticipated Northern Michigan Retreat! Enjoy this 200 feet of private water frontage on nearly 1.5 wooded acres on Island Lake. This tucked-away private walk-out retreat offers over 2,300 finished square feet with four bedrooms. Association fee $250 annually.", Inland, "Island Lake", 200},
		{"generic lake name", "Quiet home with 60 ft of frontage on Hanley Lake.", Inland, "Hanley Lake", 60},
		{"two-word generic", "Sits on 300 feet of Upper Herring Lake shoreline.", Inland, "Upper Herring Lake", 300},

		// Private frontage without a length.
		{"shores of", "Classic cottage on the shores of Little Traverse Bay.", GreatLakes, "Little Traverse Bay", 0},
		{"lakefront on", "Lakefront home on Skegemog Lake with 2,300 square feet of living space.", Inland, "Lake Skegemog", 0},
		{"waterfront home on", "Waterfront home on Green Lake.", Inland, "Green Lake", 0},
		{"big glen", "Sandy beach on Big Glen Lake, two docks.", Inland, "Glen Lake", 0},
		{"private beach", "Private beach on West Bay and a guest house.", GreatLakes, "West Grand Traverse Bay", 0},
		{"private wins over deeded access", "Private frontage on Silver Lake plus deeded access to Lake Michigan.", Inland, "Silver Lake", 0},
		{"on lake, minutes to another", "Home on Lake Leelanau, minutes to Lake Michigan beaches.", Inland, "Lake Leelanau", 0},
		{"view of one, frontage on another", "Views of West Bay and 100 feet of frontage on Spider Lake.", Inland, "Spider Lake", 100},
		{"town then lake", "In Charlevoix, on Round Lake with a boat slip.", Inland, "Round Lake", 0},
		{"road is not a lake", "Home on Long Lake Rd with 90 ft of frontage on Cedar Lake.", Inland, "Cedar Lake", 90},
		{"township is not a lake", "Torch Lake Township home with 100 ft on Clam Lake.", Inland, "Clam Lake", 100},
		{"river frontage", "Platte River frontage with a canoe launch.", Other, "Platte River", 0},
		{"river cottage", "Cottage on the Boardman River.", Other, "Boardman River", 0},
		{"private frontage unnamed", "120 ft of private frontage and a sandy beach.", Other, "", 120},

		// Access.
		{"shared frontage on West Bay", "Enjoy shared frontage on West Bay with your neighbors.", Access, "West Grand Traverse Bay", 0},
		{"deeded access", "Includes deeded access to Long Lake.", Access, "Long Lake", 0},
		{"shared frontage with length", "50 ft of shared frontage on Bass Lake.", Access, "Bass Lake", 0},
		{"association beach", "Association beach on Glen Lake, walk to the beach.", Access, "Glen Lake", 0},
		{"lake access to", "Lake access to Crystal Lake through the association park.", Access, "Crystal Lake", 0},
		{"named lake access", "Great Torch Lake access with a seasonal dock.", Access, "Torch Lake", 0},
		{"across the street", "Across the street from Lake Michigan with a deeded beach.", Access, "Lake Michigan", 0},
		{"community docks", "Walloon Lake association with 2 community docks.", Access, "Walloon Lake", 0},
		{"steps from", "Steps from the beach on Little Traverse Bay.", Access, "Little Traverse Bay", 0},
		{"Suttons Bay the town", "In the village of Suttons Bay, steps from the beach.", Access, "", 0},
		{"access with a long frontage claim", "Private lake access with 40 ft of shared frontage on Elk Lake.", Access, "Elk Lake", 0},

		// Neither frontage nor access.
		// A view is not frontage: "other" with the body, never great_lakes.
		{"views only", "Stunning views of Grand Traverse Bay from every room.", Other, "Grand Traverse Bay", 0},
		{"named only", "Torch Lake living at its finest.", Inland, "Torch Lake", 0},
		{"pond", "A spring-fed pond in the back yard.", Other, "", 0},
		{"nothing", "Three bedrooms, two baths, a big garage.", Other, "", 0},
		{"empty", "", Other, "", 0},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := Classify(c.desc)
			gotFt := 0
			if got.FrontageFt != nil {
				gotFt = *got.FrontageFt
			}
			if got.Type != c.typ || got.Body != c.body || gotFt != c.ft {
				t.Errorf("Classify(%q)\n got  %s / %q / %d\n want %s / %q / %d", c.desc, got.Type, got.Body, gotFt, c.typ, c.body, c.ft)
			}
		})
	}
}

func TestSentenceStartsSkipsAbbreviations(t *testing.T) {
	got := sentenceStarts("It has 100 ft. of frontage. Second one! Third")
	if fmt.Sprint(got) != "[0 28 40]" {
		t.Errorf("sentences = %v", got)
	}
}
