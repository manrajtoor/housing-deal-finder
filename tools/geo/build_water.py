#!/usr/bin/env python3
"""Builds crawler/internal/geo/water.json.gz from OpenStreetMap.

One-off data prep (stdlib only, Python 3.9+). Two Overpass queries for the
six northern Michigan counties (Grand Traverse, Leelanau, Antrim, Benzie,
Charlevoix, Emmet):

  1. The Great Lakes shore. OSM maps the Great Lakes as natural=water
     multipolygon relations, not natural=coastline (that is oceans only), so
     this takes the member ways inside the bbox of the "Lake Michigan" and
     "Lake Huron" relations (outer = mainland shore incl. Grand Traverse
     Bay, Little Traverse Bay, Suttons Bay, ...; inner = island shores).
  2. named natural=water areas (ways and multipolygon relations) and named
     natural=bay features. Kept as inland lakes: water=lake / reservoir, or
     any named water whose name says "Lake" (legacy tagging, "X Lake"
     ponds), never rivers/canals, at least MIN_AREA_HA. Lake Michigan and
     Lake Huron themselves are dropped here: query 1 covers them.

Geometry is simplified with Douglas-Peucker (TOLERANCE_M) in a local metric
projection and coordinates are stored as integers of 1e-5 degree (~1 m).

Usage:
  python3 tools/geo/build_water.py [--cache DIR] [--out PATH]

Raw Overpass responses are cached in --cache (default tools/geo/.cache,
git-ignored) so a rebuild with different filters does not hit the API.

Data © OpenStreetMap contributors, ODbL 1.0 (https://www.openstreetmap.org/copyright).
"""

import argparse
import gzip
import json
import math
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

OVERPASS = "https://overpass-api.de/api/interpreter"
# south, west, north, east
BBOX = (44.4, -86.4, 45.9, -84.6)
TOLERANCE_M = 10.0
MIN_AREA_HA = 10.0
SCALE = 100000  # 1e-5 degree units
ATTRIBUTION = "© OpenStreetMap contributors, ODbL 1.0 (https://www.openstreetmap.org/copyright)"

# Local equirectangular projection around the bbox centre: metres per degree.
LAT0 = (BBOX[0] + BBOX[2]) / 2
M_PER_DEG_LAT = 111132.0
M_PER_DEG_LON = 111320.0 * math.cos(math.radians(LAT0))

GREAT_LAKES = {"Lake Michigan", "Lake Huron", "Lake Superior"}


def bbox_str():
    return "%s,%s,%s,%s" % BBOX


# Each lake: its relation (tags only, as a marker) then its member ways in the
# bbox, so the output order says which lake a way belongs to.
QUERY_COAST = """[out:json][timeout:300];
relation["natural"="water"]["name"="Lake Michigan"];
out tags;
way(r)(%(b)s);
out geom;
relation["natural"="water"]["name"="Lake Huron"];
out tags;
way(r)(%(b)s);
out geom;""" % {"b": bbox_str()}

QUERY_WATER = """[out:json][timeout:300];
(
  way["natural"="water"]["name"](%(b)s);
  relation["natural"="water"]["name"]["name"!="Lake Michigan"]["name"!="Lake Huron"](%(b)s);
  way["natural"="bay"]["name"](%(b)s);
  relation["natural"="bay"]["name"](%(b)s);
  node["natural"="bay"]["name"](%(b)s);
);
out geom;""" % {"b": bbox_str()}


def fetch(name, query, cache):
    path = os.path.join(cache, name + ".json")
    if os.path.exists(path):
        with open(path) as f:
            return json.load(f)
    os.makedirs(cache, exist_ok=True)
    body = urllib.parse.urlencode({"data": query}).encode()
    req = urllib.request.Request(
        OVERPASS,
        data=body,
        headers={"User-Agent": "housedeals-geo-prep/1 (one-off data build)"},
    )
    raw = None
    for attempt in range(4):
        print("overpass: %s (try %d) ..." % (name, attempt + 1), file=sys.stderr)
        t = time.time()
        try:
            with urllib.request.urlopen(req, timeout=600) as r:
                raw = r.read()
            break
        except urllib.error.HTTPError as err:
            # 429/504: the public server is busy; wait and retry, politely.
            if err.code not in (429, 502, 503, 504) or attempt == 3:
                raise
            time.sleep(60 * (attempt + 1))
    print("overpass: %s %d bytes in %.0fs" % (name, len(raw), time.time() - t), file=sys.stderr)
    with open(path, "wb") as f:
        f.write(raw)
    return json.loads(raw)


def xy(p):
    return (p[1] * M_PER_DEG_LON, p[0] * M_PER_DEG_LAT)


