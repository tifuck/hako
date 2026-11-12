# Branding

The app is called **Hako** ("HAH-koh", Japanese for box) and Pip the penguin
is its mascot, logo and icon. This folder holds Pip as separate layers, a
script that stacks them into finished images, and everything needed to redraw
the whole set from nothing.

```
branding/
├── README.md        this file: how to use it, how to extend it, the full design spec
├── NAMES.md         app-name candidates and what was checked
├── gallery.html     every version on one page (generated; open it in a browser)
├── logo/            generated: the logo with no background (see Logo below)
├── parts/           what you edit
│   ├── body.svg     the penguin itself; change it and every output changes
│   ├── wordmark.svg the "hako" lettering, hand-drawn strokes, no font needed
│   ├── outline.svg  light edge put behind Pip on dark surfaces
│   ├── glyph.svg    one-colour Pip
│   ├── pose/        feet and flippers: stand, wave, cheer
│   ├── face/        default, wink, happy, sleepy, love
│   ├── wear/        hats, headset, scarf, ... (name.back.svg is drawn behind the body)
│   └── scene/       things floating in the background: snow, stars, confetti, hearts
├── presets.toml     named combinations of the above
├── build.py         stacks parts into out/ and logo/, writes gallery.html
└── out/             generated SVG + 512 px PNG per preset; committed so nobody has to build
```

## Build

Needs Python 3.11+ and nothing else. PNG export uses Chrome or Chromium if installed.

```bash
./build.py --png 512          # rebuild everything: out/, logo/, gallery.html
./build.py                    # same without PNGs
./build.py winter christmas   # only these presets

# one-offs: start from a preset, override what you like
./build.py base --bg '#0EA5E9' --shape circle -o /tmp/pip-blue.svg
./build.py winter --bg none -o /tmp/pip-winter-cutout.svg
./build.py base --face wink --wear tuft,shades --scene stars --bg '#111318' -o /tmp/pip.svg --png 1024
```

`--bg` is `none` or a `#rrggbb` colour. `--shape` is `rounded`, `circle` or `square`.

## Logo

All four are flat SVG on a transparent background, cropped tight.

| File | Use |
|---|---|
| `logo/hako-mark.svg` | Pip alone, full colour. Light surfaces. |
| `logo/hako-logo.svg` | Pip and the wordmark in navy. Light surfaces. |
| `logo/hako-logo-dark.svg` | Same with a light edge around Pip and a light wordmark. Dark or coloured surfaces. |
| `logo/hako-glyph.svg` | One colour, drawn in `currentColor`: inline it and it takes the surrounding text colour; as a plain image it is black. |

Pip's body is dark navy, so on a dark surface use the `-dark` logo, the glyph,
or an icon with a background (`out/pip-icon.svg`).

## In the app

- **Top bar:** `widgets::pip` in `libs/ui/src/widgets.rs` draws Pip with the
  painter on a tile filled with the theme's accent colour, so it follows the
  theme. It repeats the base geometry from `parts/`; change both together.
- **App icon:** `client/assets/icon.png` is the yellow icon, because a file
  cannot follow the theme. Regenerate it with
  `./build.py icon -o /tmp/icon.svg --png 256 && cp /tmp/icon.png ../client/assets/icon.png`.

## Add an event

1. Open `out/pip-base-plain.svg` (Pip with nothing on) in Inkscape or Figma and
   draw the new item on top of it.
2. Save only the new shapes as `parts/wear/<name>.svg`. Shapes that must sit
   behind the head (a headband, ears) go in `parts/wear/<name>.back.svg`.
3. Add a `[events.<name>]` table to `presets.toml` and run `./build.py --png 512`.

Rules for a part file:

- Same canvas as the others: `viewBox="0 0 120 120"`, no `width` / `height`.
- Do not redraw or move the penguin; a part holds only what it adds.
- Parts are merged into one file, so any `id` (gradients, clips) must start
  with the part's name, e.g. `scarf-rgb`.

## Design spec

Enough to redraw Pip from scratch. The files in `parts/` are the reference; if
they and this text ever disagree, the files win.

### Who Pip is

A small, round penguin: the Linux nod, since the app is Linux-only. Friendly,
a little scruffy, never cool on purpose. The one fixed quirk is a three-strand
cowlick that will not lie flat. Each event version adds at most one more idea
(a hat, a prop, an expression).

