//! Native dark-themed clip gallery: tile view with category tabs, player view with
//! speed control and trim/cut. Runs as its own process (`clipper.exe --gallery`).
//! Everything is custom-painted with GDI (double-buffered); video is rendered by MFPlay.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::c_void,
    fs,
    mem::{size_of, zeroed},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr::{null, null_mut},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};
use windows::{
    core::{PCWSTR, PROPVARIANT},
    Win32::{
        Foundation::{BOOL, HWND as PlayerHwnd, SIZE as WSize},
        Media::MediaFoundation::{
            IMFPMediaPlayer, IMFPMediaPlayerCallback, MFPCreateMediaPlayer, MFP_MEDIAPLAYER_STATE_PAUSED,
            MFP_MEDIAPLAYER_STATE_PLAYING, MFP_OPTION_NONE, MFP_POSITIONTYPE_100NS,
        },
        System::Com::{CoInitializeEx, IBindCtx, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED},
        UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::DwmSetWindowAttribute, Gdi::*},
    System::{Com::CoTaskMemFree, LibraryLoader::GetModuleHandleW, Threading::CreateMutexW, Time::*},
    UI::{
        HiDpi::GetDpiForSystem,
        Input::KeyboardAndMouse::*,
        Shell::{SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW},
        WindowsAndMessaging::*,
    },
};

use super::{hk_name, vk_name, w, Config, Hotkey, CREATE_NO_WINDOW, WM_HK_PAUSE, WM_HK_RESUME, WM_RELOAD};

// ------------------------------------------------------------------ constants

const CLASS: &str = "ClipperGallery";
const VCLASS: &str = "ClipperVideo";
const ICLASS: &str = "ClipperIcon";
const TIMER_TICK: usize = 1;
const TIMER_ICON: usize = 2;
const WM_THUMB: u32 = WM_APP + 2;
const WM_VCLICK: u32 = WM_APP + 3;
const WM_EDITDONE: u32 = WM_APP + 4;
const WM_MOUSELEAVE: u32 = 0x02A3;
const MIN_SEG: i64 = 2_000_000; // 0.2 s
const WM_OPEN_SETTINGS: u32 = WM_APP + 5;
const LENGTHS: [u32; 5] = [15, 30, 60, 120, 300];
const RESS: [u32; 4] = [0, 720, 1080, 1440];
const FPSS: [u32; 4] = [30, 60, 90, 120];
const MBPS: [u32; 6] = [5, 10, 15, 20, 30, 50];
const KEY_TITLES: [&str; 3] = ["Save clip", "Open clips", "Quit Clipper"];
const RATES: [f32; 5] = [0.25, 0.5, 1.0, 1.5, 2.0];

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}
const BG: u32 = rgb(18, 18, 18);
const PANEL: u32 = rgb(27, 27, 27);
const TILE: u32 = rgb(33, 33, 33);
const TILE_HOT: u32 = rgb(46, 46, 46);
const BTN: u32 = rgb(45, 45, 45);
const BTN_HOT: u32 = rgb(66, 66, 66);
const ACCENT: u32 = rgb(59, 130, 246);
const ACCENT_DIM: u32 = rgb(38, 78, 140);
const TEXT: u32 = rgb(235, 235, 235);
const DIM: u32 = rgb(150, 150, 150);
const DANGER: u32 = rgb(176, 56, 56);
const DANGER_HOT: u32 = rgb(210, 70, 70);
const WARN: u32 = rgb(250, 204, 21);

// ------------------------------------------------------------------ state

#[derive(Clone, Copy, PartialEq)]
enum Act {
    None,
    Tab(usize),
    Tile(usize),
    TileSel(usize),
    TileDel(usize),
    SelAll,
    SelNone,
    DeleteSel,
    Gear,
    SetLen(usize),
    SetRes(usize),
    SetFps(usize),
    SetBr(usize),
    Browse,
    Rebind(usize),
    Apply,
    Back,
    Play,
    Seek,
    Speed(usize),
    ModeTrim,
    ModeCut,
    Lane,
    Clear,
    Save,
    Delete,
    Yes,
    No,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Seek,
    Move { idx: usize, off: i64 },
    EdgeL(usize),
    EdgeR(usize),
    Create { anchor: i64, idx: usize },
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Grid,
    Player,
    Settings,
}

struct Clip {
    path: PathBuf,
    stem: String,
    cat: String,
    ts: u64,
    len: u64,
}

#[derive(Clone, Copy)]
struct Fonts {
    normal: HFONT,
    small: HFONT,
    bold: HFONT,
    big: HFONT,
}

#[derive(Clone, PartialEq)]
struct Draft {
    secs: u32,
    height: u32,
    fps: u32,
    mbps: u32,
    out: PathBuf,
    keys: [Hotkey; 3],
}

impl Draft {
    fn from_cfg(c: &Config) -> Draft {
        Draft {
            secs: c.secs,
            height: c.height,
            fps: c.fps,
            mbps: c.bitrate.trim_end_matches(|ch| ch == 'M' || ch == 'm').parse().unwrap_or(15),
            out: c.out.clone(),
            keys: c.keys,
        }
    }
}

struct State {
    draft: Option<Draft>,
    saved: Option<Draft>,
    rebinding: Option<usize>,
    cfg: Option<Config>,
    hwnd: HWND,
    video: HWND,
    icon: HWND,
    fonts: Fonts,
    clips: Vec<Clip>,
    cats: Vec<String>,
    tab: Option<String>, // None = All
    view: View,
    scroll: i32,
    max_scroll: i32,
    cur: Option<PathBuf>,
    player: Option<IMFPMediaPlayer>,
    pos: i64,
    dur: i64,
    playing: bool,
    ready: bool,
    rate_idx: usize,
    rate_ok: bool,
    segs: Vec<(i64, i64)>, // selected areas (100 ns units), kept sorted/merged after each drag
    sel: Option<usize>,
    cut_mode: bool,
    drag: Drag,
    lane_y: (i32, i32),
    regions: Vec<(RECT, Act)>,
    hover: Act,
    tracking: bool,
    seek_x: (i32, i32),
    status: String,
    status_at: Option<Instant>,
    confirm: Option<String>,
    pending_del: Vec<PathBuf>,
    selected: HashSet<PathBuf>,
    mem: HDC,
    bmp: HBITMAP,
    bmp_size: (i32, i32),
}

impl State {
    fn new() -> Self {
        State {
            draft: None,
            saved: None,
            rebinding: None,
            cfg: None,
            hwnd: null_mut(),
            video: null_mut(),
            icon: null_mut(),
            fonts: Fonts { normal: null_mut(), small: null_mut(), bold: null_mut(), big: null_mut() },
            clips: Vec::new(),
            cats: Vec::new(),
            tab: None,
            view: View::Grid,
            scroll: 0,
            max_scroll: 0,
            cur: None,
            player: None,
            pos: 0,
            dur: 0,
            playing: false,
            ready: false,
            rate_idx: 2,
            rate_ok: true,
            segs: Vec::new(),
            sel: None,
            cut_mode: false,
            drag: Drag::None,
            lane_y: (0, 0),
            regions: Vec::new(),
            hover: Act::None,
            tracking: false,
            seek_x: (0, 1),
            status: String::new(),
            status_at: None,
            confirm: None,
            pending_del: Vec::new(),
            selected: HashSet::new(),
            mem: null_mut(),
            bmp: null_mut(),
            bmp_size: (0, 0),
        }
    }
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::new());
    static DPI: Cell<i32> = Cell::new(96);
    static ICON_PLAYING: Cell<bool> = Cell::new(false);
}

// Thumbnails come from a worker thread: path -> (HBITMAP as isize, width, height)
static THUMBS: OnceLock<Mutex<HashMap<PathBuf, (isize, i32, i32)>>> = OnceLock::new();
static ASKED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static EDIT_MSG: Mutex<String> = Mutex::new(String::new());

fn thumbs() -> &'static Mutex<HashMap<PathBuf, (isize, i32, i32)>> {
    THUMBS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn asked() -> &'static Mutex<HashSet<PathBuf>> {
    ASKED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    ST.with(|s| f(&mut s.borrow_mut()))
}

fn sc(v: i32) -> i32 {
    v * DPI.with(|d| d.get()) / 96
}

fn player() -> Option<IMFPMediaPlayer> {
    st(|s| s.player.clone())
}

fn invalidate() {
    let h = st(|s| s.hwnd);
    unsafe { InvalidateRect(h, null(), 0) };
}

fn set_status(msg: &str) {
    st(|s| {
        s.status = msg.to_string();
        s.status_at = Some(Instant::now());
    });
    invalidate();
}

// ------------------------------------------------------------------ formatting / clips

fn fmt_local(ts: u64) -> String {
    unsafe {
        let t = ts * 10_000_000 + 116_444_736_000_000_000;
        let ft = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
        let mut utc: SYSTEMTIME = zeroed();
        let mut loc: SYSTEMTIME = zeroed();
        if FileTimeToSystemTime(&ft, &mut utc) == 0 {
            return String::new();
        }
        SystemTimeToTzSpecificLocalTime(null(), &utc, &mut loc);
        format!("{:04}-{:02}-{:02}  {:02}:{:02}", loc.wYear, loc.wMonth, loc.wDay, loc.wHour, loc.wMinute)
    }
}

