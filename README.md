# carasoul

A tiny always-on Windows companion that lets you flip through the wallpapers in
`wallpapers/` without leaving the desktop.

It lives in the **notification area** and never shows a window of its own: start
it once and it registers itself to start with Windows, sits in the tray, and
waits. Left-click the tray icon to open the shelf, or right-click it for the few
things a wallpaper app needs to be asked (next wallpaper, the folder, quitting).

Hold **Shift** on the homescreen and a shelf of parallelogram cards wells up out
of the bottom of the screen like liquid, swelling into shape. Glide through the
cards with the arrow keys (or the mouse), **let go of Shift, and whatever is under
the focus marker becomes your wallpaper** — the shelf pours away down the screen
onto the desktop as it lands. Windows' accent colour follows it, and the shelf's
own colours are pulled from the wallpaper in focus.

---

## Documentation rule (read this first)

**Any change to this app must be documented in this README.** That means, in the
same commit as the code change:

- new or changed behaviour → update **Controls** and **How it works**
- a new flag, folder, or file → update **Command line** / **Project layout**
- a change to the renderer, input, or animation approach → update the matching
  subsection so the *why* is recorded, not just the *what*
- any new measured number (frame cost, memory) → update **Performance**, and say
  how it was measured
- anything you had to fight Windows over → add it to **Gotchas and limitations**

If you skip this, the next person has to reverse-engineer your reasoning from
the diff. The sections below are written to make that unnecessary.

---

## Controls

| Input | Action |
| --- | --- |
| Hold **Shift** (~165 ms) **on the desktop** | Open the shelf |
| **←/→** or **↑/↓** | Rotate through wallpapers (wraps around endlessly) |
| Mouse move | Hover to move the focus marker |
| **Release Shift** | Apply the focused wallpaper (+ its accent colour) |
| Left-click a card | Apply it immediately |
| **Esc** or right-click | Cancel, applying nothing |
| **Enter** | Also applies (kept as an alias for Shift-release) |
| **Ctrl+Shift+Q** while open | Quit |
| **Left-click** the tray icon | Open the shelf (no Shift needed) |
| **Right-click** the tray icon | The tray menu, below |
| Launching the app again | Opens the shelf in the copy already running |

Tray menu:

| Item | Action |
| --- | --- |
| Open shelf | Same as left-clicking the icon |
| Next wallpaper | Rotate one step and apply it, with no shelf involved |
| Open wallpaper folder | Opens `wallpapers/` in Explorer, creating it if it is not there |
| Rescan wallpapers | Re-reads the folder, for when an image is added while the shelf is already open |
| Start with Windows | Ticked when the startup entry is in place; toggles it |
| Quit | Leaves, removing the tray icon |

Details that matter:

- The strip only opens on a Shift-hold when the **desktop itself** has focus, so
  holding Shift in any other app does nothing. That restriction does not apply to
  a shelf opened **from the tray** — that is an explicit request, wherever the
  focus happens to be.
- A Shift-hold only opens if **no other key** is pressed during the hold, so
  typing a capital letter never triggers it.
- While the shelf is open, `←/→/↑/↓/Enter/Esc` are **swallowed** so they cannot
  walk the desktop's icon selection or launch something.
- Releasing Shift commits. That is why there is no separate confirm step: the
  shelf pours away down the screen (**Animation**) and the wallpaper lands as it
  goes.
- A shelf opened from the tray has no Shift to release, so it is **pinned**: it
  waits for Enter or a click, and Esc (or a click on the shelf) still cancels.
  Choosing **Next wallpaper** while it is open just rotates the carousel, since
  that reads as browsing rather than as an instruction to land.
- The shelf opens on the display the **pointer** is on, not the one it was
  launched on, so on two monitors you can hold Shift on either of them. On a
  **portrait** monitor the cards are sized from the monitor's short side (its
  width), which keeps the shelf in proportion instead of stretching top to
  bottom.

