# Gets GET /scope from the box and decodes the DMX frames on GPIO4.
# Usage: python3 -I scripts/dmx-decode.py [HOST] [CAPTURE_FILE]
import sys, urllib.request
host = sys.argv[1] if len(sys.argv) > 1 else "autoblink.local"
src = sys.argv[2] if len(sys.argv) > 2 else None
txt = open(src).read() if src else urllib.request.urlopen(f"http://{host}/scope", timeout=15).read().decode()
if not src: open("/tmp/scope-last.txt", "w").write(txt)
w = txt.split(); mhz, first, n = int(w[0]), int(w[1]), int(w[2]); ents = [int(x) for x in w[3:3 + n]]
BIT = 4.0
# Level runs: (start_us, end_us, level). Gaps: (start_us, end_us).
edges, gaps, gs = [], [], None
for e in ents:
    t, lv, stop = (e & ~3) / mhz, e & 1, (e >> 1) & 1
    if stop and gs is None: gs = t; continue
    if stop and gs is not None: gaps.append((gs, t)); gs = None
    edges.append((t, lv))
runs, cur, t0 = [], first, 0.0
for t, lv in edges:
    if lv != cur: runs.append((t0, t, cur)); cur, t0 = lv, t
end = SPAN = max([t for t, _ in edges] + [0])
runs.append((t0, end, cur))
def level(t):
    for a, b, lv in runs:
        if a <= t < b: return lv
    return runs[-1][2]
def uncertain(a, b): return any(g0 < b and g1 > a for g0, g1 in gaps)
falls = [a for a, b, lv in runs if lv == 0]
lows = [(a, b) for a, b, lv in runs if lv == 0]
print(f"capture {end/1000:.1f} ms, {len(ents)} entries, {len(gaps)} sampler stops (max {max([b-a for a,b in gaps]+[0]):.1f} µs)")
breaks = [(a, b) for a, b in lows if b - a >= 60]
prev = None
for i, (ba, bb) in enumerate(breaks):
    nxt = breaks[i + 1][0] if i + 1 < len(breaks) else None
    mab_end = next((a for a in falls if a >= bb), None)
    if mab_end is None: break
    slots, errs, unc, mtbs, t = [], 0, 0, 0.0, mab_end
    while t is not None and (nxt is None or t < nxt) and t + 44 <= end:
        if level(t + BIT * 0.5) != 0: errs += 1
        v = sum(level(t + BIT * (k + 1.5)) << k for k in range(8))
        if not (level(t + BIT * 9.5) and level(t + BIT * 10.5)): errs += 1
        if uncertain(t, t + 44): unc += 1
        slots.append(v)
        t2 = next((a for a in falls if a >= t + BIT * 9.6), None)
        if t2 is not None and (nxt is None or t2 < nxt): mtbs = max(mtbs, t2 - (t + 44))
        t = t2
    period = f"{(ba - prev)/1000:7.2f}" if prev is not None else "      -"
    prev = ba
    full = nxt is not None
    print(f"frame at {ba/1000:6.2f} ms  period {period} ms  break {bb-ba:6.1f} µs  MAB {mab_end-bb:7.1f} µs  "
          f"slots {len(slots):3d}{'' if full else '+'}  SC {slots[0] if slots else '-'}  ch1-8 {slots[1:9]}  "
          f"framing err {errs}  uncertain {unc}  max gap between slots {mtbs:.1f} µs")
