//! Wallpaper folder scanning, background thumbnail decoding and the accent
//! colour that the UI (and optionally Windows) follows.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::sys;

/// Decoding size defaults; the real size is chosen from the monitor so the
/// focused card is never upscaled (which is what made the images look soft).
pub const MIN_THUMB_W: u32 = 512;
pub const MAX_THUMB_W: u32 = 1280;

/// Second, much smaller decode of each image, stretched across the shelf as a
/// soft colour wash so the backdrop reflects the wallpaper being previewed.
pub const WASH_W: u32 = 40;
pub const WASH_H: u32 = 25;

/// Roughly how much memory thumbnails may occupy before distant ones are dropped.
const THUMB_BUDGET: usize = 64 * 1024 * 1024;

/// Quality of the monitor-sized copies handed to Windows. High enough that the
/// re-encode is invisible on a photo, low enough that the file is small.
const READY_QUALITY: u8 = 92;

/// How much bigger than a card an original has to be for a card-sized copy to be
/// worth its disk. Below this, decoding the original again is already cheap, and
/// a copy would only add a file — a 4K image is ~25x the card, an 850x544 one is
/// under 3x.
const CARD_CACHE_RATIO: u64 = 4;

/// How far down the queue the worker will go **on spec**, building monitor-sized
/// copies for cards a commit could plausibly land on. Beyond that, a copy is only
/// built when a card is actually landed on — `request_ready` jumps the queue for
/// those — which keeps a larger folder from costing a second of CPU per image for
/// wallpapers nobody ever looked at.
const PREPARE_WINDOW: usize = 8;

pub struct Item {
    pub path: PathBuf,
    pub name: String,
    /// 0x00RRGGBB, decoded in the background.
    pub thumb: Option<Vec<u32>>,
    pub wash: Option<Vec<u32>>,
    pub accent: Option<[u8; 3]>,
}

struct Decoded {
    idx: usize,
    thumb: Vec<u32>,
    wash: Vec<u32>,
    accent: [u8; 3],
}

pub struct Library {
    pub dir: PathBuf,
    pub items: Vec<Item>,
    pub thumb_w: u32,
    pub thumb_h: u32,
    /// Size of the monitor-sized copies (see [`ready_for`]); the monitor's own
    /// resolution, since that is what Windows has to fill.
    pub ready_w: u32,
    pub ready_h: u32,
    /// The folder's contents as of the last scan, in carousel order. Kept so a
    /// refresh can tell "nothing changed" from "something changed" without
    /// redoing any work.
    files: Vec<PathBuf>,
    rx: Option<Receiver<Decoded>>,
    /// Where the UI asks for one item to be prepared ahead of the queue.
    req: Option<Sender<usize>>,
    paths: Vec<PathBuf>,
    order: Vec<usize>,
    pub complete: bool,
}

impl Library {
    pub fn load(
        dir_override: Option<PathBuf>,
        current_wallpaper: Option<&str>,
        thumb_w: u32,
        thumb_h: u32,
        ready_w: u32,
        ready_h: u32,
    ) -> Self {
        let dir = resolve_dir(dir_override);
        let files = listing(&dir);
        let items = scan(&files);
        let hint = current_wallpaper.and_then(|cur| match_item(&items, Path::new(cur)));
        let order = queue_order(items.len(), hint);
        let paths: Vec<PathBuf> = order.iter().map(|i| items[*i].path.clone()).collect();

        // Nothing is decoded yet: the app stays tiny until the strip is opened
        // for the first time, and only then does the worker thread spin up.
        let complete = items.is_empty();
        Self {
            dir,
            items,
            thumb_w,
            thumb_h,
            ready_w,
            ready_h,
            files,
            rx: None,
            req: None,
            paths,
            order,
            complete,
        }
    }