def dp(points, tol):
    """Douglas-Peucker on [(lat, lon)], iterative; keeps both ends."""
    n = len(points)
    if n < 3:
        return points
    pts = [xy(p) for p in points]
    keep = [False] * n
    keep[0] = keep[-1] = True
    stack = [(0, n - 1)]
    while stack:
        a, b = stack.pop()
        ax, ay = pts[a]
        bx, by = pts[b]
        dx, dy = bx - ax, by - ay
        l2 = dx * dx + dy * dy
        best, idx = -1.0, -1
        for i in range(a + 1, b):
            px, py = pts[i]
            if l2 == 0:
                d = math.hypot(px - ax, py - ay)
            else:
                t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / l2))
                d = math.hypot(px - (ax + t * dx), py - (ay + t * dy))
            if d > best:
                best, idx = d, i
        if best > tol:
            keep[idx] = True
            stack.append((a, idx))
            stack.append((idx, b))
    return [p for p, k in zip(points, keep) if k]


def simplify_ring(ring):
    """Simplifies a closed ring (first == last) without collapsing it."""
    if len(ring) < 4:
        return ring
    # Split at the point farthest from the start so DP has two open halves.
    p0 = xy(ring[0])
    far = max(range(len(ring)), key=lambda i: math.dist(p0, xy(ring[i])))
    a = dp(ring[: far + 1], TOLERANCE_M)
    b = dp(ring[far:], TOLERANCE_M)
    out = a[:-1] + b
    if len(out) < 4:
        return ring
    return out


def ring_area_m2(ring):
    s = 0.0
    for i in range(len(ring) - 1):
        x1, y1 = xy(ring[i])
        x2, y2 = xy(ring[i + 1])
        s += x1 * y2 - x2 * y1
    return abs(s) / 2


def geom(way_geometry):
    return [(g["lat"], g["lon"]) for g in way_geometry if g is not None]


def stitch(segments):
    """Joins open way geometries end to end into closed rings."""
    segs = [list(s) for s in segments if len(s) >= 2]
    rings = []
    while segs:
        cur = segs.pop()
        changed = True
        while cur[0] != cur[-1] and changed:
            changed = False
            for i, s in enumerate(segs):
                if s[0] == cur[-1]:
                    cur += s[1:]
                elif s[-1] == cur[-1]:
                    cur += s[::-1][1:]
                elif s[-1] == cur[0]:
                    cur = s + cur[1:]
                elif s[0] == cur[0]:
                    cur = s[::-1] + cur[1:]
                else:
                    continue
                segs.pop(i)
                changed = True
                break
        if cur[0] == cur[-1] and len(cur) >= 4:
            rings.append(cur)
    return rings


def chain(ways):
    """Joins open ways that share end points into longer polylines."""
    segs = [list(w) for w in ways if len(w) >= 2]
    out = []
    while segs:
        cur = segs.pop()
        changed = True
        while changed and cur[0] != cur[-1]:
            changed = False
            for i, s in enumerate(segs):
                if s[0] == cur[-1]:
                    cur = cur + s[1:]
                elif s[-1] == cur[0]:
                    cur = s + cur[1:]
                elif s[-1] == cur[-1]:
                    cur = cur + s[::-1][1:]
                elif s[0] == cur[0]:
                    cur = s[::-1] + cur[1:]
                else:
                    continue
                segs.pop(i)
                changed = True
                break
        out.append(cur)
    return out


def flat(points):
    out = []
    for lat, lon in points:
        out.append(round(lat * SCALE))
        out.append(round(lon * SCALE))
    return out


def is_lake(tags):
    name = tags.get("name", "")
    if name in GREAT_LAKES:
        return False
    w = tags.get("water")
    if w in ("river", "canal", "stream", "ditch", "wastewater", "lock", "moat", "fishpond"):
        return False
    if w in ("lake", "reservoir"):
        return True
    return "Lake" in name.split()


def lake_name(name):
    """Cleans OSM lake names the way listings write them."""
    name = name.split(" / ")[0].strip()  # "Green Lake / Lake Wahbekanetta"
    words = name.split()
    name = " ".join(w[:1].upper() + w[1:] if w.islower() else w for w in words)  # "Intermediate lake"
    if name == "Big Glen Lake":  # crawler/internal/water calls it Glen Lake
        name = "Glen Lake"
    return name


def bay_name(name):
    """"West Arm Grand Traverse Bay" -> "West Grand Traverse Bay" (as listings say)."""
    return name.replace(" Arm ", " ")


def point_in_rings(lat, lon, rings):
    inside = False
    for r in rings:
        for i in range(len(r) - 1):
            (y1, x1), (y2, x2) = r[i], r[i + 1]
            if (y1 > lat) != (y2 > lat):
                if lon < x1 + (lat - y1) * (x2 - x1) / (y2 - y1):
                    inside = not inside
    return inside


def dist_to_lines(lat, lon, lines):
    px, py = xy((lat, lon))
    best = float("inf")
    for line in lines:
        for i in range(len(line) - 1):
            ax, ay = xy(line[i])
            bx, by = xy(line[i + 1])
            dx, dy = bx - ax, by - ay
            l2 = dx * dx + dy * dy
            t = 0.0 if l2 == 0 else max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / l2))
            best = min(best, math.hypot(px - (ax + t * dx), py - (ay + t * dy)))
    return best