## Project layout

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Layout maths, the liquid silhouettes, spring animations, the state machine, frame loop |
| `src/canvas.rs` | Software renderer: premultiplied BGRA into the layered-window bitmap, including the per-row span fill the liquid is drawn with |
| `src/sys.rs` | Win32: overlay window, input (polling + keyboard hook), registry, wallpaper, accent, console/instance plumbing |
| `src/library.rs` | Folder scan, background thumbnail/wash decoding, accent extraction, the two prepared copies |
| `src/tray.rs` | The notification-area icon: its menu, its balloon, and the messages that come back from the shell |
| `build.rs` | Writes the `.rc` for the icon + version info and compiles it with the SDK's `rc.exe` (see **Building**) |
| `assets/` | The icon export, plus the generated `carasoul.ico` |
| `tools/make_icon.py` | Regenerates `carasoul.ico` from the export (needs Pillow) |
| `wallpapers/` | The images. Drop new ones in and open the shelf; no restart needed |

## How it works

### Presentation

The shelf is a single `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE` popup
covering only the shelf band, painted by hand into a 32-bit DIB and pushed with
`UpdateLayeredWindow` using premultiplied alpha. It never takes focus, so it
cannot disturb whatever you were doing.

The bitmap is **allocated lazily** on the first frame actually drawn, which is
why idle memory is small: at boot the process holds no pixel buffers.

The one thing on screen is either the shelf's wash panel or — while the shelf is
arriving or pouring away — the liquid standing in for it. Both are built from the
same outline, so the shelf can morph without a visible hand-over; see
**Animation**.

### Startup and the notification area

The binary is linked as a **GUI app** (`windows_subsystem`), so starting with
Windows shows no console and no window: the process appears, adds a tray icon and
waits. The overlay window is created but never shown until the shelf opens, and it
is a `WS_EX_TOOLWINDOW`, so there is no taskbar button either.

The tray icon is the whole of the app's visible surface, which makes it the one
part that has to be right: `tray.rs` registers it with `Shell_NotifyIconW`
(version 4), builds the `HICON` from an embedded PNG at whatever size the shell
asks for, and re-adds it when Explorer restarts and takes the notification area
with it (`WM_TASKBARCREATED`). Clicks and menu picks arrive on the overlay
window's message proc, which does no work of its own: it records the request as a
bit in an atomic and the frame loop acts on it. That keeps every Win32 callback a
leaf, and keeps the tray and the animation loop from re-entering each other.

The icon is the export's **full-colour** master, not the white-on-transparent
`notification/mono-*` set: Android recolours notification icons for the theme, but
the Windows tray draws one as-is, and a white silhouette is invisible on a light
taskbar. The full-bleed square is also why the icon needs no corner rounding of
its own on Windows.

Three things follow from having no window to open:

- **The balloons are the UI.** A windowless app has to say things through the
  notification area, so the first run ever says one thing there — how to drive it,
  or where to put wallpapers if there are none yet — and then stays quiet.
- **The tooltip is the status.** It carries the wallpaper currently under the
  focus marker, which is the closest thing to "the current selection" that no
  window can otherwise show.
- **The command line borrows a console.** A GUI build has none to print to, so the
  modes that report something (`--list`, `--demo`, …) attach to the console they
  were launched from. Started from Explorer there is nothing to borrow, and
  nothing is printed.

Starting twice is harmless: a named mutex decides who is first, and a second
launch posts `WM_OPEN_SHELF` to the window of the instance already running — so
opening the app from the Start menu opens the shelf rather than starting a rival
copy.

### Input

Two mechanisms, deliberately:

1. **Polling** (`GetAsyncKeyState`) for Shift, the mouse buttons and Ctrl+Q.
   Shift must never be swallowed, and polling costs nothing when idle.
2. **A low-level keyboard hook** (`WH_KEYBOARD_LL`), installed *only while the
   shelf is open*, which swallows the navigation keys so they never reach any
   window.

**Gotcha worth knowing:** a key swallowed by the hook **never reaches
`GetAsyncKeyState`**, so polling alone cannot see it. The hook therefore records
the keypress into an atomic bitmask (`PENDING`) which `Input::poll` drains and
ORs into the returned edges. If you ever add a navigation key, wire it up in
*two* places: the hook's `bit_for` table and the `Edges`/poll mapping.

`RegisterHotKey` claims the same keys as a fallback, and `--demo` shows whether
either mechanism failed to install.

The keys are claimed for as long as the shelf is open, which includes a shelf
opened from the tray — it is on screen and being driven, so it owns the arrow
keys until Enter, Esc or a click dismisses it.

### Carousel maths