    /// Starts the background decode pass. Cheap and safe to call repeatedly.
    pub fn start_prefetch(&mut self) {
        if self.rx.is_some() || self.paths.is_empty() {
            return;
        }
        let (tx, rx) = channel::<Decoded>();
        let (req_tx, req_rx) = channel::<usize>();
        self.rx = Some(rx);
        self.req = Some(req_tx);
        let paths = std::mem::take(&mut self.paths);
        let idxs = std::mem::take(&mut self.order);
        let (tw, th) = (self.thumb_w, self.thumb_h);
        let (rw, rh) = (self.ready_w, self.ready_h);
        thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || worker(tx, req_rx, paths, idxs, (tw, th), (rw, rh)))
            .ok();
    }

    /// Re-reads the folder, so images added while the app is running show up
    /// without a restart. Thumbnails already decoded for a path are kept: this
    /// rescans, it does not re-decode.
    pub fn rescan(&mut self, current_wallpaper: Option<&Path>) {
        self.files = listing(&self.dir);
        self.rebuild(current_wallpaper);
    }

    /// Re-reads the folder **only if its contents changed**, which is one
    /// directory listing. Called every time the shelf opens, so dropping an image
    /// in the folder and holding Shift just works, with nothing to watch and
    /// nothing to pay for when the folder is untouched.
    ///
    /// Returns true when the library was rebuilt.
    pub fn refresh_if_changed(&mut self, current_wallpaper: Option<&Path>) -> bool {
        let listed = listing(&self.dir);
        if listed == self.files {
            return false;
        }
        self.files = listed;
        self.rebuild(current_wallpaper);
        true
    }

    fn rebuild(&mut self, current_wallpaper: Option<&Path>) {
        let mut old = std::mem::take(&mut self.items);
        let mut items = scan(&self.files);
        for it in items.iter_mut() {
            if let Some(prev) = old.iter_mut().find(|p| p.path == it.path) {
                it.thumb = prev.thumb.take();
                it.wash = prev.wash.take();
                it.accent = prev.accent;
            }
        }
        // Whatever is running is decoding a list that no longer exists.
        self.rx = None;
        self.req = None;
        self.items = items;
        self.complete = self.items.is_empty();
        let hint = current_wallpaper.and_then(|cur| match_item(&self.items, cur));
        self.order = queue_order(self.items.len(), hint);
        self.paths = self
            .order
            .iter()
            .map(|i| self.items[*i].path.clone())
            .collect();
    }

    /// Asks the worker to prepare this wallpaper next. The user is looking at it,
    /// which means it is the one a Shift-release is about to apply.
    pub fn request_ready(&self, idx: usize) {
        if let Some(tx) = self.req.as_ref() {
            let _ = tx.send(idx);
        }
    }

    /// Drains whatever the worker finished; returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = self.rx.as_ref() else {
            return false;
        };
        let mut got = false;
        loop {
            match rx.try_recv() {
                Ok(d) => {
                    if let Some(it) = self.items.get_mut(d.idx) {
                        it.thumb = Some(d.thumb);
                        it.wash = Some(d.wash);
                        it.accent = Some(d.accent);
                    }
                    got = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    self.complete = true;
                    break;
                }
            }
        }
        got
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn accent(&self, i: usize) -> Option<[u8; 3]> {
        self.items.get(i).and_then(|it| it.accent)
    }

    /// Releases thumbnails that are far from the focus once they would exceed the
    /// memory budget, so memory stays bounded no matter how many you add.
    pub fn trim(&mut self, focus: usize) {
        let per_thumb = (self.thumb_w * self.thumb_h * 4) as usize;
        let limit = (THUMB_BUDGET / per_thumb.max(1)).max(6);
        if self.items.len() <= limit {
            return;
        }
        let keep = (limit / 2).max(4);
        let (lo, hi) = (focus.saturating_sub(keep), focus + keep);
        for (i, it) in self.items.iter_mut().enumerate() {
            if i < lo || i > hi {
                it.thumb = None;
            }
        }
    }

    /// Index of the wallpaper Windows currently has set, if we can find it.
    pub fn index_of(&self, current: &str) -> Option<usize> {
        match_item(&self.items, Path::new(current))
    }

    /// File name of an item, for the tray tooltip.
    pub fn name(&self, i: usize) -> Option<&str> {
        self.items.get(i).map(|it| it.name.as_str())
    }
}

fn match_item(items: &[Item], current: &Path) -> Option<usize> {
    items.iter().position(|it| it.path == current).or_else(|| {
        current.file_name().and_then(|n| {
            let n = n.to_string_lossy().to_lowercase();
            items.iter().position(|it| it.name.to_lowercase() == n)
        })
    })
}