def build(cache, out):
    coast = fetch("coast", QUERY_COAST, cache)
    water = fetch("water", QUERY_WATER, cache)

    # Coastline: chain ways by shared end nodes, then simplify.
    by_lake = {}
    lake = None
    for e in coast["elements"]:
        if e["type"] == "relation":
            lake = e.get("tags", {}).get("name")
        elif e["type"] == "way" and "geometry" in e and lake:
            by_lake.setdefault(lake, []).append(geom(e["geometry"]))
    coast_out = []
    for gl_name, ways in sorted(by_lake.items()):
        for c in chain(ways):
            coast_out.append({"n": gl_name, "p": flat(dp(c, TOLERANCE_M))})

    coast_lines = [c for lake_ways in by_lake.values() for c in chain(lake_ways)]
    lakes, bays, bay_points = [], [], []
    lake_rings = []  # unsimplified, for the bay-point filter
    bay_nodes = []
    skipped_small = 0
    for e in water["elements"]:
        tags = e.get("tags", {})
        name = tags.get("name")
        if not name:
            continue
        nat = tags.get("natural")
        if e["type"] == "node":
            if nat == "bay":
                bay_nodes.append((name, e["lat"], e["lon"]))
            continue
        if e["type"] == "way":
            g = geom(e.get("geometry", []))
            if len(g) < 4 or g[0] != g[-1]:
                rings_outer, rings_inner = [], []
            else:
                rings_outer, rings_inner = [g], []
        else:
            outer = [geom(m["geometry"]) for m in e.get("members", [])
                     if m["type"] == "way" and m.get("role", "outer") in ("outer", "") and "geometry" in m]
            inner = [geom(m["geometry"]) for m in e.get("members", [])
                     if m["type"] == "way" and m.get("role") == "inner" and "geometry" in m]
            rings_outer, rings_inner = stitch(outer), stitch(inner)
        if not rings_outer:
            continue
        area_ha = (sum(map(ring_area_m2, rings_outer)) - sum(map(ring_area_m2, rings_inner))) / 1e4
        rings = [flat(simplify_ring(r)) for r in rings_outer + rings_inner]
        if nat == "bay":
            bays.append({"n": bay_name(name), "r": rings, "ha": round(area_ha)})
        elif nat == "water" and is_lake(tags):
            if area_ha < MIN_AREA_HA:
                skipped_small += 1
                continue
            lakes.append({"n": lake_name(name), "r": rings, "ha": round(area_ha)})
            lake_rings.append(rings_outer + rings_inner)

    # Bay labels (nodes) name a stretch of Great Lakes shore. Keep only those
    # in Great Lakes water: not inside an inland lake ("Horton Bay" and
    # "South Arm" are on Lake Charlevoix) and nearer the Great Lakes shore
    # than any inland lake shore.
    dropped_bays = []
    for name, lat, lon in bay_nodes:
        near = [r for r in lake_rings if any(
            min(p[0] for p in ring) - 0.05 < lat < max(p[0] for p in ring) + 0.05 and
            min(p[1] for p in ring) - 0.07 < lon < max(p[1] for p in ring) + 0.07 for ring in r)]
        in_lake = any(point_in_rings(lat, lon, r) for r in near)
        d_lake = min((dist_to_lines(lat, lon, r) for r in near), default=float("inf"))
        d_coast = dist_to_lines(lat, lon, coast_lines)
        if in_lake or d_lake < d_coast or name.startswith("Lake "):
            dropped_bays.append(name)
            continue
        bay_points.append({"n": name, "p": flat([(lat, lon)])})
    print("bay labels dropped (inland): %s" % ", ".join(sorted(dropped_bays)), file=sys.stderr)

    lakes.sort(key=lambda l: (-l["ha"], l["n"]))
    bays.sort(key=lambda b: b["ha"])
    bay_points.sort(key=lambda b: b["n"])
    doc = {
        "attribution": ATTRIBUTION,
        "source": "Overpass API (%s), built by tools/geo/build_water.py" % OVERPASS,
        "built": time.strftime("%Y-%m-%d", time.gmtime()),
        "bbox": list(BBOX),
        "scale": SCALE,
        "toleranceM": TOLERANCE_M,
        "coast": coast_out,
        "lakes": lakes,
        "bays": bays,
        "bayPoints": bay_points,
    }
    raw = json.dumps(doc, separators=(",", ":"), ensure_ascii=False).encode()
    with gzip.GzipFile(out, "wb", mtime=0) as f:
        f.write(raw)
    pts = sum(len(c["p"]) for c in coast_out) // 2
    print("coast: %d chains, %d points" % (len(coast_out), pts), file=sys.stderr)
    print("lakes: %d (skipped %d under %g ha)" % (len(lakes), skipped_small, MIN_AREA_HA), file=sys.stderr)
    print("bays: %d polygons, %d points" % (len(bays), len(bay_points)), file=sys.stderr)
    print("%s: %d bytes (%d raw)" % (out, os.path.getsize(out), len(raw)), file=sys.stderr)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(os.path.dirname(here))
    ap = argparse.ArgumentParser()
    ap.add_argument("--cache", default=os.path.join(here, ".cache"))
    ap.add_argument("--out", default=os.path.join(root, "crawler", "internal", "geo", "water.json.gz"))
    a = ap.parse_args()
    build(a.cache, a.out)


if __name__ == "__main__":
    main()
