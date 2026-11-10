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
# A break is a low of 88 µs or more. A sampler stop can make it look longer.
breaks = [(a, b) for a, b in lows if b - a >= 88]
def fall_near(t, tol=1.5):
    return next((a for a in falls if abs(a - t) <= tol), None)
prev = None
for i, (ba, bb) in enumerate(breaks):
    nxt = breaks[i + 1][0] if i + 1 < len(breaks) else end
    sc = next((a for a in falls if a >= bb), None)
    if sc is None or sc >= nxt: continue
    slots, errs, unk, idle, t = [], 0, 0, 0.0, sc
    while t + 44 <= nxt and t + 44 <= end:
        if uncertain(t - 1, t + 45):
            unk += 1; slots.append(None)
        else:
            v = sum(level(t + BIT * (k + 1.5)) << k for k in range(8))
            if level(t + BIT * 0.5) != 0 or not (level(t + BIT * 9.5) and level(t + BIT * 10.5)): errs += 1
            slots.append(v)
        # The next start bit follows the two stop bits, or comes later after idle time.
        t2 = fall_near(t + 44)
        if t2 is None:
            if uncertain(t + 40, t + 48): t2 = t + 44
            else:
                t2 = next((a for a in falls if a > t + 43), None)
                if t2 is None or t2 >= nxt: break
                idle = max(idle, t2 - (t + 44))
        t = t2
    period = f"{(ba - prev)/1000:6.2f}" if prev is not None else "     -"
    prev = ba
    whole = breaks[i + 1:] != []
    print(f"frame at {ba/1000:6.2f} ms  period {period} ms  break {bb-ba:6.1f} µs  MAB {sc-bb:7.1f} µs  "
          f"slots {len(slots):3d}{'' if whole else '+'}  unknown {unk:2d}  framing err {errs}  "
          f"max idle between slots {idle:5.1f} µs  SC,ch1-8 {slots[:9]}")
