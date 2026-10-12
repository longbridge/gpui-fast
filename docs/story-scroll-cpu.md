# Story scroll CPU against gpui-pre

How much CPU the GPUI Kit story gallery spends while scrolling each story,
built on gpui-pre and on gpui-fast. Every story was scrolled with the same
synthetic wheel input under both builds, and the process CPU time was read
while it scrolled.

## Setup

- GPUI Kit `1e7ffaa6` (`crates/story`), release profile. The gpui-pre
  build uses `gpui-pre 0.3.9`; the gpui-fast build adds
  `--features gpui-fast`, which selects `gpui-fast 0.1.7` from crates.io.
- Linux on Hyprland 0.56.2, 24 cores, one 3840x2160 60 Hz display at scale 1.5,
  wgpu renderer on Wayland.
- Each story opens with `gpui-component-story <Story>` as an 875x600 floating
  window centered on the current workspace, and the pointer is placed over the
  story content (62% across, 60% down the window).
- A uinput virtual mouse sends `REL_WHEEL_HI_RES` steps of 30 (a quarter
  notch) at 250 Hz for 15 s, reversing direction every 2.5 s so the content
  keeps moving instead of resting at an end.
- CPU is the process utime + stime from `/proc/<pid>/stat`, over 3 s idle
  before scrolling and over the 15 s of scrolling; 100% is one core.

## Results

On the 76 stories that scrolled, gpui-fast spent less CPU than gpui-pre on 75
of them. Mean scroll CPU fell from 13.8% to 8.7% (the sum from 1052 to 662,
-37%), and the median story fell 34%. The one story that rose, Introduction
(+6%), is within noise.

