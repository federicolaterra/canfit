# Assets

Text captures of canfit's output and the README images, used by the documentation.

| File | What it shows | Source |
|---|---|---|
| `monitor-calm-80x24.txt` | Live monitor at 80×24, calm machine | render test on recorded and synthetic fixture data |
| `monitor-critical-80x24.txt` | Live monitor at 80×24 under critical pressure | render test on synthetic fixture data |
| `monitor-critical-140x44.txt` | The same at full screen, with the "Why?" panel beside the list | render test on synthetic fixture data |
| `monitor-explain-80x24.txt` | The `?` explanation panel | render test on synthetic fixture data |
| `monitor-close-dialog-80x24.txt` | The confirmation dialog shown by `x` | render test on synthetic fixture data |
| `fit-check-qwen2.5-coder-3b.txt` | `canfit model.gguf` (plain text, piped) | real run on an M1 Pro, 2026-11-09 |
| `canfit-logo.svg` | The `canfit` wordmark (class `canfit-logo`, 5rem monospace text; dark text, light under a dark colour scheme) | hand-written SVG, README header |
| `top-chip.svg` | The word `top` in white on an orange (`#EA580C`) rounded chip, for the tagline | hand-written SVG, README header |
| `canfit-github.png` | Screenshot of the live monitor (1280×839, dark theme), shown after the README's documentation table | real run on the maintainer's Mac, 2026-11-10 |
| `canfit-demo-video-github.png` | Cover image of the YouTube demo (2276×1280), linked from the README's "Watch the demo" section | provided by the maintainer, 2026-11-11 |

The monitor captures are regenerated from `src/ui/snapshots/` (the `insta` render snapshots) and
show fake apps, not a real machine. The README's animated GIF of the "doesn't fit → close Chrome →
now it fits" flow has not been recorded yet.