### Style

- Flat fills only. No outlines, no gradients (the RGB scarf is the one
  exception), no drop shadows on the character.
- Everything is built from circles, ellipses and rounded shapes. Stroked details
  use round caps and round joins.
- Big head, tiny limbs: the body is a single egg shape and the eyes sit below
  its middle, close to the beak. That placement is most of the cuteness.
- It has to read at 20 px. If a prop is not recognisable at that size, make it
  bigger and simpler rather than more detailed.
- A version adds at most three colours to the base palette.

### Canvas and layers

120 × 120 units, origin top left. Layers bottom to top: background, scene,
ground shadow, pose, wear `.back`, body, face, wear.

### Palette

| Use | Colour |
|---|---|
| Body, flippers, tuft | `#1E293B` |
| Face and belly | `#FFFFFF` |
| Beak and feet | `#FFA726` |
| Eyes | `#1F1B2E` |
| Blush | `#FF7A9A` at 50 % |
| Accent red (hats, bow tie, tag) | `#E5484D`, soft `#FF6B6B` |
| Cream (fur trim, bobble) | `#FFF1E6` |
| Accent yellow (headset, pom) | `#FFD23F` |
| Accent violet | `#7C83F5` |
| Accent teal / pink | `#5EEAD4` / `#FF7AA2` |
| Icon backgrounds | yellow `#FFC94D` (default), coral `#FF7A6B`, sky `#A8DEFF`, mint `#B9F3D0`, violet `#C9BEFF`, night `#1E1B4B` |

### Geometry

| Piece | Shape |
|---|---|
| Body | ellipse, centre (60, 67), radii 31 × 34 |
| Head sheen | white ellipse 7 × 3.2 at (42, 45), rotated −38°, 14 % opacity |
| Face | two white circles r 13 at (49, 60) and (71, 60) |
| Belly | white ellipse, centre (60, 77), radii 21 × 22, overlapping the face circles |
| Eyes | ellipses 5 × 6 at (49, 61) and (71, 61) |
| Eye shine | white circle r 1.9 up and right of centre (+1.7, −2.2); r 0.9 at 80 % down and left (−1.6, +2.3) |
| Blush | ellipses 4.4 × 2.6 at (45.5, 70) and (74.5, 70) |
| Beak | downward triangle (55.5, 66.5) → (64.5, 66.5) → (60, 72), same-colour 2.5 stroke with round joins |
| Feet | ellipses 9 × 4.5 at (48, 100) and (72, 100), behind the body |
| Flippers, down | ellipses 6.5 × 14 at (29, 72) rotated 18° and (91, 72) rotated −18°, behind the body |
| Flipper, raised | ellipse 6.5 × 13.5 at (96, 54) rotated 38° (mirror at (24, 54), −38°) |
| Tuft | three strokes from (60, 36) to (52, 26), (65, 23), (71, 29); width 3.6, round caps, behind the body |

Faces other than the default change only the eyes: closed eyes are arcs 10
wide, stroke 2.6 (`happy` curves up, `sleepy` curves down, `wink` closes the
right eye), and `love` swaps both eyes for `#FF4D6D` hearts about 10 wide.

Hats sit on a brim that spans x 28–92 around y 42–54, just above the face
circles. Headwear replaces the tuft (leave `"tuft"` out of `wear`).

### Backgrounds

- Shape: rounded square with corner radius 30 (25 %), a circle, or a plain square.
- Halo: white circle r 46 at (60, 62), 25 % opacity on light colours and 20 % on
  dark ones (dark means luminance below 110 of 255), so the dark body never
  disappears.
- Ground shadow: black ellipse 27 × 4 at (60, 104), 18 % opacity.
- A transparent version has no halo and no shadow.
- Scene pieces (snow, stars, confetti, hearts) stay in the ring between the
  penguin and the edge, at least 5 units inside it, so the circle crop is safe.

### Type

Headings on the gallery page are Fredoka 600 (Google Fonts). Use it for a
wordmark too: the rounded terminals match Pip's shapes.

### How this was made

Every shape was written by hand as SVG, rendered with headless Chrome and
checked at 300 px, as a 44 px circle and at 20 px before it was kept.
`build.py` is the only tooling: it strips the `<svg>` wrapper from each part
and concatenates them in layer order.