fn fmt_time(t100ns: i64) -> String {
    let s = (t100ns / 10_000_000).max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// "<Category>_<YYYY-MM-DD>_<HH-MM-SS>_<id>" -> Category; anything else -> "Other"
fn parse_cat(stem: &str) -> String {
    let p: Vec<&str> = stem.rsplitn(4, '_').collect();
    if p.len() == 4 && p[2].len() == 10 && p[2].as_bytes()[4] == b'-' && !p[3].is_empty() {
        p[3].to_string()
    } else {
        "Other".to_string()
    }
}

fn load_clips(out: &Path) -> Vec<Clip> {
    let mut v: Vec<Clip> = fs::read_dir(out)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().map_or(false, |x| x == "mp4"))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            let ts = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
            let stem = e.path().file_stem()?.to_string_lossy().into_owned();
            Some(Clip { cat: parse_cat(&stem), path: e.path(), stem, ts, len: m.len() })
        })
        .collect();
    v.sort_by(|a, b| b.ts.cmp(&a.ts));
    v
}

fn refresh_clips() {
    let out = st(|s| s.cfg.as_ref().map(|c| c.out.clone()));
    let Some(out) = out else { return };
    let clips = load_clips(&out);
    let mut cats: Vec<String> = Vec::new();
    for c in &clips {
        if !cats.contains(&c.cat) {
            cats.push(c.cat.clone());
        }
    }
    cats.sort_by(|a, b| {
        let rank = |x: &String| if x == "Desktop" { 0 } else if x == "Other" { 2 } else { 1 };
        rank(a).cmp(&rank(b)).then(a.to_lowercase().cmp(&b.to_lowercase()))
    });
    st(|s| {
        let keep: HashSet<PathBuf> = clips.iter().map(|c| c.path.clone()).collect();
        s.selected.retain(|p| keep.contains(p));
        s.clips = clips;
        if let Some(t) = &s.tab {
            if !cats.contains(t) {
                s.tab = None;
            }
        }
        s.cats = cats;
    });
    request_thumbs();
    invalidate();
}

fn visible(s: &State) -> Vec<usize> {
    s.clips
        .iter()
        .enumerate()
        .filter(|(_, c)| s.tab.as_ref().map_or(true, |t| &c.cat == t))
        .map(|(i, _)| i)
        .collect()
}

// ------------------------------------------------------------------ thumbnails (Windows shell)

unsafe fn shell_thumb(p: &Path) -> Option<(isize, i32, i32)> {
    let wide = w(&p.to_string_lossy());
    let f: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>).ok()?;
    let hbm = f.GetImage(WSize { cx: 480, cy: 270 }, SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK).ok()?;
    let mut bm: BITMAP = zeroed();
    GetObjectW(hbm.0 as HANDLE, size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void);
    Some((hbm.0 as isize, bm.bmWidth, bm.bmHeight))
}

fn request_thumbs() {
    let hwnd = st(|s| s.hwnd) as usize;
    let todo: Vec<PathBuf> = {
        let paths: Vec<PathBuf> = st(|s| s.clips.iter().map(|c| c.path.clone()).collect());
        let mut a = asked().lock().unwrap();
        paths.into_iter().filter(|p| a.insert(p.clone())).collect()
    };
    if todo.is_empty() {
        return;
    }
    thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        for p in todo {
            if let Some(t) = shell_thumb(&p) {
                thumbs().lock().unwrap().insert(p, t);
            }
            PostMessageW(hwnd as HWND, WM_THUMB, 0, 0);
        }
    });
}

// ------------------------------------------------------------------ drawing helpers

fn rc(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

unsafe fn fill(hdc: HDC, r: &RECT, c: u32) {
    SetDCBrushColor(hdc, c);
    FillRect(hdc, r, GetStockObject(DC_BRUSH) as HBRUSH);
}

unsafe fn round(hdc: HDC, r: &RECT, c: u32, rad: i32) {
    SetDCBrushColor(hdc, c);
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(NULL_PEN));
    RoundRect(hdc, r.left, r.top, r.right + 1, r.bottom + 1, rad, rad);
}

unsafe fn text(hdc: HDC, s: &str, r: &RECT, c: u32, font: HFONT, flags: u32) {
    SelectObject(hdc, font);
    SetTextColor(hdc, c);
    SetBkMode(hdc, TRANSPARENT as i32);
    let ws: Vec<u16> = s.encode_utf16().collect();
    let mut rr = *r;
    DrawTextW(hdc, ws.as_ptr(), ws.len() as i32, &mut rr, flags | DT_NOPREFIX);
}

unsafe fn text_w(hdc: HDC, font: HFONT, s: &str) -> i32 {
    SelectObject(hdc, font);
    let ws: Vec<u16> = s.encode_utf16().collect();
    let mut sz: SIZE = zeroed();
    GetTextExtentPoint32W(hdc, ws.as_ptr(), ws.len() as i32, &mut sz);
    sz.cx
}

unsafe fn poly(hdc: HDC, pts: &[(i32, i32)], c: u32) {
    SetDCBrushColor(hdc, c);
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(NULL_PEN));
    let p: Vec<POINT> = pts.iter().map(|&(x, y)| POINT { x, y }).collect();
    Polygon(hdc, p.as_ptr(), p.len() as i32);
}

unsafe fn play_glyph(hdc: HDC, cx: i32, cy: i32, size: i32, playing: bool, c: u32) {
    if playing {
        poly(hdc, &[(cx - size / 3, cy - size / 2), (cx - size / 3, cy + size / 2), (cx + size / 2, cy)], c);
    } else {
        let bw = (size / 4).max(2);
        fill(hdc, &rc(cx - size / 3, cy - size / 2, cx - size / 3 + bw, cy + size / 2), c);
        fill(hdc, &rc(cx + size / 3 - bw, cy - size / 2, cx + size / 3, cy + size / 2), c);
    }
}