| Story | pre idle % | pre scroll % | fast idle % | fast scroll % | fast vs pre |
|---|--:|--:|--:|--:|--:|
| Input Group | 47.3 | 48.5 | 21.3 | 26.0 | -46% |
| VirtualList | 0.7 | 32.2 | 0.7 | 19.1 | -41% |
| Shell | 12.0 | 28.2 | 7.3 | 14.2 | -50% |
| Message | 0.3 | 27.2 | 0.7 | 7.7 | -72% |
| Input | 0.7 | 25.3 | 0.7 | 17.3 | -32% |
| Questionnaire | 0.7 | 22.1 | 0.7 | 14.1 | -36% |
| Tabs ¹ | 0.3 | 22.1 | 0.3 | 8.1 | -63% |
| Button | 0.7 | 20.6 | 0.3 | 8.7 | -58% |
| Calendar | 0.7 | 19.0 | 0.7 | 10.7 | -44% |
| MessageScroller | 0.3 | 18.7 | 0.3 | 13.4 | -28% |
| DataTable | 0.7 | 18.1 | 0.3 | 13.1 | -28% |
| Empty | 0.3 | 17.7 | 0.7 | 10.0 | -44% |
| Marker | 16.0 | 17.3 | 6.3 | 9.4 | -46% |
| Bubble | 0.3 | 16.7 | 0.7 | 7.0 | -58% |
| Theme Colors | 0.3 | 16.4 | 0.3 | 11.5 | -30% |
| Attachment | 16.3 | 16.2 | 8.7 | 11.4 | -30% |
| Textarea ¹ | 0.3 | 16.2 | 0.3 | 12.6 | -22% |
| Combobox | 0.0 | 15.7 | 0.3 | 10.3 | -34% |
| Notification | 0.7 | 15.2 | 0.7 | 7.9 | -48% |
| Shimmer | 13.7 | 14.7 | 5.7 | 10.1 | -31% |
| Alert | 0.7 | 14.3 | 0.7 | 7.2 | -50% |
| Diff | 2.3 | 14.2 | 0.3 | 10.5 | -26% |
| Collapsible | 0.7 | 14.0 | 0.7 | 7.0 | -50% |
| Command | 0.7 | 13.5 | 0.7 | 10.1 | -25% |
| Slider | 0.3 | 13.4 | 0.7 | 7.3 | -46% |
| Select | 0.7 | 13.3 | 0.7 | 10.3 | -23% |
| Sidebar | 0.3 | 13.3 | 0.7 | 6.7 | -50% |
| Form | 0.3 | 13.2 | 0.3 | 8.9 | -33% |
| Table | 0.3 | 13.2 | 0.7 | 4.1 | -69% |
| Pagination | 0.3 | 13.1 | 0.3 | 7.4 | -44% |
| StatusBar | 0.7 | 12.9 | 0.3 | 7.1 | -45% |
| Dialog | 0.7 | 12.6 | 0.7 | 9.2 | -27% |
| Chart | 0.7 | 12.5 | 0.7 | 10.6 | -15% |
| Tooltip | 0.7 | 12.5 | 0.7 | 6.5 | -48% |
| GroupBox | 0.7 | 12.4 | 0.7 | 6.9 | -44% |
| AlertDialog | 0.7 | 12.1 | 0.7 | 7.0 | -42% |
| Tag ¹ | 0.7 | 12.1 | 0.7 | 6.8 | -44% |
| DatePicker | 0.7 | 12.0 | 0.7 | 7.0 | -42% |
| OtpInput | 0.3 | 11.8 | 0.7 | 7.2 | -39% |
| Settings | 0.7 | 11.7 | 0.3 | 9.1 | -22% |
| Stepper ¹ | 0.7 | 11.6 | 0.7 | 6.9 | -41% |
| List | 0.7 | 11.4 | 0.3 | 9.7 | -15% |
| Checkbox | 0.0 | 11.3 | 0.3 | 6.8 | -40% |
| NumberInput | 0.7 | 11.3 | 0.7 | 8.7 | -23% |
| DescriptionList | 0.7 | 11.1 | 0.3 | 7.5 | -32% |
| Switch | 0.7 | 11.1 | 0.7 | 6.8 | -39% |
| Editor | 0.7 | 11.0 | 0.3 | 9.8 | -11% |
| HoverCard ¹ | 0.7 | 11.0 | 0.3 | 7.1 | -35% |
| Icon | 10.0 | 11.0 | 4.7 | 8.4 | -24% |
| Progress | 0.3 | 11.0 | 0.7 | 6.9 | -37% |
| Toolbar | 0.7 | 11.0 | 0.0 | 9.1 | -17% |
| Avatar | 0.7 | 10.9 | 0.3 | 6.8 | -38% |
| Badge | 0.7 | 10.9 | 1.0 | 6.6 | -39% |
| Dock | 0.7 | 10.9 | 0.7 | 7.8 | -28% |
| TimeField | 0.7 | 10.9 | 0.7 | 6.9 | -37% |
| Accordion | 0.7 | 10.6 | 0.7 | 6.7 | -37% |
| Label | 0.7 | 10.6 | 0.3 | 8.0 | -25% |
| Tree | 0.7 | 10.4 | 0.7 | 8.5 | -18% |
| Separator | 0.3 | 10.2 | 0.7 | 7.0 | -31% |
| DropdownButton | 0.7 | 10.1 | 0.7 | 6.6 | -35% |
| Radio | 0.7 | 10.0 | 0.3 | 6.7 | -33% |
| Introduction | 0.7 | 9.9 | 0.7 | 10.5 | +6% |
| Menu | 0.3 | 9.9 | 0.7 | 6.7 | -32% |
| Spinner | 10.0 | 9.9 | 4.0 | 4.0 | -60% |
| Toggle | 0.7 | 9.9 | 0.3 | 6.5 | -34% |
| Sheet | 0.7 | 9.8 | 0.3 | 8.0 | -18% |
| ColorPicker | 0.3 | 9.5 | 0.3 | 6.5 | -32% |
| Rating | 0.3 | 9.3 | 0.7 | 6.6 | -29% |
| NativeMenu | 0.7 | 9.2 | 0.7 | 6.7 | -27% |
| Speech | 0.0 | 9.1 | 0.7 | 7.1 | -22% |
| Breadcrumb | 0.7 | 8.8 | 0.7 | 6.7 | -24% |
| Kbd | 0.7 | 8.8 | 0.7 | 6.2 | -30% |
| Clipboard | 0.7 | 8.7 | 0.7 | 7.5 | -14% |
| Skeleton | 8.0 | 8.5 | 3.0 | 6.7 | -21% |
| Scrollbar | 0.7 | 8.4 | 0.3 | 7.3 | -13% |
| Image | 0.7 | 7.7 | 0.7 | 6.3 | -18% |

¹ Rerun: the first pass was disturbed by desktop activity (focus/pointer taken by another window).

### No scrolling at the cursor (scroll CPU < 5% in both builds, reproduced twice)

The wheel at the cursor position does not scroll anything in these stories, so they say nothing about scroll cost.

| Story | pre idle % | pre scroll % | fast idle % | fast scroll % |
|---|--:|--:|--:|--:|
| Carousel | 0.7 | 3.9 | 1.0 | 4.5 |
| Popover | 0.7 | 2.3 | 0.7 | 3.2 |
| Resizable | 0.7 | 1.5 | 0.3 | 2.8 |

## Notes

- One pass per story/build. CPU is read in 10 ms ticks (CLK_TCK=100); with desktop noise, differences under ~2 points are within noise.
- Stories with non-trivial idle CPU (Input Group, Marker, Attachment, Shimmer, Shell, Icon, Spinner, Skeleton) are animating; their scroll figure includes that baseline.
- `Theme Colors` (pre) exited during scrolling once with no core dump; the rerun completed and is the value shown.
- The driver script is below; it writes `results.json` and skips entries already measured, retrying ones that errored.

## Driver

Run it with the two binaries in one directory as `story-pre` and
`story-fast`, from a Hyprland session where the user can write `/dev/uinput`.
It needs `python-evdev`, and it takes over the pointer while it runs.