/// Where the selection belongs after the folder has been re-read: on the
/// wallpaper it was already on, found **by path**. Indices are no good for this,
/// because an image that sorts before the current one shifts every index after
/// it — following the index instead would silently change which wallpaper a
/// Shift-release applies.
pub fn index_after_refresh(items: &[Item], previous: Option<&Path>) -> usize {
    previous
        .and_then(|p| items.iter().position(|it| it.path == p))
        .unwrap_or(0)
}

fn resolve_dir(override_dir: Option<PathBuf>) -> PathBuf {
    if let Some(p) = override_dir {
        return p;
    }
    if let Ok(p) = std::env::var("WALLPAPER_DIR") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    // Walk up from the executable: an installed build keeps `wallpapers` next to
    // the exe, while a dev build sits in target/release with the folder several
    // levels above it. Both layouts have to find the same images.
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent();
        for _ in 0..4 {
            let Some(d) = dir else { break };
            candidates.push(d.join("wallpapers"));
            dir = d.parent();
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("wallpapers"));
    }

    candidates
        .iter()
        .find(|c| has_images(c))
        .or_else(|| candidates.iter().find(|c| c.is_dir()))
        .cloned()
        .unwrap_or_else(|| {
            candidates
                .into_iter()
                .next()
                .unwrap_or_else(|| PathBuf::from("wallpapers"))
        })
}

fn is_supported(path: &Path) -> bool {
    path.is_file()
        && matches!(
            path.extension()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default()
                .as_str(),
            "jpg" | "jpeg" | "jfif" | "png"
        )
}

fn has_images(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().any(|e| is_supported(&e.path())))
        .unwrap_or(false)
}

fn scan(files: &[PathBuf]) -> Vec<Item> {
    files
        .iter()
        .map(|path| Item {
            name: path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: path.clone(),
            thumb: None,
            wash: None,
            accent: None,
        })
        .collect()
}

/// The folder's supported images, in carousel order. Kept separate from building
/// the items so a refresh can compare listings without allocating anything else.
fn listing(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_supported(p))
        .collect();
    out.sort_by_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    out
}

/// Decode order for a fresh scan: the wallpaper Windows already has set first,
/// then outwards, so the first Shift-hold has something to show immediately.
fn queue_order(n: usize, hint: Option<usize>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if let Some(h) = hint {
        order.sort_by_key(|i| (*i as i64 - h as i64).abs());
    }
    order
}

/// Everything one decode is good for: the thumbnail the cards are drawn from, the
/// wash and accent that come with it, and — when the *original* was decoded rather
/// than a card-sized copy of it — the pieces the two caches are built from.
struct Preview {
    thumb: Vec<u32>,
    wash: Vec<u32>,
    accent: [u8; 3],
    fresh: Option<Fresh>,
}

struct Fresh {
    /// The original, for the monitor-sized copy: fitting it again would mean
    /// decoding it again.
    full: image::DynamicImage,
    /// The card-sized fit, kept so the lossless card copy costs no second fit.
    card: image::RgbImage,
}

fn worker(
    tx: Sender<Decoded>,
    req: Receiver<usize>,
    paths: Vec<PathBuf>,
    idxs: Vec<usize>,
    thumb_size: (u32, u32),
    ready_size: (u32, u32),
) {
    let (tw, th) = thumb_size;
    let (rw, rh) = ready_size;
    sys::lower_current_thread_priority();

    // The queue is walked front to back, but a request from the UI (the card the
    // user has just landed on) is pulled to the front of it: that is the image a
    // Shift-release is about to need, and the whole point of preparing ahead is
    // that the wait has already happened by then.
    let mut pos_of = vec![usize::MAX; idxs.iter().copied().max().map_or(0, |m| m + 1)];
    for (pos, &idx) in idxs.iter().enumerate() {
        pos_of[idx] = pos;
    }
    let mut pending: VecDeque<usize> = (0..paths.len()).collect();
    let mut done = vec![false; paths.len()];

    loop {
        let mut wanted = None;
        while let Ok(idx) = req.try_recv() {
            wanted = Some(idx);
        }
        let (pos, requested) = match wanted
            .and_then(|idx| pos_of.get(idx).copied())
            .filter(|p| *p != usize::MAX && !done[*p])
        {
            Some(p) => {
                done[p] = true;
                (p, true)
            }
            None => {
                let mut next = None;
                while let Some(p) = pending.pop_front() {
                    if !done[p] {
                        done[p] = true;
                        next = Some(p);
                        break;
                    }
                }
                match next {
                    Some(p) => (p, false),
                    None => break,
                }
            }
        };
        let path = &paths[pos];
        let Some(preview) = decode(path, tw, th) else {
            continue;
        };
        let send = tx.send(Decoded {
            idx: idxs[pos],
            thumb: preview.thumb,
            wash: preview.wash,
            accent: preview.accent,
        });
        // Showing comes before preparing: the thumbnail is what the shelf needs
        // right now, the copy is for a Shift-release that may never happen.
        if let Some(fresh) = preview.fresh.as_ref() {
            // The lossless card copy is cheap and pays for itself the next time
            // the app starts, so every image gets one.
            if worth_caching(fresh.full.width(), fresh.full.height(), tw, th) {
                if let Some(dst) = card_file(path, tw, th) {
                    write_card(&dst, &fresh.card);
                }
            }
            if prepares_on_spec(pos, requested) {
                prepare(path, Some(&fresh.full), rw, rh);
            }
        }
        if send.is_err() {
            break;
        }
    }
}