The selection is a real index (`target_idx`, 0..n-1) plus an **unwrapped**
position (`focus_pos`) moved by a spring. `nearest_virtual` picks the closest
instance of a target index, so stepping from the last wallpaper to the first
rotates by one card instead of unwinding the whole strip. Card slots outside the
focus are mapped back onto items with `wrap_index` (modulo, negative-safe), which
is what makes the rotation endless and lets a few wallpapers fill the display.

### Renderer

Everything is a rounded rectangle pushed through an affine shear — that is the
whole trick behind the parallelogram cards. Three paths exist, and none of them
ever allocates a full-size scratch buffer:

- `draw_texture_fast` — **the hot path**. One pass: horizontal anti-aliasing is
  exact against the sheared edges and the flat top/bottom edges get analytic
  vertical coverage, so no coverage mask is ever rasterised. Point sampling for
  the small dimmed cards, bilinear for the focused one. The point-sampled path
  dims, premultiplies by coverage and blends entirely in 8-bit integers, because
  it is the bulk of the card pixels and the float round-trip was visible in the
  frame cost.
- `draw_shelf` — composes the whole backdrop in **one** pass. Its rounded-rect
  mask is derived analytically per row too; the coverage buffer it used to
  rasterise spans the entire shelf, so that was megabytes of pure memory
  traffic every frame. Wash colours are read once per source texel, cross-faded
  and tinted there, and on rows that are fully covered the pixels under that
  texel are written with a single `fill` — a memset per block — which leaves
  only the anti-aliased column at each end of the row to the per-pixel path.
- `fill_profile` — **the liquid**. The shape is a span per row out of a closure,
  so the coverage it accumulates is one row of the bitmap rather than a mask the
  size of the body, and the run of pixels both of a row's two sub-samples agree on
  is written in one go. It is also told when it is the first thing drawn into a
  cleared band (`over_clear`), because then every pixel can be *stored* rather
  than blended with what is under it: same bytes, but 8x cheaper, and a body
  fading in is exactly the case that would otherwise be stuck on the per-pixel
  path. The closure hands back the row's centre as well as its width, which is
  what lets the body sway.

Blending is 8-bit integer maths throughout; float colour maths survives only
where the value genuinely varies per pixel (the bilinear sample, the shading
ramp, the accent mixes). The shapes have to be drawn this way because the naive
version (separate coverage buffer plus layered translucent passes) ran at
~10 fps. See **Performance**.

### Animation

Every animated value is a `step_spring` (semi-implicit, slightly under-damped)
rather than an exponential ease, so motion overshoots a touch and settles. Cards
shear into the direction of travel, the entrance is staggered by distance from the
focus so the strip unfurls outward, and the commit collapse eases *in* while the
skew unwinds. Springs snap to their target once close enough, which is what lets
the app stop repainting entirely when nothing is moving.

#### The liquid

The shelf's backdrop is only ever the wash panel or the liquid standing in for it,
and both are built from one outline (`Shelf`, the settled panel). Every morph is
defined against that outline and lands on it exactly, so the frame the shelf
settles on is pixel-identical to the panel and the hand-over between the two
cannot be seen. That is subtle enough to be worth pinning down, so `main.rs`
carries `liquid_geometry`, two tests that assert exactly those endpoints and that
no frame of either morph leaves the band.

- **Arriving** (`Morph::Rise`, off `vis`): a body wells up out of the bottom of
  the band. Its front rises ahead of its tail, so the body swells rather than
  sliding up as a slab; its leading edge is a meniscus — full width a little way
  behind the edge — and that edge sharpens into the panel's own corners as the
  body settles. A ripple and a sway, both fading out with the rise, give the
  surface some slosh, and they are confined to the tapered edge: past the
  meniscus the body is already the full width of the band, so wobbling it there
  would only open notches down the screen edges.
- **Leaving** (`Morph::Pour`, off `commit`): the body gathers into a neck under
  the middle, drains away from above, and trails a stream off the bottom of the
  band behind a bulge that runs down ahead of it. The cards converge on that neck
  and drop out of the band with it, which is what makes the pour look like the
  shelf going with the liquid rather than the liquid leaving through it.
- **Cards ride the surface**: until the front has passed a card it is held with
  its lower third below the surface, so the strip looks pushed up out of the
  liquid instead of fading in ahead of it.