```sh
python3 bench.py <bin-dir> results.json "Accordion|Alert|..."
```

<details>
<summary>bench.py</summary>

```python
#!/usr/bin/env python3
"""Scroll each story under both builds and record the process CPU time.

For every story: launch as an Omarchy floating window (875x600) on the current workspace, settle, measure
idle CPU, then send continuous hi-res wheel events over the story content
(down then up) and measure CPU while scrolling.
"""
import json, os, signal, subprocess, sys, time
from evdev import UInput, ecodes as e

BIN = sys.argv[1]
OUT = sys.argv[2]
STORIES = sys.argv[3].split("|")
VARIANTS = ["pre", "fast"]
W, H = 875, 600
SETTLE, IDLE, SCROLL = 4.0, 3.0, 15.0
FLIP = 2.5          # seconds before reversing direction
RATE = 250          # wheel events per second
STEP = 30           # hi-res units per event (120 = one notch)
TICK = os.sysconf("SC_CLK_TCK")


def hypr(*args):
    return subprocess.run(["hyprctl", *args], capture_output=True, text=True).stdout


def cpu_ticks(pid):
    with open(f"/proc/{pid}/stat") as f:
        parts = f.read().rsplit(")", 1)[1].split()
    return int(parts[11]) + int(parts[12])  # utime + stime, all threads


def find_window(pid, timeout=15):
    end = time.time() + timeout
    while time.time() < end:
        for c in json.loads(hypr("clients", "-j")):
            if c["pid"] == pid and c["mapped"]:
                return c
        time.sleep(0.2)
    return None


def scroll(ui, seconds):
    n = int(seconds * RATE)
    flip = int(FLIP * RATE)
    start = time.monotonic()
    acc = 0
    for i in range(n):
        d = -STEP if (i // flip) % 2 == 0 else STEP
        ui.write(e.EV_REL, e.REL_WHEEL_HI_RES, d)
        acc += d
        if abs(acc) >= 120:
            notch = 1 if acc > 0 else -1
            ui.write(e.EV_REL, e.REL_WHEEL, notch)
            acc -= notch * 120
        ui.syn()
        delay = start + (i + 1) / RATE - time.monotonic()
        if delay > 0:
            time.sleep(delay)


def run_one(ui, variant, story):
    proc = subprocess.Popen([f"{BIN}/story-{variant}", story],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        win = find_window(proc.pid)
        if not win:
            return {"error": "no window"}
        # Omarchy floating window: float, 875x600, centered.
        for _ in range(25):
            hypr("dispatch", f'hl.dsp.focus({{ window = "pid:{proc.pid}" }})')
            if json.loads(hypr("activewindow", "-j") or "{}").get("pid") == proc.pid:
                break
            time.sleep(0.2)
        else:
            return {"error": "window not focused"}
        hypr("dispatch", 'hl.dsp.window.float({ action = "enable" })')
        hypr("dispatch", f"hl.dsp.window.resize({{ x = {W}, y = {H} }})")
        hypr("dispatch", "hl.dsp.window.center()")
        time.sleep(0.3)
        win = find_window(proc.pid)
        x, y = win["at"]
        w, h = win["size"]
        # Story content sits right of the sidebar.
        hypr("dispatch", f"hl.dsp.cursor.move({{ x = {x + int(w * 0.62)}, y = {y + int(h * 0.6)} }})")
        time.sleep(SETTLE)
        if proc.poll() is not None:
            return {"error": "exited"}
        t0, c0 = time.monotonic(), cpu_ticks(proc.pid)
        time.sleep(IDLE)
        t1, c1 = time.monotonic(), cpu_ticks(proc.pid)
        scroll(ui, SCROLL)
        t2, c2 = time.monotonic(), cpu_ticks(proc.pid)
        if proc.poll() is not None:
            return {"error": "exited during scroll"}
        idle = (c1 - c0) / TICK / (t1 - t0) * 100
        scr = (c2 - c1) / TICK / (t2 - t1) * 100
        return {"idle_cpu": round(idle, 1), "scroll_cpu": round(scr, 1),
                "scroll_cpu_sec": round((c2 - c1) / TICK, 2)}
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def main():
    caps = {e.EV_KEY: [e.BTN_LEFT, e.BTN_RIGHT, e.BTN_MIDDLE],
            e.EV_REL: [e.REL_X, e.REL_Y, e.REL_WHEEL, e.REL_WHEEL_HI_RES]}
    ui = UInput(caps, name="gpui-bench-mouse")
    time.sleep(1.5)
    results = {}
    if os.path.exists(OUT):
        results = json.load(open(OUT))
    try:
        for story in STORIES:
            for v in VARIANTS:
                key = f"{story}::{v}"
                if key in results and "error" not in results[key]:
                    continue
                r = run_one(ui, v, story)
                results[key] = r
                print(story, v, r, flush=True)
                json.dump(results, open(OUT, "w"), indent=1)
                time.sleep(1)
    finally:
        ui.close()


main()
```

</details>