/// Whether the worker should build this card's monitor-sized copy without being
/// asked: the ones near the focus at open time, and anything the user landed on.
fn prepares_on_spec(position: usize, requested: bool) -> bool {
    requested || position < PREPARE_WINDOW
}

/// A card copy is only worth keeping when the original carries far more pixels
/// than the card needs.
fn worth_caching(src_w: u32, src_h: u32, tw: u32, th: u32) -> bool {
    (src_w as u64 * src_h as u64) >= CARD_CACHE_RATIO * (tw as u64) * (th as u64)
}

fn decode(src: &Path, tw: u32, th: u32) -> Option<Preview> {
    // Cards are drawn from the card-sized copy when there is one: reading 512x320
    // is ~35x faster than decoding a 4K original, and it is **lossless**, so the
    // pixels are exactly the ones the original would have produced. That is what
    // makes the first Shift-hold after a boot cheap, since the shelf is drawn from
    // this and nothing else.
    let card = card_file(src, tw, th).filter(|p| p.is_file());
    let decoded = image::open(card.as_deref().unwrap_or(src)).ok()?;
    if decoded.width() == 0 || decoded.height() == 0 {
        return None;
    }

    let (small, fresh) = if card.is_some() {
        // Already the card's size and shape.
        (decoded, None)
    } else {
        // Aspect preserved, centre-cropped to the card ratio. CatmullRom keeps the
        // downscale crisp where Triangle looked noticeably soft.
        let small = decoded.resize_to_fill(tw, th, image::imageops::FilterType::CatmullRom);
        (small, Some(decoded))
    };

    let rgb = small.to_rgb8();
    let mut px = Vec::with_capacity((tw * th) as usize);
    for p in rgb.pixels() {
        px.push(((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32);
    }
    let accent = accent_from(&px);

    let tiny = image::imageops::resize(&rgb, WASH_W, WASH_H, image::imageops::FilterType::Triangle);
    let mut wash = Vec::with_capacity((WASH_W * WASH_H) as usize);
    for p in tiny.pixels() {
        wash.push(((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32);
    }

    let fresh = fresh.map(|full| Fresh { full, card: rgb });
    Some(Preview {
        thumb: px,
        wash,
        accent,
        fresh,
    })
}

/// Decodes one image just far enough to name its accent colour, for the
/// `--apply` command line path.
pub fn accent_of(path: &Path) -> Option<[u8; 3]> {
    decode(path, 320, 200).map(|p| p.accent)
}

// ---------------------------------------------------------------------------
// the two prepared copies
// ---------------------------------------------------------------------------

/// Windows is handed a wallpaper as a *file*, and transcodes whatever it is given
/// before it reaches the desktop: a 4K PNG is seconds of work, all of it landing
/// after the pour has finished. So each image is decoded once, fitted to the
/// monitor and re-encoded as a JPEG while the shelf is still open, which leaves
/// Windows almost nothing to do when the time comes.
///
/// Called from two places: the decode worker (which already holds the decoded
/// image) and the apply thread, as a fallback for an item the worker has not
/// reached. Whichever gets there second waits for the first rather than decoding
/// the same 4K file twice.
pub fn ready_for(src: &Path, w: u32, h: u32) -> Option<PathBuf> {
    prepare(src, None, w, h)
}

fn prepare(src: &Path, decoded: Option<&image::DynamicImage>, w: u32, h: u32) -> Option<PathBuf> {
    if w == 0 || h == 0 {
        return None;
    }
    let dst = ready_file(src, w, h)?;
    prune_once(&dst);

    // A source that is not bigger than the screen has nothing to gain from a
    // copy: Windows scales it just as well, and one of ours would bake in an
    // upscale *and* a second JPEG generation. Handing over the original is both
    // sharper and cheaper. (Checked before the cache, so a copy made by an older
    // build — when this was unconditional — is never used again.)
    let (src_w, src_h) = match decoded {
        Some(img) => (img.width(), img.height()),
        None => image::image_dimensions(src).ok()?,
    };
    if src_w <= w && src_h <= h {
        return None;
    }
    if dst.is_file() {
        return Some(dst);
    }

    if !claim(&dst) {
        // Somebody else is building this one; wait for their file rather than
        // repeating the decode. Five seconds is far longer than a decode takes.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if dst.is_file() {
                return Some(dst);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    let built = match decoded {
        Some(img) => write_ready(img, &dst, w, h),
        None => image::open(src).is_ok_and(|img| write_ready(&img, &dst, w, h)),
    };
    release(&dst);
    (built && dst.is_file()).then_some(dst)
}

/// The lossless card-sized copy the shelf is drawn from, written once per source
/// and read back by every start after that. PNG rather than JPEG because it is
/// only ~0.2 MB and 512x320: there is no reason for the thing the UI draws to be
/// a generation away from the original.
fn write_card(dst: &Path, card: &image::RgbImage) -> bool {
    use image::ImageEncoder;

    // Written and then renamed, so "the file exists" always means "it is whole".
    // (The encoder is told the format rather than left to read it off the file
    // name, because the temporary's name is not one it would recognise.)
    let tmp = dst.with_extension("part");
    let saved = (|| -> Option<()> {
        let file = std::fs::File::create(&tmp).ok()?;
        let mut out = std::io::BufWriter::new(file);
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(
                card.as_raw(),
                card.width(),
                card.height(),
                image::ExtendedColorType::Rgb8,
            )
            .ok()?;
        out.flush().ok()?;
        Some(())
    })()
    .is_some();

    if saved && std::fs::rename(&tmp, dst).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(&tmp);
    false
}

/// Fits the image to the monitor and writes it as a JPEG, atomically: the file
/// only appears under its final name once it is complete, which is what lets the
/// other thread treat "file exists" as "it is ready".
fn write_ready(img: &image::DynamicImage, dst: &Path, w: u32, h: u32) -> bool {
    let fitted;
    let img = if img.width() == w && img.height() == h {
        img
    } else {
        fitted = img.resize_to_fill(w, h, image::imageops::FilterType::CatmullRom);
        &fitted
    };

    let tmp = dst.with_extension("part");
    let written = (|| -> Option<()> {
        let file = std::fs::File::create(&tmp).ok()?;
        let mut out = std::io::BufWriter::new(file);
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, READY_QUALITY);
        enc.encode_image(img).ok()?;
        out.flush().ok()?;
        Some(())
    })()
    .is_some();

    if written && std::fs::rename(&tmp, dst).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(&tmp);
    false
}

/// Where the copies live: `%LOCALAPPDATA%\carasoul\cache`, one file per source
/// image, size and modification time.
///
/// The monitor-sized copies keep the key they have always had, because the hash
/// changing would throw away every copy on disk and make the next open rebuild
/// all of them; the card copies are a new namespace, and take a `card` tag so the
/// two can never collide.
fn ready_file(src: &Path, w: u32, h: u32) -> Option<PathBuf> {
    cached_path(src, "", "jpg", w, h)
}

fn card_file(src: &Path, tw: u32, th: u32) -> Option<PathBuf> {
    cached_path(src, "card|", "png", tw, th)
}

fn cached_path(src: &Path, tag: &str, ext: &str, w: u32, h: u32) -> Option<PathBuf> {
    let src = std::fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let meta = std::fs::metadata(&src).ok()?;
    let stamp = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{tag}{}|{stamp}|{}|{w}x{h}", src.display(), meta.len()).bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(cache_dir()?.join(format!("{hash:016x}.{ext}")))
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("TEMP"))
        .map(PathBuf::from)?;
    let dir = base.join("carasoul").join("cache");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Copies of wallpapers that have since changed or been deleted would otherwise
/// pile up forever, so the cache is swept once per run. Leftover temporaries go
/// too: they are only ever left behind by a process that was killed mid-write,
/// and a completed write always renames its own away.
fn prune_once(dst: &Path) {
    static SWEPT: AtomicBool = AtomicBool::new(false);
    if SWEPT.swap(true, Ordering::Relaxed) {
        return;
    }
    const KEEP: Duration = Duration::from_secs(60 * 60 * 24 * 90);
    let Some(dir) = dst.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().is_ok_and(|age| age > KEEP))
            .unwrap_or(false);
        if stale || entry.path().extension().is_some_and(|e| e == "part") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The one build of a given file at a time. Held only while an image is being
/// transcoded; see [`prepare`].
static IN_FLIGHT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn claim(dst: &Path) -> bool {
    let mut guard = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if guard.iter().any(|p| p == dst) {
        return false;
    }
    guard.push(dst.to_path_buf());
    true
}

fn release(dst: &Path) {
    let mut guard = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|p| p != dst);
}

/// Picks a vivid, mid-bright representative colour and nudges it into a range
/// that reads well as a UI accent.
pub fn accent_from(px: &[u32]) -> [u8; 3] {
    let mut weight = vec![0f32; 4096];
    let mut sums = vec![0f32; 4096 * 3];
    let mut total = 0f32;

    for &p in px.iter().step_by(3) {
        let r = ((p >> 16) & 0xff) as f32 / 255.0;
        let g = ((p >> 8) & 0xff) as f32 / 255.0;
        let b = (p & 0xff) as f32 / 255.0;
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        let sat = if mx > 0.001 { (mx - mn) / mx } else { 0.0 };
        let w = sat * sat * (0.25 + mx);
        let bin = (((r * 15.0) as usize) << 8) | (((g * 15.0) as usize) << 4) | (b * 15.0) as usize;
        weight[bin] += w;
        sums[bin * 3] += r * w;
        sums[bin * 3 + 1] += g * w;
        sums[bin * 3 + 2] += b * w;
        total += w;
    }

    // Fallback: plain average, for near-greyscale wallpapers.
    let mean = {
        let mut acc = [0f32; 3];
        let mut n = 0f32;
        for &p in px.iter().step_by(7) {
            acc[0] += ((p >> 16) & 0xff) as f32 / 255.0;
            acc[1] += ((p >> 8) & 0xff) as f32 / 255.0;
            acc[2] += (p & 0xff) as f32 / 255.0;
            n += 1.0;
        }
        if n > 0.0 {
            [acc[0] / n, acc[1] / n, acc[2] / n]
        } else {
            [0.30, 0.30, 0.35]
        }
    };

    let rgb = if total < 0.5 {
        [mean[0], mean[1], mean[2]]
    } else {
        let best = weight
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let w = weight[best].max(1e-6);
        [
            sums[best * 3] / w,
            sums[best * 3 + 1] / w,
            sums[best * 3 + 2] / w,
        ]
    };

    let (h, s, l) = rgb_to_hsl(rgb[0], rgb[1], rgb[2]);
    let s = if s < 0.08 { 0.06 } else { s.clamp(0.35, 0.95) };
    let l = l.clamp(0.45, 0.72);
    let out = hsl_to_rgb(h, s, l);
    [
        (out[0] * 255.0).round().clamp(0.0, 255.0) as u8,
        (out[1] * 255.0).round().clamp(0.0, 255.0) as u8,
        (out[2] * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let l = (mx + mn) * 0.5;
    let d = mx - mn;
    if d.abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-6);
    let h = if mx == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if mx == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h.rem_euclid(1.0), s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(1.0) * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c * 0.5;
    [r + m, g + m, b + m]
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a source image somewhere private, and hands back its path along
    /// with a cleanup guard.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("carasoul-test-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self { dir }
        }

        fn image(&self, name: &str, w: u32, h: u32) -> PathBuf {
            let mut img = image::RgbImage::new(w, h);
            for (x, y, px) in img.enumerate_pixels_mut() {
                *px = image::Rgb([(x % 251) as u8, (y % 241) as u8, 96]);
            }
            let path = self.dir.join(name);
            img.save(&path).expect("write source image");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn remove(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// The whole point of the copy: Windows is handed something monitor-sized
    /// instead of a 4K original, and asking twice does not transcode twice.
    #[test]
    fn ready_copy_is_monitor_sized_and_reused() {
        let scratch = Scratch::new("ready");
        let src = scratch.image("source.png", 1600, 900);

        let ready = ready_for(&src, 640, 360).expect("ready copy");
        assert!(ready.is_file());
        assert_eq!(image::image_dimensions(&ready).unwrap(), (640, 360));

        let stamp = std::fs::metadata(&ready).unwrap().modified().unwrap();
        let again = ready_for(&src, 640, 360).expect("cached copy");
        assert_eq!(again, ready, "a second ask should reuse the same file");
        assert_eq!(
            std::fs::metadata(&again).unwrap().modified().unwrap(),
            stamp,
            "a second ask should not rewrite the file"
        );
        remove(&ready);
    }

    /// Editing a wallpaper has to invalidate its copy, or a changed image would
    /// keep applying as the old one.
    #[test]
    fn edited_source_gets_a_new_copy() {
        let scratch = Scratch::new("edited");
        let src = scratch.image("source.png", 800, 450);
        let first = ready_for(&src, 320, 180).expect("ready copy");
        assert!(first.is_file());

        // A different image, written over the same name.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut img = image::RgbImage::new(800, 450);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 97) as u8, 200, 30]);
        }
        img.save(&src).expect("rewrite source");

        let second = ready_for(&src, 320, 180).expect("rebuilt copy");
        assert_ne!(second, first, "the copy should be keyed to the source");
        remove(&first);
        remove(&second);
    }

    /// The worker and the apply thread can both ask for the same image. Neither
    /// may come back empty, and both must agree on where it is.
    #[test]
    fn concurrent_asks_agree() {
        let scratch = Scratch::new("concurrent");
        let src = scratch.image("source.png", 1200, 675);
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let src = src.clone();
                std::thread::spawn(move || ready_for(&src, 400, 225))
            })
            .collect();
        let paths: Vec<PathBuf> = handles
            .into_iter()
            .map(|h| h.join().unwrap().expect("both asks should succeed"))
            .collect();
        assert_eq!(paths[0], paths[1]);
        assert_eq!(image::image_dimensions(&paths[0]).unwrap(), (400, 225));
        remove(&paths[0]);
    }

    /// A rescan keeps what was already decoded and follows the wallpaper that is
    /// still set, so adding images to the folder does not cost a fresh decode pass.
    #[test]
    fn rescan_keeps_decoded_items() {
        let scratch = Scratch::new("rescan");
        let first = scratch.image("a.png", 200, 100);
        let mut lib = Library::load(Some(scratch.dir.clone()), None, 200, 125, 400, 225);
        assert_eq!(lib.len(), 1);
        lib.items[0].thumb = Some(vec![0x112233]);
        lib.items[0].accent = Some([1, 2, 3]);

        let _second = scratch.image("b.png", 200, 100);
        lib.rescan(Some(&first));

        assert_eq!(lib.len(), 2, "the new image should be picked up");
        let kept = lib
            .items
            .iter()
            .find(|it| it.path == first)
            .expect("a.png still there");
        assert_eq!(
            kept.thumb.as_deref(),
            Some(&[0x112233u32][..]),
            "thumb kept"
        );
        assert_eq!(kept.accent, Some([1, 2, 3]));
        assert_eq!(lib.paths.len(), 2, "the prefetch queue was rebuilt");
    }

    /// An image dropped into the folder while the app is running has to show up the
    /// next time the shelf opens. An untouched folder, on the other hand, must not
    /// rebuild anything: that would restart the decode worker for nothing.
    #[test]
    fn refresh_picks_up_new_images_only_when_the_folder_changes() {
        let scratch = Scratch::new("refresh");
        let first = scratch.image("a.png", 200, 100);
        let mut lib = Library::load(Some(scratch.dir.clone()), None, 200, 125, 400, 225);
        assert_eq!(lib.len(), 1);

        assert!(
            !lib.refresh_if_changed(Some(&first)),
            "an unchanged folder was rebuilt"
        );

        let second = scratch.image("b.png", 200, 100);
        assert!(
            lib.refresh_if_changed(Some(&first)),
            "the new image was missed"
        );
        assert_eq!(lib.len(), 2);

        std::fs::remove_file(&second).expect("remove b.png");
        assert!(
            lib.refresh_if_changed(Some(&first)),
            "the deleted image was missed"
        );
        assert_eq!(lib.len(), 1);
    }

    /// The selection follows the file the user was on, not the slot number: an
    /// image that sorts before it shifts every index after it, and a Shift-release
    /// must not start applying a different wallpaper because of it.
    #[test]
    fn selection_follows_the_path_not_the_index() {
        let scratch = Scratch::new("selection");
        let first = scratch.image("m.png", 200, 100);
        let mut lib = Library::load(Some(scratch.dir.clone()), None, 200, 125, 400, 225);
        let previous = lib.items[0].path.clone();

        scratch.image("a.png", 200, 100); // sorts before m.png, so m.png moves
        assert!(lib.refresh_if_changed(Some(&previous)));
        let moved = index_after_refresh(&lib.items, Some(&previous));
        assert_eq!(lib.items[moved].path, previous);
        assert_ne!(moved, 0, "this test is pointless unless the index moved");
        assert_eq!(lib.items[moved].name, "m.png");
        let _ = first;
    }

    /// The card copy is what the shelf is drawn from after the first run, so it
    /// has to be *exactly* what the original would have produced, and it has to be
    /// preferred over the original once it exists.
    #[test]
    fn card_copy_is_lossless_and_preferred() {
        let scratch = Scratch::new("card-copy");
        let src = scratch.image("source.png", 1600, 900);
        let (tw, th) = (512, 320);

        let first = decode(&src, tw, th).expect("decode the original");
        assert!(first.fresh.is_some(), "the original was decoded");
        let card = card_file(&src, tw, th).expect("card path");
        assert!(write_card(&card, &first.fresh.as_ref().unwrap().card));

        let second = decode(&src, tw, th).expect("decode the copy");
        assert!(
            second.fresh.is_none(),
            "the card copy should have been used"
        );
        assert_eq!(second.thumb, first.thumb, "cards must not change");
        assert_eq!(second.wash, first.wash, "and neither may the wash");
        assert_eq!(second.accent, first.accent, "or the accent");

        remove(&card);
    }

    /// Keep the card copies bounded: only for originals that are expensive
    /// relative to what the card needs.
    #[test]
    fn card_copies_are_only_kept_for_big_originals() {
        let (tw, th) = (512, 320);
        assert!(worth_caching(4096, 2304, tw, th), "4K is 25x the card");
        assert!(worth_caching(2752, 1536, tw, th), "still 8x");
        assert!(
            !worth_caching(850, 544, tw, th),
            "worth less than decoding it again"
        );
        assert!(!worth_caching(tw, th, tw, th));
    }

    /// A copy that would be an *upscale* of the original is skipped entirely:
    /// Windows scales it just as well, and ours would bake in a second JPEG
    /// generation for nothing.
    #[test]
    fn ready_copy_refuses_to_upscale() {
        let scratch = Scratch::new("no-upscale");
        let src = scratch.image("small.png", 800, 450);

        assert!(
            ready_for(&src, 1920, 1080).is_none(),
            "an upscaling copy is not worth making"
        );
        let down = ready_for(&src, 400, 225).expect("a downscaling copy is");
        assert_eq!(image::image_dimensions(&down).unwrap(), (400, 225));
        remove(&down);
    }

    /// Speculation is bounded, but a card the user actually landed on is always
    /// prepared — that is what makes the bound safe rather than a gamble.
    #[test]
    fn only_nearby_cards_are_prepared_on_spec() {
        assert!(prepares_on_spec(0, false), "the focused card");
        assert!(
            prepares_on_spec(PREPARE_WINDOW - 1, false),
            "the last of the window"
        );
        assert!(!prepares_on_spec(PREPARE_WINDOW, false), "beyond it");
        assert!(!prepares_on_spec(200, false));
        assert!(prepares_on_spec(PREPARE_WINDOW, true), "landed on it");
        assert!(prepares_on_spec(200, true), "however far away it is");
    }
}