- **Colour**: the liquid is the panel's own gradient, lit just behind its leading
  edge. `RISE_WASH` / `POUR_WASH` cross-fade it with the wallpaper wash, and the
  wash is drawn *over* it at exactly the alpha the liquid leaves, so the pair come
  out at the shelf's single alpha and nothing shows the desktop through the seam.
  Settled, the liquid's alpha is zero and it is not drawn at all.

### Applying a wallpaper

`SystemParametersInfoW(SPI_SETDESKWALLPAPER)` plus the accent registry values and
a `WM_DWMCOLORIZATIONCOLORCHANGED` broadcast. Both are slow — the broadcast alone
can take hundreds of milliseconds, and Windows transcodes the image it is given
before any of it reaches the desktop — so they run on a **separate thread**,
letting the overlay finish its animation while the desktop changes behind it. That
thread starts as the pour begins, so the wallpaper is already changing as the
liquid leaves.

The transcoding is the part that used to be felt: a 4K PNG is seconds of work,
and all of it landed *after* the pour had finished. So the wallpaper Windows is
handed is no longer the original file, but a **monitor-sized JPEG prepared ahead
of time** (`library::ready_for`), cached under
`%LOCALAPPDATA%\carasoul\cache` and keyed to the source path, its mtime and the
monitor size — edit an image and its copy is rebuilt; add one and it gets one the
first time it is shown.

The copies are made by the same background worker that decodes the thumbnails:
each image is decoded once, the thumbnail is sent to the UI, and *then* the copies
are written from the decode already in hand. The card the user has landed on is
pulled to the front of that queue, because it is the one a Shift-release is about
to need; if the apply thread ever gets there first it waits for the worker's file
rather than decoding the same 4K image twice.

There are **two** prepared copies, with different jobs and different limits:

| Copy | What it is for | Written |
| --- | --- | --- |
| Card (`.png`) | what the shelf draws — the whole reason the second start of a session is cheap | whenever an original is decoded and the original is ≥ 4× the card's pixels |
| Monitor (`.jpg`) | what Windows is handed to put on the desktop | only for the cards near the focus, or one the user actually landed on |

The card copy is **lossless** on purpose: it is only ~0.3 MB at 512x320, and it is
the thing the UI draws, so there is no reason for it to be a JPEG generation away
from the original. The monitor copy stays a JPEG because it has to be small for
Windows to swallow quickly, and it is skipped entirely when the source is not
bigger than the screen — a copy of an image that Windows would have to *upscale*
anyway adds a second JPEG generation for nothing, so small wallpapers are handed
over untouched.

The size a monitor copy is built for is the display the pointer is on when the
shelf opens (and again whenever the monitor layout changes), so the copy follows
you from one screen to the other instead of being frozen at the one from launch.
Windows paints the single wallpaper across every monitor with the chosen fit, so
on a mixed landscape/portrait pair the copy can only be exact for the display it
was sized for; the other crops it.

## Performance

Measured on this machine at 1920x1080 with `--demo` (which prints these numbers
itself, so re-measure rather than trusting the table):

| Stage | ms/frame |
| --- | --- |
| Cards | ~6.6 |
| Focus ring/glow | ~0.9 |
| Wallpaper wash (whole shelf) | ~0.95 |
| Surface clear | ~0.5 |
| **Total** | **~9.2 ms (~75 fps)** |

Those readings come from a session where the background worker was still
transcoding 4K wallpapers (see below). The same shelf measured once that queue
had drained read ~8.9 ms, so the overflow work costs the frame a few tenths of a
millisecond — the worker runs at below-normal priority for exactly that reason.

The liquid takes the wash's place for most of the entrance and all of the pour,
and the two only overlap for the brief colour hand-over, so a morphing shelf costs
less than the flat panel did. What the liquid itself costs, timed with
`cargo test --release cost -- --nocapture`:

| Liquid, 1920x1080 band | ms/frame |
| --- | --- |
| Widest body, end of the entrance | ~1.3 |
| Falling stream, the pour | ~0.2 |
| The same body blended instead of stored | ~9.9 |

The last row is why `fill_profile` takes `over_clear`: the body is drawn straight
into the cleared band, so it can be stored rather than blended, and a body that is
still fading in — most of the entrance — would otherwise never reach the fast
path. The bench times the one `fill_profile` call a frame makes and nothing else,
so the stages above still have to be added; it is a micro-benchmark rather than
`--demo`'s per-stage figure, and it moves with clock and cache (repeated runs on
this machine read anywhere from ~0.8 to ~1.3 ms). Inside a real frame the wash is
still drawn over the liquid during the hand-over, and that one *is* on the slow
path, so `--demo` reads a backdrop of ~2 ms across the first second — the entrance
and the thumbnail decode together — against ~0.95 ms once it has settled.