unsafe fn button(hdc: HDC, s: &mut State, r: RECT, label: &str, act: Act, bg: u32, hot: u32, fg: u32) {
    let c = if s.hover == act { hot } else { bg };
    round(hdc, &r, c, sc(10));
    text(hdc, label, &r, fg, s.fonts.normal, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    s.regions.push((r, act));
}

// ------------------------------------------------------------------ painting

unsafe fn check_mark(hdc: HDC, r: &RECT) {
    let pen = CreatePen(PS_SOLID, sc(3).max(2), rgb(255, 255, 255));
    let old = SelectObject(hdc, pen);
    let (x0, y0, w, h) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
    MoveToEx(hdc, x0 + w * 22 / 100, y0 + h * 52 / 100, null_mut());
    LineTo(hdc, x0 + w * 42 / 100, y0 + h * 72 / 100);
    LineTo(hdc, x0 + w * 78 / 100, y0 + h * 30 / 100);
    SelectObject(hdc, old);
    DeleteObject(pen);
}

unsafe fn paint_grid(hdc: HDC, s: &mut State, w: i32, h: i32) {
    let f = s.fonts;
    let top = sc(56);
    // grid first (so the top bar overdraws any scrolled content)
    let vis = visible(s);
    let margin = sc(20);
    let gap = sc(16);
    let cols = ((w - 2 * margin + gap) / (sc(290) + gap)).max(1);
    let tw = (w - 2 * margin - (cols - 1) * gap) / cols;
    let thh = tw * 9 / 16;
    let info = sc(52);
    let th = thh + info;
    let rows = (vis.len() as i32 + cols - 1) / cols;
    let content = if rows > 0 { rows * (th + gap) - gap + 2 * margin } else { 0 };
    s.max_scroll = (content - (h - top)).max(0);
    s.scroll = s.scroll.clamp(0, s.max_scroll);

    if vis.is_empty() {
        let r = rc(0, top, w, h);
        text(hdc, "No clips here yet. Press Alt+F10 while Clipper is recording.", &r, DIM, f.normal, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    let saved = SaveDC(hdc);
    IntersectClipRect(hdc, 0, top, w, h);
    let tmap = thumbs().lock().unwrap();
    let mem = CreateCompatibleDC(hdc);
    for (n, &ci) in vis.iter().enumerate() {
        let (col, row) = (n as i32 % cols, n as i32 / cols);
        let x = margin + col * (tw + gap);
        let y = top + margin + row * (th + gap) - s.scroll;
        if y + th < top || y > h {
            continue;
        }
        let tile = rc(x, y, x + tw, y + th);
        let is_sel = s.selected.contains(&s.clips[ci].path);
        let sel_mode = !s.selected.is_empty();
        let hot = s.hover == Act::Tile(ci) || s.hover == Act::TileDel(ci) || s.hover == Act::TileSel(ci);
        if is_sel {
            round(hdc, &rc(tile.left - 2, tile.top - 2, tile.right + 2, tile.bottom + 2), ACCENT, sc(14));
        }
        round(hdc, &tile, if is_sel { rgb(38, 50, 72) } else if hot { TILE_HOT } else { TILE }, sc(12));
        // thumbnail area
        let ta = rc(x + sc(6), y + sc(6), x + tw - sc(6), y + thh - sc(2));
        round(hdc, &ta, rgb(8, 8, 8), sc(8));
        let c = &s.clips[ci];
        if let Some(&(hbm, bw, bh)) = tmap.get(&c.path) {
            if bw > 0 && bh > 0 {
                let (aw, ah) = (ta.right - ta.left, ta.bottom - ta.top);
                let sc_f = (aw as f64 / bw as f64).min(ah as f64 / bh as f64);
                let (dw, dh) = ((bw as f64 * sc_f) as i32, (bh as f64 * sc_f) as i32);
                let old = SelectObject(mem, hbm as HGDIOBJ);
                SetStretchBltMode(hdc, HALFTONE);
                StretchBlt(hdc, ta.left + (aw - dw) / 2, ta.top + (ah - dh) / 2, dw, dh, mem, 0, 0, bw, bh, SRCCOPY);
                SelectObject(mem, old);
            }
        } else {
            let (cx, cy) = ((ta.left + ta.right) / 2, (ta.top + ta.bottom) / 2);
            poly(hdc, &[(cx - sc(10), cy - sc(14)), (cx - sc(10), cy + sc(14)), (cx + sc(16), cy)], rgb(70, 70, 70));
        }
        // info
        let title = if c.cat == "Other" { c.stem.clone() } else { c.cat.clone() };
        let l1 = rc(x + sc(12), y + thh, x + tw - sc(12), y + thh + info / 2);
        text(hdc, &title, &l1, TEXT, f.bold, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        let l2 = rc(x + sc(12), y + thh + info / 2 - sc(4), x + tw - sc(12), y + th - sc(2));
        let sub = format!("{}   {:.1} MB", fmt_local(c.ts), c.len as f64 / 1048576.0);
        text(hdc, &sub, &l2, DIM, f.small, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        s.regions.push((tile, Act::Tile(ci)));
        if hot || sel_mode {
            // selection checkbox, top-left of the thumbnail
            let cb = rc(ta.left + sc(8), ta.top + sc(8), ta.left + sc(8) + sc(26), ta.top + sc(8) + sc(26));
            round(hdc, &cb, rgb(235, 235, 235), sc(8));
            let inner = rc(cb.left + 2, cb.top + 2, cb.right - 2, cb.bottom - 2);
            if is_sel {
                round(hdc, &inner, ACCENT, sc(7));
                check_mark(hdc, &inner);
            } else {
                round(hdc, &inner, if s.hover == Act::TileSel(ci) { rgb(70, 70, 70) } else { rgb(24, 24, 24) }, sc(7));
            }
            s.regions.push((rc(cb.left - sc(4), cb.top - sc(4), cb.right + sc(4), cb.bottom + sc(4)), Act::TileSel(ci)));
        }
        if hot {
            let d = rc(ta.right - sc(34), ta.top + sc(6), ta.right - sc(6), ta.top + sc(34));
            let dh = s.hover == Act::TileDel(ci);
            round(hdc, &d, if dh { DANGER_HOT } else { rgb(0, 0, 0) }, sc(8));
            text(hdc, "\u{00D7}", &d, TEXT, f.big, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            s.regions.push((d, Act::TileDel(ci)));
        }
    }
    DeleteDC(mem);
    drop(tmap);
    RestoreDC(hdc, saved);

    // top bar with category tabs
    fill(hdc, &rc(0, 0, w, top), PANEL);
    s.regions.push((rc(0, 0, w, top), Act::None)); // blocks clicks on tiles scrolled under the bar
    let title = rc(sc(20), 0, sc(120), top);
    text(hdc, "Clipper", &title, TEXT, f.big, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    let nsel = s.selected.len();
    let mut x = sc(130);
    let mut labels: Vec<(String, Option<String>, usize)> = Vec::new();
    if nsel > 0 {
        text(hdc, &format!("{nsel} selected"), &rc(x, 0, x + sc(120), top), TEXT, f.bold, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        x += sc(130);
        let items: [(&str, Act, u32, u32); 3] = [
            ("Select all", Act::SelAll, BTN, BTN_HOT),
            ("Clear", Act::SelNone, BTN, BTN_HOT),
            ("Delete selected", Act::DeleteSel, DANGER, DANGER_HOT),
        ];
        for (label, act, bg, hot) in items {
            let bw = text_w(hdc, f.normal, label) + sc(28);
            button(hdc, s, rc(x, sc(10), x + bw, top - sc(10)), label, act, bg, hot, TEXT);
            x += bw + sc(8);
        }
    } else {
        labels.push(("All".to_string(), None, 0));
        for (i, c) in s.cats.iter().enumerate() {
            labels.push((c.clone(), Some(c.clone()), i + 1));
        }
    }
    for (label, val, idx) in labels {
        let tw2 = text_w(hdc, f.normal, &label) + sc(28);
        if x + tw2 > w - sc(250) {
            break;
        }
        let r = rc(x, sc(12), x + tw2, top - sc(12));
        let sel = s.tab == val;
        let bg = if sel { ACCENT } else if s.hover == Act::Tab(idx) { BTN_HOT } else { BTN };
        round(hdc, &r, bg, sc(16));
        text(hdc, &label, &r, TEXT, f.normal, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        s.regions.push((r, Act::Tab(idx)));
        x += tw2 + sc(8);
    }
    button(hdc, s, rc(w - sc(116), sc(12), w - sc(16), top - sc(12)), "Settings", Act::Gear, BTN, BTN_HOT, TEXT);
    let cnt = rc(w - sc(230), 0, w - sc(128), top);
    text(hdc, &format!("{} clips", vis.len()), &cnt, DIM, f.small, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    // scroll indicator
    if s.max_scroll > 0 {
        let track = h - top - 2 * sc(8);
        let bar = (track as f64 * (h - top) as f64 / (content as f64)).max(sc(30) as f64) as i32;
        let by = top + sc(8) + ((track - bar) as f64 * s.scroll as f64 / s.max_scroll as f64) as i32;
        round(hdc, &rc(w - sc(8), by, w - sc(4), by + bar), rgb(80, 80, 80), sc(4));
    }
}

unsafe fn paint_player(hdc: HDC, s: &mut State, w: i32, h: i32) {
    let f = s.fonts;
    let top = sc(52);
    let ctrl = ctrl_h();
    fill(hdc, &rc(0, 0, w, top), PANEL);
    button(hdc, s, rc(sc(10), sc(9), sc(112), top - sc(9)), "\u{2190}  Back", Act::Back, BTN, BTN_HOT, TEXT);
    let name = s
        .cur
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    text(hdc, &name, &rc(sc(128), 0, w - sc(360), top), TEXT, f.bold, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    if let Some(t) = s.status_at {
        if t.elapsed() < Duration::from_secs(6) && !s.status.is_empty() {
            text(hdc, &s.status, &rc(w - sc(350), 0, w - sc(16), top), WARN, f.normal, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        }
    }

    let y0 = h - ctrl;
    fill(hdc, &rc(0, y0, w, h), PANEL);
    let (col, col_dim) = if s.cut_mode { (DANGER, rgb(110, 40, 40)) } else { (ACCENT, ACCENT_DIM) };

    // --- seek bar
    let (bx0, bx1) = (sc(78), w - sc(78));
    let ty = y0 + sc(24);
    text(hdc, &fmt_time(s.pos), &rc(sc(12), y0 + sc(8), bx0 - sc(6), y0 + sc(40)), DIM, f.small, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
    text(hdc, &fmt_time(s.dur), &rc(bx1 + sc(8), y0 + sc(8), w - sc(8), y0 + sc(40)), DIM, f.small, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    round(hdc, &rc(bx0, ty, bx1, ty + sc(6)), rgb(70, 70, 70), sc(6));
    s.seek_x = (bx0, bx1);
    let dur = s.dur;
    if dur > 0 {
        let x_of = |t: i64| bx0 + (((bx1 - bx0) as i64) * t.clamp(0, dur) / dur) as i32;
        for &(a, b) in &s.segs {
            fill(hdc, &rc(x_of(a), ty, x_of(b).max(x_of(a) + 1), ty + sc(6)), col_dim);
        }
        let px = x_of(s.pos);
        if px > bx0 {
            // progress, drawn thin so selections stay visible underneath
            round(hdc, &rc(bx0, ty + sc(2), px, ty + sc(4)), TEXT, sc(2));
        }
        round(hdc, &rc(px - sc(7), ty - sc(4), px + sc(7), ty + sc(10)), TEXT, sc(14));
    }
    s.regions.push((rc(bx0 - sc(8), y0 + sc(8), bx1 + sc(8), y0 + sc(42)), Act::Seek));

    // --- selection lane: drag on empty space to add an area, drag edges/body to adjust
    let (ly0, ly1) = (y0 + sc(48), y0 + sc(48) + sc(28));
    s.lane_y = (ly0, ly1);
    round(hdc, &rc(bx0, ly0, bx1, ly1), rgb(36, 36, 36), sc(8));
    if s.segs.is_empty() {
        let hint = if s.cut_mode { "Drag here to select areas to remove" } else { "Drag here to select areas to keep" };
        text(hdc, hint, &rc(bx0, ly0, bx1, ly1), rgb(110, 110, 110), f.small, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    if dur > 0 {
        let x_of = |t: i64| bx0 + (((bx1 - bx0) as i64) * t.clamp(0, dur) / dur) as i32;
        for (i, &(a, b)) in s.segs.iter().enumerate() {
            let xl = x_of(a);
            let xr = x_of(b).max(xl + sc(6));
            let r = rc(xl, ly0 + sc(3), xr, ly1 - sc(3));
            if s.sel == Some(i) {
                round(hdc, &rc(r.left - 2, r.top - 2, r.right + 2, r.bottom + 2), WARN, sc(8));
            }
            round(hdc, &r, col, sc(6));
            fill(hdc, &rc(r.left + sc(2), r.top + sc(5), r.left + sc(5), r.bottom - sc(5)), rgb(240, 240, 240));
            fill(hdc, &rc(r.right - sc(5), r.top + sc(5), r.right - sc(2), r.bottom - sc(5)), rgb(240, 240, 240));
            if r.right - r.left > sc(70) {
                text(hdc, &fmt_time(b - a), &rc(r.left + sc(8), r.top, r.right - sc(8), r.bottom), TEXT, f.small, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
        }
    }
    s.regions.push((rc(bx0 - sc(8), ly0, bx1 + sc(8), ly1), Act::Lane));

    // --- controls row
    let ry = y0 + sc(94);
    let bh = sc(40);
    let pb = rc(sc(16), ry, sc(16) + sc(54), ry + bh);
    round(hdc, &pb, if s.hover == Act::Play { BTN_HOT } else { BTN }, sc(10));
    play_glyph(hdc, (pb.left + pb.right) / 2, (pb.top + pb.bottom) / 2, sc(16), !s.playing, TEXT);
    s.regions.push((pb, Act::Play));

    let mut x = pb.right + sc(22);
    text(hdc, "Speed", &rc(x, ry, x + sc(48), ry + bh), DIM, f.small, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    x += sc(50);
    for (i, r) in RATES.iter().enumerate() {
        let pr = rc(x, ry + sc(4), x + sc(52), ry + bh - sc(4));
        let sel = s.rate_idx == i;
        let bg = if sel { ACCENT } else if s.hover == Act::Speed(i) { BTN_HOT } else { BTN };
        round(hdc, &pr, bg, sc(14));
        text(hdc, &format!("{}x", r), &pr, TEXT, f.small, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        s.regions.push((pr, Act::Speed(i)));
        x += sc(58);
    }

    // edit controls, right-aligned: [Trim (keep)] [Cut (remove)] [Clear] [Save] ... [Delete]
    let save_l = if s.cut_mode { "Save cut" } else { "Save trim" };
    let (trim_bg, cut_bg) = if s.cut_mode { (BTN, DANGER) } else { (ACCENT, BTN) };
    let items: [(&str, Act, u32, u32); 5] = [
        ("Delete clip", Act::Delete, DANGER, DANGER_HOT),
        (save_l, Act::Save, col_dim, col),
        ("Clear", Act::Clear, BTN, BTN_HOT),
        ("Cut (remove)", Act::ModeCut, cut_bg, if s.cut_mode { DANGER_HOT } else { BTN_HOT }),
        ("Trim (keep)", Act::ModeTrim, trim_bg, if s.cut_mode { BTN_HOT } else { rgb(90, 160, 255) }),
    ];
    let mut rx = w - sc(16);
    for (label, act, bg, hot) in items {
        let bw = text_w(hdc, f.normal, label) + sc(30);
        let r = rc(rx - bw, ry, rx, ry + bh);
        button(hdc, s, r, label, act, bg, hot, TEXT);
        rx -= bw + if act == Act::Delete { sc(28) } else { sc(8) };
    }
}

unsafe fn paint_modal(hdc: HDC, s: &mut State, w: i32, h: i32) {
    let Some(msg) = s.confirm.clone() else { return };
    let f = s.fonts;
    s.regions.clear();
    let (bw, bh) = (sc(420), sc(170));
    let r = rc((w - bw) / 2, (h - bh) / 2, (w + bw) / 2, (h + bh) / 2);
    round(hdc, &rc(r.left - 2, r.top - 2, r.right + 2, r.bottom + 2), rgb(70, 70, 70), sc(14));
    round(hdc, &r, rgb(36, 36, 36), sc(12));
    text(hdc, &msg, &rc(r.left + sc(20), r.top + sc(16), r.right - sc(20), r.top + sc(90)), TEXT, f.normal, DT_CENTER | DT_VCENTER | DT_WORDBREAK);
    let by = r.bottom - sc(60);
    button(hdc, s, rc(r.left + sc(40), by, r.left + sc(190), by + sc(40)), "Delete", Act::Yes, DANGER, DANGER_HOT, TEXT);
    button(hdc, s, rc(r.right - sc(190), by, r.right - sc(40), by + sc(40)), "Cancel", Act::No, BTN, BTN_HOT, TEXT);
}

unsafe fn on_paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut client: RECT = zeroed();
    GetClientRect(hwnd, &mut client);
    let (w, h) = (client.right, client.bottom);
    st(|s| {
        if s.mem.is_null() {
            s.mem = CreateCompatibleDC(hdc);
        }
        if s.bmp.is_null() || s.bmp_size != (w, h) {
            let nb = CreateCompatibleBitmap(hdc, w.max(1), h.max(1));
            SelectObject(s.mem, nb);
            if !s.bmp.is_null() {
                DeleteObject(s.bmp);
            }
            s.bmp = nb;
            s.bmp_size = (w, h);
        }
        let mem = s.mem;
        fill(mem, &client, BG);
        s.regions.clear();
        match s.view {
            View::Grid => paint_grid(mem, s, w, h),
            View::Player => paint_player(mem, s, w, h),
            View::Settings => paint_settings(mem, s, w, h),
        }
        paint_modal(mem, s, w, h);
        let p = ps.rcPaint;
        BitBlt(hdc, p.left, p.top, p.right - p.left, p.bottom - p.top, mem, p.left, p.top, SRCCOPY);
    });
    EndPaint(hwnd, &ps);
}

// ------------------------------------------------------------------ player control

fn ctrl_h() -> i32 {
    sc(150)
}

fn layout() {
    let (hwnd, video, icon, view, modal) = st(|s| (s.hwnd, s.video, s.icon, s.view, s.confirm.is_some()));
    unsafe {
        let mut r: RECT = zeroed();
        GetClientRect(hwnd, &mut r);
        // the video is a separate window drawn above the parent, so it must be hidden
        // while the confirmation box (painted on the parent) is showing
        if view == View::Player && !modal {
            let (top, ctrl) = (sc(52), ctrl_h());
            MoveWindow(video, 0, top, r.right, (r.bottom - top - ctrl).max(10), 1);
            ShowWindow(video, SW_SHOWNA);
        } else {
            ShowWindow(video, SW_HIDE);
            ShowWindow(icon, SW_HIDE);
        }
    }
    if let Some(p) = player() {
        unsafe {
            let _ = p.UpdateVideo();
        }
    }
}

fn stop_player() {
    let p = st(|s| s.player.take());
    if let Some(p) = p {
        unsafe {
            let _ = p.Shutdown();
        }
    }
}

fn start_player(path: &Path) {
    let video = st(|s| s.video);
    let wide = w(&path.to_string_lossy());
    let mut p: Option<IMFPMediaPlayer> = None;
    let r = unsafe {
        MFPCreateMediaPlayer(
            PCWSTR(wide.as_ptr()),
            BOOL(1),
            MFP_OPTION_NONE,
            None::<&IMFPMediaPlayerCallback>,
            PlayerHwnd(video as isize),
            Some(&mut p),
        )
    };
    match (r, p) {
        (Ok(()), Some(p)) => st(|s| s.player = Some(p)),
        _ => set_status("Windows could not play this clip."),
    }
}

fn open_clip(ci: usize) {
    let Some(path) = st(|s| s.clips.get(ci).map(|c| c.path.clone())) else { return };
    stop_player();
    st(|s| {
        s.view = View::Player;
        s.cur = Some(path.clone());
        s.pos = 0;
        s.dur = 0;
        s.ready = false;
        s.rate_ok = false;
        s.segs.clear();
        s.sel = None;
        s.drag = Drag::None;
        s.playing = true;
        s.status.clear();
    });
    layout();
    start_player(&path);
    invalidate();
}

fn go_back() {
    set_rebinding(None);
    stop_player();
    st(|s| {
        s.view = View::Grid;
        s.cur = None;
        s.drag = Drag::None;
        s.segs.clear();
        s.sel = None;
    });
    layout();
    refresh_clips();
}

fn flash_icon(playing: bool) {
    let (hwnd, icon) = st(|s| (s.hwnd, s.icon));
    ICON_PLAYING.with(|c| c.set(playing));
    unsafe {
        let z = sc(44);
        SetWindowPos(icon, HWND_TOP, sc(16), sc(52) + sc(16), z, z, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        InvalidateRect(icon, null(), 1);
        SetTimer(hwnd, TIMER_ICON, 900, None);
    }
}

fn toggle_play() {
    if let Some(p) = player() {
        unsafe {
            match p.GetState() {
                Ok(s) if s == MFP_MEDIAPLAYER_STATE_PLAYING => {
                    let _ = p.Pause();
                    st(|s| s.playing = false);
                    flash_icon(false);
                    invalidate();
                    return;
                }
                Ok(s) if s == MFP_MEDIAPLAYER_STATE_PAUSED => {
                    let _ = p.Play();
                    st(|s| s.playing = true);
                    flash_icon(true);
                    invalidate();
                    return;
                }
                _ => {}
            }
        }
    }
    // ended or never loaded: restart the current clip
    let idx = st(|s| s.cur.clone().and_then(|c| s.clips.iter().position(|x| x.path == c)));
    if let Some(i) = idx {
        open_clip(i);
        flash_icon(true);
    }
}

fn set_pos(ns: i64) {
    let Some(p) = player() else { return };
    let dur = st(|s| s.dur);
    let ns = if dur > 0 { ns.clamp(0, dur) } else { ns.max(0) };
    unsafe {
        let _ = p.SetPosition(&MFP_POSITIONTYPE_100NS, &PROPVARIANT::from(ns));
    }
    st(|s| s.pos = ns);
    invalidate();
}

fn seek_to_x(x: i32) {
    let (bx0, bx1, dur) = st(|s| (s.seek_x.0, s.seek_x.1, s.dur));
    if dur <= 0 || bx1 <= bx0 {
        return;
    }
    let f = ((x - bx0) as f64 / (bx1 - bx0) as f64).clamp(0.0, 1.0);
    set_pos((dur as f64 * f) as i64);
}

fn set_rate(i: usize) {
    st(|s| {
        s.rate_idx = i;
        s.rate_ok = false;
    });
    invalidate();
}

fn tick() {
    let (p, view) = st(|s| (s.player.clone(), s.view));
    // expire the status line
    let expired = st(|s| s.status_at.map_or(false, |t| t.elapsed() > Duration::from_secs(6)) && !s.status.is_empty());
    if expired {
        st(|s| s.status.clear());
        invalidate();
    }
    if view != View::Player {
        return;
    }
    let Some(p) = p else { return };
    unsafe {
        let get = |r: windows::core::Result<PROPVARIANT>| r.ok().and_then(|v| i64::try_from(&v).ok());
        let dur = get(p.GetDuration(&MFP_POSITIONTYPE_100NS)).unwrap_or(0);
        let pos = get(p.GetPosition(&MFP_POSITIONTYPE_100NS)).unwrap_or(0);
        let playing = matches!(p.GetState(), Ok(s) if s == MFP_MEDIAPLAYER_STATE_PLAYING);
        let (rate_idx, rate_ok, dragging) = st(|s| (s.rate_idx, s.rate_ok, s.drag == Drag::Seek));
        let mut ok = rate_ok;
        let mut rate_failed = false;
        if dur > 0 && !rate_ok {
            ok = true; // try once per clip/selection
            rate_failed = p.SetRate(RATES[rate_idx]).is_err() && RATES[rate_idx] != 1.0;
        }
        let changed = st(|s| {
            let c = s.dur != dur || (!dragging && s.pos != pos) || s.playing != playing;
            s.dur = dur;
            if !dragging {
                s.pos = pos;
            }
            s.playing = playing;
            s.rate_ok = ok;
            c
        });
        if rate_failed {
            set_status("That speed isn't supported for this clip.");
        }
        if changed {
            invalidate();
        }
    }
}

// ------------------------------------------------------------------ delete / edit

fn ask_delete(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let msg = if paths.len() == 1 {
        let name = paths[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        format!("Delete this clip permanently?\n{name}")
    } else {
        format!("Delete {} clips permanently?", paths.len())
    };
    st(|s| {
        s.confirm = Some(msg);
        s.pending_del = paths;
    });
    layout();
    invalidate();
}

fn do_delete() {
    let paths = st(|s| {
        s.confirm = None;
        std::mem::take(&mut s.pending_del)
    });
    if paths.is_empty() {
        return;
    }
    if st(|s| s.view == View::Player && s.cur.as_ref().map_or(false, |c| paths.contains(c))) {
        go_back();
    }
    layout();
    for path in &paths {
        for _ in 0..10 {
            if fs::remove_file(path).is_ok() || !path.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if let Some((hbm, _, _)) = thumbs().lock().unwrap().remove(path) {
            unsafe { DeleteObject(hbm as HGDIOBJ) };
        }
        asked().lock().unwrap().remove(path);
    }
    st(|s| {
        for p in &paths {
            s.selected.remove(p);
        }
    });
    refresh_clips();
}

fn toggle_sel(ci: usize) {
    st(|s| {
        if let Some(c) = s.clips.get(ci) {
            let p = c.path.clone();
            if !s.selected.remove(&p) {
                s.selected.insert(p);
            }
        }
    });
    invalidate();
}

fn fsec(t: i64) -> String {
    format!("{:.3}", t as f64 / 10_000_000.0)
}

fn has_audio(cfg: &Config, src: &Path) -> bool {
    Command::new(&cfg.ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(src)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_or(false, |o| String::from_utf8_lossy(&o.stderr).contains("Audio:"))
}

/// Encoders to try for an edit, best first (the last one, libx264, works everywhere).
fn encoders(cfg: &Config) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if cfg.encoder != "auto" {
        v.push(cfg.encoder.clone());
    }
    for e in ["h264_nvenc", "h264_amf", "h264_qsv", "libx264"] {
        if !v.iter().any(|x| x == e) {
            v.push(e.to_string());
        }
    }
    v
}

fn venc(cfg: &Config, enc: &str) -> Vec<String> {
    if enc == "libx264" {
        vec!["-c:v".into(), "libx264".into(), "-preset".into(), "veryfast".into(), "-b:v".into(), cfg.bitrate.clone()]
    } else {
        let mut v = vec!["-c:v".to_string(), enc.to_string(), "-b:v".into(), cfg.bitrate.clone()];
        if enc.contains("nvenc") {
            v.extend(["-preset".to_string(), "p4".to_string()]);
        }
        v
    }
}

/// keep = [a, b] (trim), or cut_mid = true to remove [a, b] instead. Times in 100 ns units.
/// Keep the given ranges (100 ns units) and join them into one new clip.
fn run_edit(cfg: &Config, src: &Path, out: &Path, keep: &[(i64, i64)]) -> bool {
    if keep.is_empty() {
        return false;
    }
    let audio = has_audio(cfg, src);
    for enc in encoders(cfg) {
        let mut cmd = Command::new(&cfg.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if keep.len() == 1 {
            let (a, b) = keep[0];
            cmd.args(["-ss", &fsec(a), "-i"]).arg(src).args(["-t", &fsec(b - a)]);
            cmd.args(venc(cfg, &enc));
            cmd.args(["-c:a", "copy"]);
        } else {
            let mut f = String::new();
            let mut cat = String::new();
            for (i, &(a, b)) in keep.iter().enumerate() {
                let (sa, sb) = (fsec(a), fsec(b));
                f += &format!("[0:v]trim=start={sa}:end={sb},setpts=PTS-STARTPTS[v{i}];");
                cat += &format!("[v{i}]");
                if audio {
                    f += &format!("[0:a]atrim=start={sa}:end={sb},asetpts=PTS-STARTPTS[a{i}];");
                    cat += &format!("[a{i}]");
                }
            }
            f += &format!("{cat}concat=n={}:v=1:a={}[v]", keep.len(), if audio { 1 } else { 0 });
            if audio {
                f += "[a]";
            }
            cmd.arg("-i").arg(src).args(["-filter_complex", &f, "-map", "[v]"]);
            if audio {
                cmd.args(["-map", "[a]"]);
            }
            cmd.args(venc(cfg, &enc));
            if audio {
                cmd.args(["-c:a", "aac", "-b:a", "160k"]);
            }
        }
        let ok = cmd
            .args(["-movflags", "+faststart"])
            .arg(out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_or(false, |s| s.success());
        if ok {
            return true;
        }
        let _ = fs::remove_file(out);
    }
    false
}

fn new_edit_path(src: &Path) -> PathBuf {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    loop {
        let id = RandomState::new().build_hasher().finish() & 0xFFFF;
        let p = src.with_file_name(format!("{stem}-e{id:04x}.mp4"));
        if !p.exists() {
            return p;
        }
    }
}

fn start_edit() {
    let (cfg, src, dur, segs, cut, hwnd) =
        st(|s| (s.cfg.clone(), s.cur.clone(), s.dur, s.segs.clone(), s.cut_mode, s.hwnd));
    let (Some(cfg), Some(src)) = (cfg, src) else { return };
    if dur <= 0 {
        return;
    }
    if segs.is_empty() {
        set_status("Drag on the bar under the timeline to select areas first.");
        return;
    }
    // segments are sorted and merged; build the list of ranges to keep
    let keep: Vec<(i64, i64)> = if cut {
        let mut v = Vec::new();
        let mut cur = 0i64;
        for &(a, b) in &segs {
            if a > cur {
                v.push((cur, a));
            }
            cur = cur.max(b);
        }
        if cur < dur {
            v.push((cur, dur));
        }
        v
    } else {
        segs.iter().map(|&(a, b)| (a.max(0), b.min(dur))).collect()
    };
    let keep: Vec<(i64, i64)> = keep.into_iter().filter(|&(a, b)| b - a >= 1_000_000).collect();
    if keep.is_empty() {
        set_status("Nothing would be left of the clip.");
        return;
    }
    if keep.len() == 1 && keep[0].0 <= 1_000_000 && keep[0].1 >= dur - 1_000_000 {
        set_status("Nothing to change.");
        return;
    }
    let out = new_edit_path(&src);
    set_status(if cut { "Cutting\u{2026}" } else { "Trimming\u{2026}" });
    let hw = hwnd as usize;
    thread::spawn(move || {
        let ok = run_edit(&cfg, &src, &out, &keep);
        let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        *EDIT_MSG.lock().unwrap() = if ok { format!("Saved {name}") } else { "Edit failed (is ffmpeg available?)".to_string() };
        unsafe { PostMessageW(hw as HWND, WM_EDITDONE, 0, 0) };
    });
}

fn t_to_x(s: &State, t: i64) -> i32 {
    let (a, b) = s.seek_x;
    if s.dur <= 0 {
        return a;
    }
    a + (((b - a) as i64) * t.clamp(0, s.dur) / s.dur) as i32
}

fn x_to_t(s: &State, x: i32) -> i64 {
    let (a, b) = s.seek_x;
    if b <= a {
        return 0;
    }
    (s.dur as f64 * ((x - a) as f64 / (b - a) as f64).clamp(0.0, 1.0)) as i64
}

/// Sort and union overlapping/touching segments.
fn merge_segs(s: &mut State) {
    let sel_start = s.sel.and_then(|i| s.segs.get(i)).map(|x| x.0);
    let mut v = std::mem::take(&mut s.segs);
    v.sort();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (a, b) in v {
        if let Some(l) = out.last_mut() {
            if a <= l.1 {
                l.1 = l.1.max(b);
                continue;
            }
        }
        out.push((a, b));
    }
    s.segs = out;
    s.sel = sel_start.and_then(|t| s.segs.iter().position(|x| x.0 <= t && t <= x.1));
}

fn lane_press(x: i32) {
    let edge = sc(8);
    st(|s| {
        if s.dur <= 0 {
            return;
        }
        let t = x_to_t(s, x);
        let mut found: Option<(usize, Drag)> = None;
        for i in (0..s.segs.len()).rev() {
            let (a, b) = s.segs[i];
            let (xl, xr) = (t_to_x(s, a), t_to_x(s, b));
            if (x - xl).abs() <= edge {
                found = Some((i, Drag::EdgeL(i)));
            } else if (x - xr).abs() <= edge {
                found = Some((i, Drag::EdgeR(i)));
            } else if x > xl && x < xr {
                found = Some((i, Drag::Move { idx: i, off: t - a }));
            }
            if found.is_some() {
                break;
            }
        }
        match found {
            Some((i, d)) => {
                s.sel = Some(i);
                s.drag = d;
            }
            None => {
                s.segs.push((t, t));
                let idx = s.segs.len() - 1;
                s.sel = Some(idx);
                s.drag = Drag::Create { anchor: t, idx };
            }
        }
    });
    let h = st(|s| s.hwnd);
    unsafe { SetCapture(h) };
    invalidate();
}

fn drag_update(s: &mut State, x: i32) {
    let t = x_to_t(s, x);
    let dur = s.dur;
    match s.drag {
        Drag::Move { idx, off } => {
            if let Some(seg) = s.segs.get_mut(idx) {
                let len = seg.1 - seg.0;
                let a = (t - off).clamp(0, (dur - len).max(0));
                *seg = (a, a + len);
            }
        }
        Drag::EdgeL(idx) => {
            if let Some(seg) = s.segs.get_mut(idx) {
                seg.0 = t.clamp(0, (seg.1 - MIN_SEG).max(0));
            }
        }
        Drag::EdgeR(idx) => {
            if let Some(seg) = s.segs.get_mut(idx) {
                seg.1 = t.clamp((seg.0 + MIN_SEG).min(dur), dur);
            }
        }
        Drag::Create { anchor, idx } => {
            if let Some(seg) = s.segs.get_mut(idx) {
                *seg = (anchor.min(t), anchor.max(t));
            }
        }
        _ => {}
    }
}

fn finish_drag() {
    let had = st(|s| {
        let d = s.drag;
        s.drag = Drag::None;
        if let Drag::Create { idx, .. } = d {
            if let Some(&(a, b)) = s.segs.get(idx) {
                if b - a < MIN_SEG {
                    s.segs.remove(idx);
                    s.sel = None;
                }
            }
        }
        if d != Drag::None {
            merge_segs(s);
        }
        d != Drag::None
    });
    unsafe { ReleaseCapture() }; // always, so a click before the clip has loaded can't leave the mouse captured
    if had {
        invalidate();
    }
}

fn lane_remove_at(x: i32, y: i32) {
    let removed = st(|s| {
        if s.view != View::Player || y < s.lane_y.0 || y > s.lane_y.1 {
            return false;
        }
        for i in (0..s.segs.len()).rev() {
            let (a, b) = s.segs[i];
            if x >= t_to_x(s, a) && x <= t_to_x(s, b) {
                s.segs.remove(i);
                s.sel = None;
                return true;
            }
        }
        false
    });
    if removed {
        invalidate();
    }
}

/// 0 = nothing, 1 = edge (resize), 2 = body (move)
fn lane_cursor_kind(x: i32, y: i32) -> u8 {
    st(|s| {
        match s.drag {
            Drag::EdgeL(_) | Drag::EdgeR(_) | Drag::Create { .. } => return 1,
            Drag::Move { .. } => return 2,
            _ => {}
        }
        if s.view != View::Player || s.confirm.is_some() || y < s.lane_y.0 || y > s.lane_y.1 {
            return 0;
        }
        let edge = sc(8);
        for &(a, b) in s.segs.iter().rev() {
            let (xl, xr) = (t_to_x(s, a), t_to_x(s, b));
            if (x - xl).abs() <= edge || (x - xr).abs() <= edge {
                return 1;
            }
            if x > xl && x < xr {
                return 2;
            }
        }
        0
    })
}

// ------------------------------------------------------------------ settings

fn post_tray(msg: u32) {
    unsafe {
        let h = FindWindowW(w("ClipperWnd").as_ptr(), null());
        if !h.is_null() {
            PostMessageW(h, msg, 0, 0);
        }
    }
}

/// While a hotkey is being captured the tray's global hotkeys are paused, so pressing an
/// already-bound combination reaches this window instead of triggering its action.
fn set_rebinding(v: Option<usize>) {
    let was = st(|s| std::mem::replace(&mut s.rebinding, v).is_some());
    if was != v.is_some() {
        post_tray(if v.is_some() { WM_HK_PAUSE } else { WM_HK_RESUME });
    }
    invalidate();
}

fn open_settings() {
    stop_player();
    st(|s| {
        let d = s.cfg.as_ref().map(Draft::from_cfg);
        s.draft = d.clone();
        s.saved = d;
        s.view = View::Settings;
        s.scroll = 0;
        s.cur = None;
        s.confirm = None;
    });
    set_rebinding(None);
    layout();
    invalidate();
}

fn apply_settings() {
    set_rebinding(None);
    let (cfg, d, saved) = st(|s| (s.cfg.clone(), s.draft.clone(), s.saved.clone()));
    let (Some(mut cfg), Some(d)) = (cfg, d) else { return };
    if saved.as_ref() == Some(&d) {
        set_status("No changes to apply.");
        return;
    }
    let out_changed = cfg.out != d.out;
    cfg.secs = d.secs;
    cfg.height = d.height;
    cfg.fps = d.fps;
    cfg.bitrate = format!("{}M", d.mbps);
    cfg.out = d.out.clone();
    cfg.keys = d.keys;
    if fs::create_dir_all(&cfg.out).is_err() {
        set_status("Couldn't create that save folder.");
        return;
    }
    if !cfg.save() {
        set_status("Couldn't write clipper.cfg.");
        return;
    }
    st(|s| {
        s.cfg = Some(cfg);
        s.saved = Some(d);
    });
    post_tray(WM_RELOAD);
    if out_changed {
        refresh_clips();
    }
    set_status("Applied. Recording restarted.");
}

fn browse_folder() {
    let hwnd = st(|s| s.hwnd);
    let picked = unsafe {
        let mut display = [0u16; 260];
        let title = w("Choose where to save clips");
        let mut bi: BROWSEINFOW = zeroed();
        bi.hwndOwner = hwnd;
        bi.pszDisplayName = display.as_mut_ptr();
        bi.lpszTitle = title.as_ptr();
        bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            None
        } else {
            let mut path = [0u16; 520];
            let ok = SHGetPathFromIDListW(pidl, path.as_mut_ptr()) != 0;
            CoTaskMemFree(pidl as *const c_void);
            if ok {
                let len = path.iter().position(|&c| c == 0).unwrap_or(0);
                Some(PathBuf::from(String::from_utf16_lossy(&path[..len])))
            } else {
                None
            }
        }
    };
    if let Some(p) = picked {
        st(|s| {
            if let Some(d) = s.draft.as_mut() {
                d.out = p;
            }
        });
        invalidate();
    }
}

fn capture_key(vk: u32) {
    let k = vk as u16;
    if matches!(k, VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL | VK_RCONTROL | VK_LMENU | VK_RMENU) {
        return; // wait for the real key
    }
    if k == VK_ESCAPE {
        set_rebinding(None);
        return;
    }
    if vk_name(vk).is_none() {
        set_status("That key can't be used as a hotkey.");
        return;
    }
    let down = |key: u16| unsafe { (GetKeyState(key as i32) as u16) & 0x8000 != 0 };
    let mut mods = 0u32;
    if down(VK_MENU) {
        mods |= MOD_ALT;
    }
    if down(VK_CONTROL) {
        mods |= MOD_CONTROL;
    }
    if down(VK_SHIFT) {
        mods |= MOD_SHIFT;
    }
    if down(VK_LWIN) || down(VK_RWIN) {
        mods |= MOD_WIN;
    }
    let hk = Hotkey { mods, vk };
    let conflict = st(|s| {
        let i = s.rebinding?;
        let d = s.draft.as_ref()?;
        d.keys.iter().enumerate().find(|(j, kk)| *j != i && **kk == hk).map(|(j, _)| j)
    });
    if let Some(j) = conflict {
        set_status(&format!("{} is already used by \"{}\".", hk_name(&hk), KEY_TITLES[j]));
        return;
    }
    st(|s| {
        if let (Some(i), Some(d)) = (s.rebinding, s.draft.as_mut()) {
            d.keys[i] = hk;
        }
    });
    set_rebinding(None);
}

unsafe fn row_label(hdc: HDC, f: &Fonts, y: i32, title: &str, sub: &str) {
    text(hdc, title, &rc(sc(40), y, sc(250), y + sc(20)), TEXT, f.bold, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    if !sub.is_empty() {
        text(hdc, sub, &rc(sc(40), y + sc(20), sc(250), y + sc(40)), DIM, f.small, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    }
}

unsafe fn pills(hdc: HDC, s: &mut State, y: i32, labels: &[String], sel: Option<usize>, mk: fn(usize) -> Act) {
    let mut x = sc(260);
    for (i, l) in labels.iter().enumerate() {
        let r = rc(x, y, x + sc(88), y + sc(36));
        let act = mk(i);
        let bg = if sel == Some(i) { ACCENT } else if s.hover == act { BTN_HOT } else { BTN };
        round(hdc, &r, bg, sc(14));
        text(hdc, l, &r, TEXT, s.fonts.normal, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        s.regions.push((r, act));
        x += sc(96);
    }
}

unsafe fn paint_settings(hdc: HDC, s: &mut State, w: i32, h: i32) {
    let f = s.fonts;
    let top = sc(56);
    let Some(d) = s.draft.clone() else { return };
    let dirty = s.saved.as_ref() != Some(&d);
    let row = sc(72);

    let saved_dc = SaveDC(hdc);
    IntersectClipRect(hdc, 0, top, w, h);
    let mut y = top + sc(28) - s.scroll;

    row_label(hdc, &f, y, "Clip length", "How far back a clip reaches");
    let labels: Vec<String> = LENGTHS.iter().map(|&v| if v >= 60 { format!("{}m", v / 60) } else { format!("{v}s") }).collect();
    pills(hdc, s, y, &labels, LENGTHS.iter().position(|&v| v == d.secs), Act::SetLen);
    y += row;

    row_label(hdc, &f, y, "Quality", "Resolution (scaled = more CPU)");
    let labels: Vec<String> = RESS.iter().map(|&v| if v == 0 { "Native".to_string() } else { format!("{v}p") }).collect();
    pills(hdc, s, y, &labels, RESS.iter().position(|&v| v == d.height), Act::SetRes);
    y += row;

    row_label(hdc, &f, y, "Frame rate", "Smoothness");
    let labels: Vec<String> = FPSS.iter().map(|&v| format!("{v} fps")).collect();
    pills(hdc, s, y, &labels, FPSS.iter().position(|&v| v == d.fps), Act::SetFps);
    y += row;

    row_label(hdc, &f, y, "Bit rate", "Sharper vs. file size");
    let labels: Vec<String> = MBPS.iter().map(|&v| format!("{v} Mbps")).collect();
    pills(hdc, s, y, &labels, MBPS.iter().position(|&v| v == d.mbps), Act::SetBr);
    y += row;

    row_label(hdc, &f, y, "Save location", "Where clips are stored");
    let bx = rc(sc(260), y, w - sc(190), y + sc(36));
    round(hdc, &bx, rgb(36, 36, 36), sc(10));
    text(
        hdc,
        &d.out.to_string_lossy(),
        &rc(bx.left + sc(12), bx.top, bx.right - sc(12), bx.bottom),
        TEXT,
        f.normal,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_PATH_ELLIPSIS,
    );
    button(hdc, s, rc(w - sc(176), y, w - sc(40), y + sc(36)), "Browse...", Act::Browse, BTN, BTN_HOT, TEXT);
    y += row;

    text(hdc, "Hotkeys", &rc(sc(40), y, sc(250), y + sc(28)), TEXT, f.big, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    y += sc(40);
    for i in 0..3 {
        text(hdc, KEY_TITLES[i], &rc(sc(40), y, sc(250), y + sc(36)), TEXT, f.bold, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        let active = s.rebinding == Some(i);
        let label = if active { "Press the new keys...  (Esc cancels)".to_string() } else { hk_name(&d.keys[i]) };
        let (bg, hot) = if active { (ACCENT, ACCENT) } else { (BTN, BTN_HOT) };
        button(hdc, s, rc(sc(260), y, sc(260) + sc(300), y + sc(36)), &label, Act::Rebind(i), bg, hot, TEXT);
        if !active {
            text(hdc, "click to rebind", &rc(sc(570), y, sc(760), y + sc(36)), rgb(110, 110, 110), f.small, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        }
        y += sc(52);
    }
    y += sc(12);
    let note = "Press Apply to save. Applying restarts recording, so the replay buffer starts empty. Games in exclusive fullscreen may hide the save notification.";
    text(hdc, note, &rc(sc(40), y, w - sc(60), y + sc(50)), DIM, f.small, DT_LEFT | DT_WORDBREAK);
    let content_bottom = y + sc(70);
    s.max_scroll = (content_bottom + s.scroll - h).max(0);
    s.scroll = s.scroll.clamp(0, s.max_scroll);
    RestoreDC(hdc, saved_dc);

    // top bar (drawn last so scrolled content slides under it)
    fill(hdc, &rc(0, 0, w, top), PANEL);
    s.regions.push((rc(0, 0, w, top), Act::None));
    button(hdc, s, rc(sc(10), sc(9), sc(112), top - sc(9)), "\u{2190}  Back", Act::Back, BTN, BTN_HOT, TEXT);
    text(hdc, "Settings", &rc(sc(128), 0, sc(400), top), TEXT, f.big, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    let (abg, ahot) = if dirty { (ACCENT, rgb(90, 160, 255)) } else { (BTN, BTN_HOT) };
    button(hdc, s, rc(w - sc(120), sc(10), w - sc(16), top - sc(10)), "Apply", Act::Apply, abg, ahot, TEXT);
    if let Some(t) = s.status_at {
        if t.elapsed() < Duration::from_secs(6) && !s.status.is_empty() {
            text(hdc, &s.status, &rc(sc(420), 0, w - sc(132), top), WARN, f.normal, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        }
    }
}

// ------------------------------------------------------------------ input

fn hit(s: &State, x: i32, y: i32) -> Act {
    for (r, a) in s.regions.iter().rev() {
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
            return *a;
        }
    }
    Act::None
}

fn on_click(act: Act, x: i32) {
    if !matches!(act, Act::Rebind(_)) {
        set_rebinding(None); // clicking anywhere else cancels a pending rebind
    }
    match act {
        Act::Gear => open_settings(),
        Act::SetLen(i) => {
            st(|s| s.draft.as_mut().map(|d| d.secs = LENGTHS[i]));
            invalidate();
        }
        Act::SetRes(i) => {
            st(|s| s.draft.as_mut().map(|d| d.height = RESS[i]));
            invalidate();
        }
        Act::SetFps(i) => {
            st(|s| s.draft.as_mut().map(|d| d.fps = FPSS[i]));
            invalidate();
        }
        Act::SetBr(i) => {
            st(|s| s.draft.as_mut().map(|d| d.mbps = MBPS[i]));
            invalidate();
        }
        Act::Browse => browse_folder(),
        Act::Rebind(i) => set_rebinding(Some(i)),
        Act::Apply => apply_settings(),
        Act::Tab(i) => {
            st(|s| {
                s.tab = if i == 0 { None } else { s.cats.get(i - 1).cloned() };
                s.scroll = 0;
                s.selected.clear();
            });
            invalidate();
        }
        Act::Tile(ci) => {
            // while anything is selected, clicking a tile toggles it instead of opening it
            if st(|s| s.selected.is_empty()) {
                open_clip(ci);
            } else {
                toggle_sel(ci);
            }
        }
        Act::TileSel(ci) => toggle_sel(ci),
        Act::TileDel(ci) => {
            if let Some(p) = st(|s| s.clips.get(ci).map(|c| c.path.clone())) {
                ask_delete(vec![p]);
            }
        }
        Act::SelAll => {
            st(|s| {
                let vis = visible(s);
                for i in vis {
                    let p = s.clips[i].path.clone();
                    s.selected.insert(p);
                }
            });
            invalidate();
        }
        Act::SelNone => {
            st(|s| s.selected.clear());
            invalidate();
        }
        Act::DeleteSel => {
            let v: Vec<PathBuf> = st(|s| s.selected.iter().cloned().collect());
            ask_delete(v);
        }
        Act::Back => go_back(),
        Act::Play => toggle_play(),
        Act::Seek => {
            st(|s| s.drag = Drag::Seek);
            let h = st(|s| s.hwnd);
            unsafe { SetCapture(h) };
            seek_to_x(x);
        }
        Act::Lane => lane_press(x),
        Act::Speed(i) => set_rate(i),
        Act::ModeTrim => {
            st(|s| s.cut_mode = false);
            invalidate();
        }
        Act::ModeCut => {
            st(|s| s.cut_mode = true);
            invalidate();
        }
        Act::Clear => {
            st(|s| {
                s.segs.clear();
                s.sel = None;
            });
            invalidate();
        }
        Act::Save => start_edit(),
        Act::Delete => {
            if let Some(p) = st(|s| s.cur.clone()) {
                ask_delete(vec![p]);
            }
        }
        Act::Yes => do_delete(),
        Act::No => {
            st(|s| {
                s.confirm = None;
                s.pending_del.clear();
            });
            layout();
            invalidate();
        }
        Act::None => {}
    }
}

fn on_key(vk: u16) {
    let (view, modal) = st(|s| (s.view, s.confirm.is_some()));
    if modal {
        if vk == VK_ESCAPE {
            on_click(Act::No, 0);
        } else if vk == VK_RETURN {
            on_click(Act::Yes, 0);
        }
        return;
    }
    if view == View::Settings {
        if vk == VK_ESCAPE {
            go_back();
        }
        return;
    }
    if view == View::Grid {
        let ctrl = unsafe { (GetKeyState(VK_CONTROL as i32) as u16) & 0x8000 != 0 };
        if vk == VK_ESCAPE {
            on_click(Act::SelNone, 0);
        } else if vk == VK_DELETE {
            if st(|s| !s.selected.is_empty()) {
                on_click(Act::DeleteSel, 0);
            }
        } else if ctrl && vk == 0x41 {
            on_click(Act::SelAll, 0); // Ctrl+A
        }
        return;
    }
    if view != View::Player {
        return;
    }
    let pos = st(|s| s.pos);
    match vk {
        VK_SPACE => toggle_play(),
        VK_ESCAPE => go_back(),
        VK_LEFT => set_pos(pos - 50_000_000),
        VK_RIGHT => set_pos(pos + 50_000_000),
        VK_DELETE | VK_BACK => {
            // remove the selected area (the clip itself is deleted with the button)
            st(|s| {
                if let Some(i) = s.sel.take() {
                    if i < s.segs.len() {
                        s.segs.remove(i);
                    }
                }
            });
            invalidate();
        }
        _ => {}
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let mx = (lp & 0xFFFF) as i16 as i32;
    let my = ((lp >> 16) & 0xFFFF) as i16 as i32;
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            on_paint(hwnd);
            0
        }
        WM_SIZE => {
            if !st(|s| s.video.is_null()) {
                layout();
                invalidate();
            }
            0
        }
        WM_GETMINMAXINFO => {
            let mmi = lp as *mut MINMAXINFO;
            (*mmi).ptMinTrackSize = POINT { x: sc(980), y: sc(560) };
            0
        }
        WM_MOUSEMOVE => {
            let (changed, drag, need_track) = st(|s| {
                let h = hit(s, mx, my);
                let c = h != s.hover;
                s.hover = h;
                let nt = !s.tracking;
                s.tracking = true;
                (c, s.drag, nt)
            });
            if need_track {
                let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut t);
            }
            match drag {
                Drag::Seek => seek_to_x(mx),
                Drag::None => {
                    if changed {
                        invalidate();
                    }
                }
                _ => {
                    st(|s| drag_update(s, mx));
                    invalidate();
                }
            }
            0
        }
        WM_MOUSELEAVE => {
            st(|s| {
                s.tracking = false;
                s.hover = Act::None;
            });
            invalidate();
            0
        }
        WM_LBUTTONDOWN => {
            let act = st(|s| hit(s, mx, my));
            on_click(act, mx);
            0
        }
        WM_LBUTTONUP => {
            finish_drag();
            0
        }
        WM_RBUTTONDOWN => {
            lane_remove_at(mx, my);
            0
        }
        WM_SETCURSOR => {
            if (lp & 0xFFFF) as u32 == HTCLIENT {
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                ScreenToClient(hwnd, &mut p);
                let cur = match lane_cursor_kind(p.x, p.y) {
                    1 => LoadCursorW(null_mut(), IDC_SIZEWE),
                    2 => LoadCursorW(null_mut(), IDC_SIZEALL),
                    _ => null_mut(),
                };
                if !cur.is_null() {
                    SetCursor(cur);
                    return 1;
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xFFFF) as i16 as i32;
            st(|s| {
                if (s.view == View::Grid || s.view == View::Settings) && s.confirm.is_none() {
                    s.scroll = (s.scroll - delta * sc(90) / 120).clamp(0, s.max_scroll);
                }
            });
            invalidate();
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if st(|s| s.rebinding.is_some()) {
                capture_key(wp as u32);
                return 0;
            }
            if msg == WM_SYSKEYDOWN {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            on_key(wp as u16);
            0
        }
        WM_SYSCHAR | WM_CHAR | WM_SYSKEYUP => {
            if st(|s| s.rebinding.is_some()) {
                0
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_OPEN_SETTINGS => {
            open_settings();
            0
        }
        WM_VCLICK => {
            if st(|s| s.view == View::Player && s.confirm.is_none()) {
                toggle_play();
            }
            0
        }
        WM_THUMB => {
            invalidate();
            0
        }
        WM_EDITDONE => {
            let m = EDIT_MSG.lock().unwrap().clone();
            set_status(&m);
            refresh_clips();
            0
        }
        WM_APP => {
            refresh_clips(); // another Alt+F11 press
            0
        }
        WM_ACTIVATEAPP => {
            if wp != 0 && st(|s| s.view == View::Grid) {
                refresh_clips();
            }
            0
        }
        WM_TIMER => {
            match wp {
                TIMER_TICK => tick(),
                TIMER_ICON => {
                    KillTimer(hwnd, TIMER_ICON);
                    ShowWindow(st(|s| s.icon), SW_HIDE);
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_TICK);
            if st(|s| s.rebinding.is_some()) {
                post_tray(WM_HK_RESUME);
            }
            stop_player();
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn video_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_LBUTTONDOWN => {
            PostMessageW(GetParent(h), WM_VCLICK, 0, 0);
            0
        }
        WM_KEYDOWN => {
            PostMessageW(GetParent(h), m, wp, lp);
            0
        }
        WM_ERASEBKGND => {
            let mut r: RECT = zeroed();
            GetClientRect(h, &mut r);
            FillRect(wp as HDC, &r, GetStockObject(BLACK_BRUSH) as HBRUSH);
            1
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

unsafe extern "system" fn icon_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_NCHITTEST => -1, // HTTRANSPARENT: clicks fall through to the video
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(h, &mut ps);
            let mut r: RECT = zeroed();
            GetClientRect(h, &mut r);
            fill(hdc, &r, rgb(24, 24, 24));
            let playing = ICON_PLAYING.with(|c| c.get());
            play_glyph(hdc, r.right / 2, r.bottom / 2, r.right / 2, playing, rgb(240, 240, 240));
            EndPaint(h, &ps);
            0
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

unsafe fn register(name: &str, proc_: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT, bg: HBRUSH, hinst: HINSTANCE) {
    let n = w(name);
    let mut wc: WNDCLASSEXW = zeroed();
    wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
    wc.lpfnWndProc = Some(proc_);
    wc.hInstance = hinst;
    wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
    wc.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    wc.hbrBackground = bg;
    wc.lpszClassName = n.as_ptr();
    RegisterClassExW(&wc);
}

pub fn run(cfg: Config, settings_first: bool) {
    unsafe {
        // Single instance: a second Alt+F11 just refreshes and focuses the open window.
        let name = w("Local\\ClipperGalleryInstance");
        let m = CreateMutexW(null(), 0, name.as_ptr());
        if m.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            let h = FindWindowW(w(CLASS).as_ptr(), null());
            if !h.is_null() {
                PostMessageW(h, if settings_first { WM_OPEN_SETTINGS } else { WM_APP }, 0, 0);
                ShowWindow(h, SW_RESTORE);
                SetForegroundWindow(h);
            }
            return;
        }

        SetProcessDPIAware();
        DPI.with(|d| d.set(GetDpiForSystem() as i32));
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let hinst = GetModuleHandleW(null());
        register(CLASS, wnd_proc, null_mut(), hinst);
        register(VCLASS, video_proc, null_mut(), hinst);
        register(ICLASS, icon_proc, null_mut(), hinst);

        let hwnd = CreateWindowExW(
            0,
            w(CLASS).as_ptr(),
            w("Clipper").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            sc(1280),
            sc(780),
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        let dark: i32 = 1;
        DwmSetWindowAttribute(hwnd, 20, &dark as *const i32 as *const c_void, 4); // dark title bar

        let video = CreateWindowExW(
            0, w(VCLASS).as_ptr(), null(), WS_CHILD | WS_CLIPSIBLINGS, 0, 0, 10, 10, hwnd, null_mut(), hinst, null(),
        );
        let icon = CreateWindowExW(
            0, w(ICLASS).as_ptr(), null(), WS_CHILD | WS_CLIPSIBLINGS, 0, 0, sc(44), sc(44), hwnd, null_mut(), hinst, null(),
        );
        let z = sc(44);
        SetWindowRgn(icon, CreateEllipticRgn(0, 0, z + 1, z + 1), 1);

        let face = w("Segoe UI");
        let mk = |px: i32, weight: i32| CreateFontW(-sc(px), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
        let fonts = Fonts { normal: mk(15, 400), small: mk(13, 400), bold: mk(15, 600), big: mk(19, 600) };

        st(|s| {
            s.cfg = Some(cfg);
            s.hwnd = hwnd;
            s.video = video;
            s.icon = icon;
            s.fonts = fonts;
        });
        refresh_clips();
        SetTimer(hwnd, TIMER_TICK, 120, None);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        if settings_first {
            open_settings();
        }

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