The demo's printed `render N/s` is the honest figure, not `1/avg`: the loop also
sleeps 3 ms a frame and pumps Win32 messages, so it reads in the sixties and
seventies while the render itself is about 9 ms. Idle is unchanged — nothing
repaints once the springs settle.

Memory, measured with `tasklist`:

| State | Working set |
| --- | --- |
| Idle after boot, shelf never opened | ~9.5 MB |
| Shelf open and settled | ~18 MB for a handful of wallpapers, ~30 MB at 16 — the thumbnails dominate |
| CPU while the shelf is open and idle | 0 ms over 8 s |
| CPU idle, shelf never opened | ~0.16 % of one core, one thread |

The idle figure grew by ~2 MB with the tray: the icon master is decoded once at
startup and turned into an `HICON` for whatever size the shell asks for.

The binary is ~870 KB and self-contained. That is ~600 KB of app plus the icon:
a 512 px PNG master embedded for the tray, and the `.ico` resource `build.rs`
links in for the file's own icon.

Thumbnails are decoded one at a time on a **below-normal-priority** thread,
sized from the monitor so the focused card is never upscaled, and released once
they fall outside a 64 MB budget. A thumbnail is decoded from the card copy when
there is one, so the shelf costs a 512x320 read rather than a multi-megapixel
decode; the run that encounters a new image still decodes the original, and writes
the card copy for every start after it. Measured over 13 wallpapers (2752x1536
JPEGs, a 4096x2304 JPEG and a 4096x2304 PNG) with a throwaway benchmark that
timed `decode` for each of them, first from the original and then from the card
copy:

| First open of a session | Wall clock |
| --- | --- |
| Decoding the originals | ~4.6 s |
| Reading the card copies | ~0.13 s (10 ms each) |

That is the pass that fills the shelf, so it is what "the shelf is ready" costs
after a boot or a restart of the app. The originals are decoded once ever, on the
run that first meets them.

Preparing the monitor-sized copies rides along on that same thread — thumbnail
first, copy second, so the shelf is never waiting on the part it does not need
yet. The worker only ever runs while a library is being prepared: it is started
by opening the shelf, and it exits once its queue is empty, which is also what
`Library::complete` reports. Timed with a throwaway harness that called
`ready_for` on the repository's own wallpapers at 1920x1080 and printed the
elapsed time (delete the cache entry first, or it is a file-exists check and
nothing more):

| Source | Prepare | Copy |
| --- | --- | --- |
| `yangyang xualing.png`, 4096x2304 PNG (14.6 MB) | ~0.75 s | 640 KB |
| `carlotta.jpg`, 4096x2304 JPEG (2.0 MB) | ~1.0 s | 441 KB |

Those are made for the cards within reach of the focus — the eight at the front of
the decode queue — and for whatever card the user lands on, which is what stops a
large folder from costing a second of background CPU per wallpaper nobody looked
at. The delay they remove — Windows' re-transcode of the original file — is not
measurable from inside the process, because it happens in the shell after
`SPI_SETDESKWALLPAPER` returns; what *is* measurable is that the shell is given
0.6 MB of already-fitted JPEG instead of 14.6 MB of PNG. A cache built for a
different monitor size simply stops matching and is swept once it is 90 days old,
as are leftovers from a process killed mid-write.

The backdrop used to be the expensive part; it is now the cheapest. What is left
is almost entirely the cards, and the single smooth (bilinear) focused card
accounts for about a millisecond of that on its own: forcing every card through
the point-sampled path is measurably ~1 ms cheaper per frame. So the next lever
is the sampler rather than the paint — the bilinear path could go to integer
fixed-point like the point-sampled one already does. The other easy win is
shape: the strip is drawn `card_h * 2.25` tall while the shelf panel only covers
`card_h * 2.05`.

## Building

Requires the **MSVC** Rust toolchain and a linker. If `cargo build` fails with
`link.exe` errors or *"you may need to install Visual Studio build tools"*:

```
winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

**Gotcha:** winget reports success immediately, but the Visual Studio installer
keeps running in the background for several more minutes and `VC/Tools/MSVC`
appears only partway through. If the first build still fails, check for a running
`setup.exe` and wait for it rather than reinstalling.

The app icon and version info are attached by `build.rs`, which writes a `.rc` and
compiles it with `rc.exe` from the Windows SDK (found on `PATH`, or in 
`%ProgramFiles(x86)%\Windows Kits\10\bin\<newest>\`). Neither is needed for the
app to work, so a build without the SDK still succeeds — it warns, and the
executable keeps the default icon. The tray icon is unaffected: it is built at
runtime from an embedded PNG. `assets/carasoul.ico` is generated by
`python tools/make_icon.py` (Pillow) from the export in `assets/icons/`, and is
committed rather than generated on every build.

A GNU toolchain also works: `rustup target add x86_64-pc-windows-gnu` plus a
MinGW-w64 `gcc` on `PATH`, then `--target x86_64-pc-windows-gnu`. The icon
resource is wired up for MSVC only (GNU would need `windres`); the build still
succeeds there, just without the file icon.

## Command line

```
carasoul                      run it (registers the startup entry)
carasoul --list               list wallpapers with their index
carasoul --apply 3            apply index 3 + its accent, no UI
carasoul --preview            hold the shelf open (UI testing, no tray icon)
carasoul --demo               preview + auto-cycle + print frame timings
carasoul --dir "D:\Art"       use another folder
carasoul --no-autostart       run once without touching the startup entry
carasoul --uninstall          remove the startup entry, then exit
```

The ordinary launch is the interesting one: it has no console of its own, so it
prints nothing and shows nothing, and everything it can be asked to do is on the
tray menu. `--preview` and `--demo` deliberately leave the tray icon alone — they
are ways to look at the shelf, not to run the app.

`--demo` also drives navigation through the same hook → pending-bits path a
keyboard uses, so it doubles as a test of the input wiring. It prints the number
of hotkeys claimed and whether the hook installed.

## Wallpapers

`.jpg`, `.jpeg`, `.jfif` and `.png`. Sorted by file name.

**The folder is re-read every time the shelf opens**, so dropping an image in and
holding Shift finds it — no restart, and no need to ask for a rescan. That is one
directory listing, and it only rebuilds the library (and restarts the decode
worker) when the listing actually changed; the wallpaper under the focus marker
is followed across the rebuild *by path*, so a new image appearing next to it
cannot quietly become the selection. An image added while the shelf is already
open needs **Rescan wallpapers** or a close-and-reopen; there is no filesystem
watcher, deliberately — a watcher thread would cost more at idle than the
listing costs on the rare occasion the folder changes.

Other formats need a feature flag on the `image` dependency in `Cargo.toml`
(e.g. `"webp"`, `"bmp"`).

The folder is found by trying, in order: `--dir`, `%WALLPAPER_DIR%`, then
`wallpapers/` walking **up to four levels** from the executable (so a dev build
in `target/release/` finds the one at the project root), then `wallpapers/` in
the working directory. If none contains images, the app says so once through a
tray balloon instead of silently doing nothing — which is also why the tray menu
can open the folder, creating it if it is not there yet.

## Cache

`%LOCALAPPDATA%\carasoul\cache` holds the two prepared copies (see **Applying a
wallpaper**) as well as nothing else. It is derived data: deleting it is always
safe, and costs one slower open while the copies are rebuilt. Entries older than
90 days — and leftovers from a process killed mid-write — are swept on the first
copy of each run.

| File | Contents | Rough size |
| --- | --- | --- |
| `<hash>.png` | the card-sized, **lossless** image the shelf draws | ~0.3 MB (512x320) |
| `<hash>.jpg` | the monitor-sized JPEG Windows is handed, quality 92 | ~0.6 MB (1920x1080) |

`<hash>` is an FNV-1a of the source's canonical path, its size and mtime, and the
size the copy was made for — so editing an image, or changing monitors, quietly
invalidates the affected entries rather than serving something stale.

## Theme matching

Picking a wallpaper writes the Windows accent colour
(`HKCU\...\DWM\AccentColor`, `ColorPrevalence`, the Explorer accent palette) and
broadcasts `WM_DWMCOLORIZATIONCOLORCHANGED`. The accent is taken from the most
vivid mid-bright region of the image, then normalised into a range that stays
legible.

Windows treats this as undocumented territory: it may reset the accent on its
own, and some surfaces (Start, Settings) can need an explorer restart. Title bars
and the taskbar usually follow immediately.

## Tuning

The look is a handful of constants, all of them named:

| Where | What |
| --- | --- |
| `main.rs` `step_spring` calls | Spring feel: frequency and damping |
| `main.rs` `HOLD_MS`, `COMMIT_SECS`, `COMMIT_FADE` | Trigger hold, pour length, where in the pour it fades out |
| `main.rs` `RISE_*`, `POUR_*` | The liquid's milestones: how far the front has come, where the dome becomes the panel, where the panel becomes the stream, and each colour hand-over |
| `main.rs` `Liquid` | The liquid's shape: meniscus taper, ripple and sway, stream width, bulge size and speed |
| `main.rs` `build_geometry` | Card scale, lift, shear, stagger, cull distance, and how far the cards ride the surface |
| `main.rs` `make_layout` | Card size, spacing, corner radius |
| `main.rs` `strip_for`, `short_side` | Shelf height and vertical position; which monitor dimension the cards are sized from |
| `main.rs` `palette_for` | How much the accent tints the shelf |
| `canvas.rs` `draw_shelf` | Wash shading (top → bottom) and accent mix |
| `canvas.rs` `fill_profile` | How a row of the liquid is coloured and anti-aliased |
| `library.rs` | Thumbnail size bounds, the memory budget, the JPEG quality of the monitor copies, and the two caching limits (`CARD_CACHE_RATIO`, `PREPARE_WINDOW`) |

## Gotchas and limitations

- **Multiple monitors, one wallpaper.** The shelf appears on whichever display
  holds the pointer when it opens — a portrait panel included, where the cards
  are sized from the monitor's short side — and the prepared copy handed to
  Windows is sized for that same display. Windows still applies a single
  wallpaper to all of them, though, so with displays of different shapes the
  other monitor crops that copy rather than the original. It fills correctly;
  it is just one JPEG generation from an image already cropped once. The shell's
  simple wallpaper call only takes one file, so there is no way to hand it a
  copy per monitor without going through the slideshow plumbing instead.
- **The prepared copies are disk, not memory.** Roughly 0.9 MB per wallpaper ever
  *shown* — ~0.3 MB of lossless card copy plus ~0.6 MB of monitor-sized copy — in
  `%LOCALAPPDATA%\carasoul\cache`. Monitor copies are only made for the cards
  within reach of the focus, and never for wallpapers no bigger than the screen,
  so a folder of small images costs almost nothing. Nothing else keeps a copy:
  deleting the folder is always safe, it just means the next open pays for it
  again (a few seconds, in the background).
- **A new tray icon starts in the overflow.** Windows 11 hides icons it has not
  seen before under the `^` chevron; drag ours onto the taskbar, or promote it in
  *Settings → Personalisation → Taskbar → Other system tray icons*. If it ends up
  hidden somewhere confusing, remember that **launching the app again opens the
  shelf** on the instance already running, so it is never unreachable.
- **Right-clicking on the desktop** to cancel may also open the desktop's own
  context menu — the right button is not swallowed (that needs a separate
  `WH_MOUSE_LL` hook).
- **The tray menu claims the foreground before it opens.** A popup owned by an
  invisible window will not close when the user clicks away from it unless
  `SetForegroundWindow` is called first (and `WM_NULL` posted afterwards, the
  usual dance). If the menu ever starts behaving oddly, that is the first thing
  to check.
- **Renamed from `wallpaper-switcher`.** The startup entry is now `Carasoul`; the
  old `WallpaperSwitcher` value is deleted the next time the app runs with the
  startup entry enabled, so an upgrade does not leave two entries racing at logon.
- The binary is **unsigned**, so SmartScreen may warn on first run.
- The keyboard hook exists only while the shelf is open; that is deliberate, and
  worth preserving if you touch it.
- **The liquid only has the shelf band to move in.** The overlay is a band, not
  the screen (that is what keeps the frame cheap), so the pour reads as falling off
  the bottom of the band towards the desktop rather than visibly landing on it:
  the stream is clipped just under four-fifths of the way down the screen.
  Covering more would mean a full-screen layered window, which neither the
  per-frame cost nor the memory wants.
