//! clipper: tiny "instant replay" clipper for Windows. Runs in the tray.
//! FFmpeg does GPU capture + hardware encoding into a ring of short segments.
//! Desktop audio (WASAPI loopback) and mic are mixed in-process and piped to FFmpeg.
//! Alt+F10 stitches the newest segments into an MP4 (stream copy, no re-encode).
#![windows_subsystem = "windows"]

mod gallery;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::{
    collections::VecDeque,
    env,
    ffi::c_void,
    fs,
    io::Write,
    iter::once,
    mem::{size_of, zeroed},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{exit, Child, ChildStdin, Command, Stdio},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicIsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT},
    System::{
        Diagnostics::Debug::Beep,
        LibraryLoader::GetModuleHandleW,
        SystemInformation::GetLocalTime,
        Threading::{
            CreateMutexW, GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::{HiDpi::GetDpiForSystem, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SEG_SECS: u32 = 5;
const HK_SAVE: i32 = 1;
const HK_QUIT: i32 = 2;
const HK_GALLERY: i32 = 3;
const ID_SAVE: usize = 10;
const ID_QUIT: usize = 11;
const ID_GALLERY: usize = 12;
const ID_SETTINGS: usize = 13;
const WM_TRAY: u32 = WM_APP + 1;
const WM_RELOAD: u32 = WM_APP + 10; // settings changed: re-read clipper.cfg
const WM_TOAST: u32 = WM_APP + 11;
const WM_HK_PAUSE: u32 = WM_APP + 12; // gallery is capturing a new hotkey
const WM_HK_RESUME: u32 = WM_APP + 13;
const TOAST_CLASS: &str = "ClipperToast";

/// The sound played when a clip is saved (embedded in the exe).
static CLIP_WAV: &[u8] = include_bytes!("../assets/clip.wav");

// ---------------------------------------------------------------- hotkeys

#[derive(Clone, Copy, PartialEq, Debug)]
struct Hotkey {
    mods: u32, // MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN
    vk: u32,
}

fn vk_name(vk: u32) -> Option<String> {
    let n = match vk {
        0x30..=0x39 | 0x41..=0x5A => return Some((vk as u8 as char).to_string()),
        0x70..=0x87 => return Some(format!("F{}", vk - 0x6F)),
        0x60..=0x69 => return Some(format!("Num{}", vk - 0x60)),
        0x20 => "Space",
        0x2D => "Insert",
        0x2E => "Delete",
        0x24 => "Home",
        0x23 => "End",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x13 => "Pause",
        0x91 => "ScrollLock",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        _ => return None,
    };
    Some(n.to_string())
}

fn vk_from_name(n: &str) -> Option<u32> {
    (0..256u32).find(|&vk| vk_name(vk).map_or(false, |x| x.eq_ignore_ascii_case(n)))
}

fn hk_name(h: &Hotkey) -> String {
    let mut p: Vec<String> = Vec::new();
    if h.mods & MOD_CONTROL != 0 {
        p.push("Ctrl".into());
    }
    if h.mods & MOD_ALT != 0 {
        p.push("Alt".into());
    }
    if h.mods & MOD_SHIFT != 0 {
        p.push("Shift".into());
    }
    if h.mods & MOD_WIN != 0 {
        p.push("Win".into());
    }
    p.push(vk_name(h.vk).unwrap_or_else(|| format!("0x{:X}", h.vk)));
    p.join("+")
}

fn hk_parse(s: &str) -> Option<Hotkey> {
    let mut mods = 0;
    let mut vk = None;
    for tok in s.split('+') {
        match tok.trim().to_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" => mods |= MOD_WIN,
            other => vk = vk_from_name(other),
        }
    }
    Some(Hotkey { mods, vk: vk? })
}

// ---------------------------------------------------------------- config

#[derive(Clone)]
struct Config {
    ffmpeg: String,
    fps: u32,
    secs: u32,
    bitrate: String, // e.g. "15M"
    height: u32,     // 0 = native resolution, otherwise scale to this height
    encoder: String, // "auto" or an ffmpeg encoder name
    capture: String, // "ddagrab" (GPU) or "gdigrab" (works on any PC, uses more CPU)
    monitor: u32,
    out: PathBuf,
    cpu_copy: bool,
    desktop_audio: bool,
    mic: String, // "default" | "off" | name fragment
    aliases: Vec<(String, String)>, // exe stem (lowercase) -> display name
    keys: [Hotkey; 3], // save clip, open gallery, quit
}

static CFG: Mutex<Option<Config>> = Mutex::new(None);
static TMP: OnceLock<PathBuf> = OnceLock::new();
static CHILD: Mutex<Option<Child>> = Mutex::new(None);
static SRCS: Mutex<Vec<Buf>> = Mutex::new(Vec::new());
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static TOAST_HWND: AtomicIsize = AtomicIsize::new(0);
static TOAST_MSG: Mutex<(String, String, bool)> = Mutex::new((String::new(), String::new(), true));

fn cfg() -> Option<Config> {
    CFG.lock().unwrap().clone()
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

fn fatal(msg: &str) -> ! {
    let (m, t) = (w(msg), w("Clipper"));
    unsafe { MessageBoxW(null_mut(), m.as_ptr(), t.as_ptr(), MB_ICONERROR) };
    exit(1)
}

fn on(v: &str) -> bool {
    matches!(v.to_lowercase().as_str(), "on" | "true" | "1" | "yes")
}

fn exe_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_default()
}

fn default_ffmpeg() -> String {
    let local = exe_dir().join("ffmpeg.exe");
    if local.exists() { local.to_string_lossy().into() } else { "ffmpeg".into() }
}

impl Config {
    fn load() -> Self {
        let home = env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
        let mut c = Config {
            ffmpeg: default_ffmpeg(),
            fps: 60,
            secs: 30,
            bitrate: "15M".into(),
            height: 0,
            encoder: "auto".into(),
            capture: "ddagrab".into(),
            monitor: 0,
            out: PathBuf::from(home).join("Videos").join("Clips"),
            cpu_copy: false,
            desktop_audio: true,
            mic: "default".into(),
            aliases: Vec::new(),
            keys: [
                Hotkey { mods: MOD_ALT, vk: 0x79 }, // Alt+F10
                Hotkey { mods: MOD_ALT, vk: 0x7A }, // Alt+F11
                Hotkey { mods: MOD_ALT, vk: 0x7B }, // Alt+F12
            ],
        };
        if let Ok(text) = fs::read_to_string(exe_dir().join("clipper.cfg")) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let (k, v) = (k.trim().to_lowercase(), v.trim());
                    if v.is_empty() {
                        continue;
                    }
                    match k.as_str() {
                        "secs" => c.secs = v.parse().unwrap_or(c.secs).max(SEG_SECS),
                        "fps" => c.fps = v.parse().unwrap_or(c.fps).max(1),
                        "bitrate" => c.bitrate = v.into(),
                        "height" => c.height = v.parse().unwrap_or(0),
                        "encoder" => c.encoder = v.into(),
                        "capture" => c.capture = v.to_lowercase(),
                        "monitor" => c.monitor = v.parse().unwrap_or(0),
                        "out" => c.out = PathBuf::from(v),
                        "ffmpeg" => c.ffmpeg = v.into(),
                        "cpu_copy" => c.cpu_copy = on(v),
                        "desktop_audio" => c.desktop_audio = on(v),
                        "mic" => c.mic = v.into(),
                        "key_save" => c.keys[0] = hk_parse(v).unwrap_or(c.keys[0]),
                        "key_gallery" => c.keys[1] = hk_parse(v).unwrap_or(c.keys[1]),
                        "key_quit" => c.keys[2] = hk_parse(v).unwrap_or(c.keys[2]),
                        _ if k.starts_with("alias.") => c.aliases.push((k[6..].to_string(), v.to_string())),
                        _ => {}
                    }
                }
            }
        }
        c
    }

    fn save(&self) -> bool {
        let onoff = |b: bool| if b { "on" } else { "off" };
        let mut t = String::from("# Clipper settings. Edited from the Settings screen; you can also edit by hand.\n");
        t += &format!("secs={}\nfps={}\nbitrate={}\nheight={}\n", self.secs, self.fps, self.bitrate, self.height);
        t += &format!("encoder={}\ncapture={}\nmonitor={}\nout={}\n", self.encoder, self.capture, self.monitor, self.out.display());
        if self.ffmpeg != default_ffmpeg() {
            t += &format!("ffmpeg={}\n", self.ffmpeg);
        }
        t += &format!("cpu_copy={}\ndesktop_audio={}\nmic={}\n", onoff(self.cpu_copy), onoff(self.desktop_audio), self.mic);
        t += &format!(
            "key_save={}\nkey_gallery={}\nkey_quit={}\n",
            hk_name(&self.keys[0]),
            hk_name(&self.keys[1]),
            hk_name(&self.keys[2])
        );
        for (k, v) in &self.aliases {
            t += &format!("alias.{k}={v}\n");
        }
        fs::write(exe_dir().join("clipper.cfg"), t).is_ok()
    }
}

// ---------------------------------------------------------------- audio

type Buf = Arc<Mutex<VecDeque<f32>>>; // interleaved stereo f32 @ 48 kHz

struct Resampler {
    ch: usize,
    step: f64,
    frac: f64,
    prev: [f32; 2],
    frame: [f32; 2],
    n: usize,
    buf: Buf,
}

impl Resampler {
    fn new(ch: usize, rate: f64, buf: Buf) -> Self {
        Self { ch, step: rate / 48000.0, frac: 0.0, prev: [0.0; 2], frame: [0.0; 2], n: 0, buf }
    }

    fn push(&mut self, samples: impl Iterator<Item = f32>) {
        let mut out: Vec<f32> = Vec::with_capacity(2048);
        for s in samples {
            if self.n < 2 {
                self.frame[self.n] = s;
            }
            self.n += 1;
            if self.n == self.ch {
                let cur = if self.ch == 1 { [self.frame[0]; 2] } else { self.frame };
                while self.frac < 1.0 {
                    let f = self.frac as f32;
                    out.push(self.prev[0] + (cur[0] - self.prev[0]) * f);
                    out.push(self.prev[1] + (cur[1] - self.prev[1]) * f);
                    self.frac += self.step;
                }
                self.frac -= 1.0;
                self.prev = cur;
                self.n = 0;
            }
        }
        let mut q = self.buf.lock().unwrap();
        q.extend(out);
        if q.len() > 48000 * 2 * 3 / 10 {
            let drop = (q.len() - 48000 * 2 / 10) & !1; // keep ~100 ms
            q.drain(..drop);
        }
    }
}

fn start_capture(dev: &cpal::Device, loopback: bool, buf: Buf) -> Option<cpal::Stream> {
    // Using an output device as an input device enables WASAPI loopback.
    let cfg = if loopback { dev.default_output_config() } else { dev.default_input_config() }.ok()?;
    let ch = cfg.channels() as usize;
    let rate = cfg.sample_rate().0 as f64;
    let fmt = cfg.sample_format();
    let sc: cpal::StreamConfig = cfg.into();
    let mut rs = Resampler::new(ch, rate, buf);
    let err = |_: cpal::StreamError| {};
    let stream = match fmt {
        SampleFormat::F32 => dev.build_input_stream(&sc, move |d: &[f32], _| rs.push(d.iter().copied()), err, None),
        SampleFormat::I16 => dev.build_input_stream(
            &sc,
            move |d: &[i16], _| rs.push(d.iter().map(|&x| x as f32 / 32768.0)),
            err,
            None,
        ),
        SampleFormat::I32 => dev.build_input_stream(
            &sc,
            move |d: &[i32], _| rs.push(d.iter().map(|&x| x as f32 / 2147483648.0)),
            err,
            None,
        ),
        _ => return None,
    }
    .ok()?;
    stream.play().ok()?;
    Some(stream)
}

/// Clock-driven mixer: always emits exactly real-time worth of audio
/// (silence when nothing is playing), so loopback gaps never desync the video.
fn mixer(mut stdin: ChildStdin, srcs: Vec<Buf>) {
    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        thread::sleep(Duration::from_millis(10));
        let t = start.elapsed().as_secs_f64() - 0.05; // 50 ms jitter buffer
        if t <= 0.0 {
            continue;
        }
        let due = (t * 48000.0) as u64;
        if due <= sent {
            continue;
        }
        let n = (due - sent) as usize;
        let mut mix = vec![0f32; n * 2];
        for b in &srcs {
            let mut q = b.lock().unwrap();
            for m in mix.iter_mut() {
                match q.pop_front() {
                    Some(v) => *m += v,
                    None => break,
                }
            }
        }
        bytes.clear();
        for v in mix {
            bytes.extend_from_slice(&v.clamp(-1.0, 1.0).to_le_bytes());
        }
        if stdin.write_all(&bytes).is_err() {
            break;
        }
        sent += n as u64;
    }
}

// ---------------------------------------------------------------- recording

fn spawn_recorder(c: &Config, tmp: &Path, audio: bool) -> std::io::Result<Child> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |s: &[&str]| a.extend(s.iter().map(|x| x.to_string()));

    push(&["-hide_banner", "-loglevel", "error", "-y"]);
    let gpu_frames = c.capture != "gdigrab";
    if gpu_frames {
        // GPU desktop capture (Desktop Duplication): frames stay on the GPU
        push(&["-f", "lavfi", "-i"]);
        push(&[&format!("ddagrab=output_idx={}:framerate={}:draw_mouse=1", c.monitor, c.fps)]);
    } else {
        // Compatibility capture: works on any PC (primary monitor), costs some CPU
        push(&["-f", "gdigrab", "-framerate", &c.fps.to_string(), "-draw_mouse", "1", "-i", "desktop"]);
    }
    if audio {
        push(&["-f", "f32le", "-ar", "48000", "-ac", "2", "-i", "pipe:0"]);
    }
    push(&["-map", "0:v"]);
    if audio {
        push(&["-map", "1:a", "-c:a", "aac", "-b:a", "160k"]);
    }
    let hw = ["nvenc", "amf", "qsv"].iter().any(|h| c.encoder.contains(h));
    if gpu_frames && (c.cpu_copy || !hw || c.height > 0) {
        // scaling happens on the CPU (hwdownload first); native resolution stays on the GPU path
        let mut vf = "hwdownload,format=bgra".to_string();
        if c.height > 0 {
            vf += &format!(",scale=-2:{}:flags=fast_bilinear", c.height);
        }
        push(&["-vf", &vf]);
    } else if !gpu_frames && c.height > 0 {
        push(&["-vf", &format!("scale=-2:{}:flags=fast_bilinear", c.height)]);
    }
    // Constant frame rate: an idle desktop must still produce frames, otherwise there are no
    // keyframes and the "5 second" ring-buffer files can stretch to minutes or hours.
    push(&["-fps_mode", "cfr", "-c:v", &c.encoder, "-b:v", &c.bitrate]);
    if c.encoder.contains("nvenc") {
        push(&["-preset", "p4"]);
    }
    if c.encoder == "libx264" {
        push(&["-preset", "veryfast"]);
    }
    push(&["-g", &(c.fps * SEG_SECS).to_string()]);
    // Keyframes on a wall-clock schedule (not just every N frames) so segments split every SEG_SECS.
    push(&["-force_key_frames", &format!("expr:gte(t,n_forced*{SEG_SECS})")]);
    if c.encoder.contains("nvenc") {
        push(&["-forced-idr", "1"]);
    }
    let wrap = c.secs / SEG_SECS + 2;
    push(&["-f", "segment", "-segment_time", &SEG_SECS.to_string()]);
    push(&["-segment_wrap", &wrap.to_string(), "-segment_format", "mpegts"]);
    push(&[&tmp.join("seg%03d.ts").to_string_lossy()]);

    Command::new(&c.ffmpeg)
        .args(&a)
        .stdin(if audio { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(
            fs::File::create(env::temp_dir().join("clipper_ffmpeg.log"))
                .map_or_else(|_| Stdio::null(), Stdio::from),
        )
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

fn tone(freq: u32, ms: u32) {
    unsafe { Beep(freq, ms) };
}

fn save_clip(c: &Config, tmp: &Path, name: &str) {
    // Only recent segments count. Anything older than the clip length (plus slack) is stale,
    // whatever its contents are.
    let now = SystemTime::now();
    let window = Duration::from_secs((c.secs + 3 * SEG_SECS + 3) as u64);
    let mut segs: Vec<(SystemTime, PathBuf)> = fs::read_dir(tmp)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().map_or(false, |x| x == "ts"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    segs.retain(|(t, _)| now.duration_since(*t).map_or(true, |age| age <= window));
    segs.sort();
    if segs.is_empty() {
        toast("Couldn't save clip", "Nothing has been recorded yet", false);
        tone(300, 200);
        return;
    }
    let out = unique_path(&c.out, name);
    let stem = out.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let list: String = segs
        .iter()
        .map(|(_, p)| {
            let s = p.to_string_lossy().replace('\\', "/").replace('\'', "'\\''");
            format!("file '{s}'\n")
        })
        .collect();
    let list_path = tmp.join(format!("{stem}.list.txt"));
    let joined = tmp.join(format!("{stem}.joined.mp4"));
    if fs::write(&list_path, list).is_err() {
        return;
    }
    // 1) join the segments (stream copy)
    let joined_ok = Command::new(&c.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args(["-c", "copy"])
        .arg(&joined)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_or(false, |s| s.success());
    // 2) keep only the last `secs` seconds, whatever the buffer actually held
    let ok = joined_ok && {
        let trimmed = Command::new(&c.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-sseof", &format!("-{}", c.secs), "-i"])
            .arg(&joined)
            .args(["-c", "copy", "-movflags", "+faststart"])
            .arg(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_or(false, |s| s.success());
        trimmed || fs::copy(&joined, &out).is_ok()
    };
    let _ = fs::remove_file(&joined);
    let _ = fs::remove_file(&list_path);
    if ok {
        toast("Clip saved", &stem, true);
    } else {
        toast("Couldn't save clip", "ffmpeg failed. Check clipper_ffmpeg.log in %TEMP%", false);
        tone(300, 300);
    }
}

fn play_clip_sound() {
    unsafe {
        PlaySoundW(CLIP_WAV.as_ptr() as *const u16, null_mut(), SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

fn toast(title: &str, sub: &str, ok: bool) {
    *TOAST_MSG.lock().unwrap() = (title.to_string(), sub.to_string(), ok);
    let h = MAIN_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe { PostMessageW(h as HWND, WM_TOAST, 0, 0) };
    }
}

fn trigger_save() {
    let (Some(c), Some(t)) = (cfg(), TMP.get().cloned()) else { return };
    let name = detect_name(&c); // must run now: the foreground window at keypress time
    play_clip_sound(); // instant feedback; the toast follows once the file is written
    thread::spawn(move || save_clip(&c, &t, &name));
}

// ---------------------------------------------------------------- program detection

fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    let out: String = out.chars().take(32).collect();
    if out.is_empty() { "Game".into() } else { out }
}

/// "Desktop" unless the foreground window looks like a game.
/// Game = lives in a known game-library folder, or is a fullscreen window of a non-browser/non-media app.
fn detect_name(cfg: &Config) -> String {
    const LIBS: [&str; 8] = [
        "\\steamapps\\common\\",
        "\\epic games\\",
        "\\gog galaxy\\games\\",
        "\\riot games\\",
        "\\ubisoft game launcher\\games\\",
        "\\ea games\\",
        "\\xboxgames\\",
        "\\battle.net\\games\\",
    ];
    const NOT_GAMES: [&str; 16] = [
        "explorer", "chrome", "msedge", "firefox", "brave", "opera", "vivaldi", "vlc", "mpv",
        "potplayermini64", "wmplayer", "applicationframehost", "code", "powerpnt", "netflix", "spotify",
    ];
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return "Desktop".into();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 || pid == GetCurrentProcessId() {
            return "Desktop".into();
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return "Desktop".into();
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return "Desktop".into();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let lower = path.to_ascii_lowercase();
        let file = path.rsplit('\\').next().unwrap_or("");
        let stem = file.rsplit_once('.').map_or(file, |(a, _)| a);
        let stem_l = stem.to_ascii_lowercase();

        let in_lib = LIBS.iter().find(|l| lower.contains(*l));
        let mut r: RECT = zeroed();
        GetWindowRect(hwnd, &mut r);
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        let covers = GetMonitorInfoW(mon, &mut mi) != 0
            && r.left <= mi.rcMonitor.left
            && r.top <= mi.rcMonitor.top
            && r.right >= mi.rcMonitor.right
            && r.bottom >= mi.rcMonitor.bottom;
        let is_game = in_lib.is_some() || (covers && !NOT_GAMES.contains(&stem_l.as_str()));
        if !is_game {
            return "Desktop".into();
        }
        if let Some((_, nice)) = cfg.aliases.iter().find(|(k, _)| *k == stem_l) {
            return sanitize(nice);
        }
        // Steam: use the game's folder name (…\\steamapps\\common\\<Game>\\…)
        if let Some(l) = in_lib {
            if l.starts_with("\\steamapps") {
                if let Some(i) = lower.find(*l) {
                    let rest = &path[i + l.len()..];
                    if let Some(folder) = rest.split('\\').next() {
                        if !folder.is_empty() && folder.len() < rest.len() {
                            return sanitize(folder);
                        }
                    }
                }
            }
        }
        sanitize(stem)
    }
}

/// <Name>_<YYYY-MM-DD>_<HH-MM-SS>_<4 hex>.mp4, never overwriting an existing file.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let t = unsafe {
        let mut t: SYSTEMTIME = zeroed();
        GetLocalTime(&mut t);
        t
    };
    loop {
        let id = RandomState::new().build_hasher().finish() & 0xFFFF;
        let f = format!(
            "{name}_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}_{id:04x}.mp4",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        );
        let p = dir.join(f);
        if !p.exists() {
            return p;
        }
    }
}

// ---------------------------------------------------------------- gallery

/// The gallery is a separate process (same exe, `--gallery`) so it costs nothing when closed.
fn open_gallery(settings: bool) {
    if let Ok(exe) = env::current_exe() {
        let mut c = Command::new(exe);
        c.arg("--gallery");
        if settings {
            c.arg("--settings");
        }
        let _ = c.spawn();
    }
}

// ---------------------------------------------------------------- recorder lifecycle

/// Every way of recording we know, best first. The configured method is tried first; if it
/// doesn't work on this PC (no NVIDIA GPU, old driver, hybrid laptop...), the rest are tried in order.
fn candidates(c: &Config) -> Vec<Config> {
    let mut v: Vec<Config> = Vec::new();
    let mut add = |cap: &str, enc: &str, cpu: bool| {
        if !v.iter().any(|y| y.capture == cap && y.encoder == enc && y.cpu_copy == cpu) {
            let mut x = c.clone();
            x.capture = cap.into();
            x.encoder = enc.into();
            x.cpu_copy = cpu;
            v.push(x);
        }
    };
    let first = if c.encoder == "auto" { "h264_nvenc" } else { c.encoder.as_str() };
    add(&c.capture, first, c.cpu_copy);
    for cap in ["ddagrab", "gdigrab"] {
        for enc in ["h264_nvenc", "h264_amf", "h264_qsv", "libx264"] {
            for cpu in [false, true] {
                if (enc == "libx264" || cap == "gdigrab") && !cpu {
                    continue; // these always work on system-memory frames
                }
                add(cap, enc, cpu);
            }
        }
    }
    v
}

fn clear_dir(p: &Path) {
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let _ = fs::remove_file(e.path());
        }
    }
}

fn log_tail(n: usize) -> String {
    let t = fs::read_to_string(env::temp_dir().join("clipper_ffmpeg.log")).unwrap_or_default();
    let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .map(|l| l.chars().take(150).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// (Re)starts ffmpeg with the current settings, falling back through `candidates` until one
/// stays alive. Old segments are discarded because their resolution/framerate may differ.
fn restart_recorder() {
    let (Some(cfg), Some(tmp)) = (cfg(), TMP.get()) else { return };
    let mut guard = CHILD.lock().unwrap();
    if let Some(mut c) = guard.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    clear_dir(tmp);
    let srcs: Vec<Buf> = SRCS.lock().unwrap().clone();
    let audio = !srcs.is_empty();

    for (i, cand) in candidates(&cfg).iter().enumerate() {
        let mut child = match spawn_recorder(cand, tmp, audio) {
            Ok(c) => c,
            Err(_) => fatal("Could not start ffmpeg.\n\nReinstall Clipper, or put ffmpeg.exe next to clipper.exe."),
        };
        // a method that can't work on this PC makes ffmpeg exit within a moment
        let t0 = Instant::now();
        let mut alive = true;
        while t0.elapsed() < Duration::from_millis(2000) {
            if matches!(child.try_wait(), Ok(Some(_))) {
                alive = false;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if !alive {
            clear_dir(tmp);
            continue;
        }
        if audio {
            if let Some(stdin) = child.stdin.take() {
                let s = srcs.clone();
                thread::spawn(move || mixer(stdin, s)); // ends by itself when this ffmpeg dies
            }
        }
        *guard = Some(child);
        if i > 0 {
            // the configured method failed: remember the one that works so next start is instant
            let mut nc = cfg.clone();
            nc.encoder = cand.encoder.clone();
            nc.capture = cand.capture.clone();
            nc.cpu_copy = cand.cpu_copy;
            let _ = nc.save();
            *CFG.lock().unwrap() = Some(nc);
        }
        return;
    }
    let msg = format!(
        "Clipper couldn't start recording on this PC.\n\nffmpeg said:\n{}\n\nFull log: %TEMP%\\clipper_ffmpeg.log",
        log_tail(6)
    );
    let (m, t) = (w(&msg), w("Clipper"));
    unsafe { MessageBoxW(null_mut(), m.as_ptr(), t.as_ptr(), MB_ICONERROR) };
}

/// Returns the friendly names of hotkeys that could not be registered.
unsafe fn register_hotkeys(hwnd: HWND, c: &Config) -> Vec<&'static str> {
    let mut failed = Vec::new();
    for (id, hk, name) in [
        (HK_SAVE, c.keys[0], "Save clip"),
        (HK_GALLERY, c.keys[1], "Open clips"),
        (HK_QUIT, c.keys[2], "Quit"),
    ] {
        UnregisterHotKey(hwnd, id);
        if RegisterHotKey(hwnd, id, hk.mods | MOD_NOREPEAT, hk.vk) == 0 {
            failed.push(name);
        }
    }
    failed
}

unsafe fn unregister_hotkeys(hwnd: HWND) {
    for id in [HK_SAVE, HK_GALLERY, HK_QUIT] {
        UnregisterHotKey(hwnd, id);
    }
}

fn warn_hotkeys(failed: &[&str]) {
    let msg = format!(
        "Couldn't register: {}.\n\nAnother program may be using those keys (the GeForce overlay uses Alt+F10). Open Settings from the tray menu to choose different ones.",
        failed.join(", ")
    );
    let (m, t) = (w(&msg), w("Clipper"));
    unsafe { MessageBoxW(null_mut(), m.as_ptr(), t.as_ptr(), MB_ICONWARNING) };
}

fn tip(c: &Config) -> String {
    format!("Clipper - recording ({} saves)", hk_name(&c.keys[0]))
}

/// Settings were changed in the gallery: reload config, rebind hotkeys, restart recording.
unsafe fn reload(hwnd: HWND) {
    let new = Config::load();
    let _ = fs::create_dir_all(&new.out);
    *CFG.lock().unwrap() = Some(new.clone());
    let failed = register_hotkeys(hwnd, &new);
    let mut nid = tray_data(hwnd, &tip(&new));
    nid.uFlags = NIF_TIP;
    Shell_NotifyIconW(NIM_MODIFY, &nid);
    restart_recorder();
    if !failed.is_empty() {
        warn_hotkeys(&failed);
    }
}

// ---------------------------------------------------------------- toast (top-right notification)

fn sc(v: i32) -> i32 {
    unsafe { v * GetDpiForSystem() as i32 / 96 }
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

unsafe fn toast_text(hdc: HDC, s: &str, r: &RECT, c: u32, font: HFONT) {
    SelectObject(hdc, font);
    SetTextColor(hdc, c);
    SetBkMode(hdc, 1);
    let ws: Vec<u16> = s.encode_utf16().collect();
    let mut rr = *r;
    DrawTextW(hdc, ws.as_ptr(), ws.len() as i32, &mut rr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX);
}

unsafe fn paint_toast(h: HWND) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(h, &mut ps);
    let mut r: RECT = zeroed();
    GetClientRect(h, &mut r);
    let (title, sub, ok) = TOAST_MSG.lock().unwrap().clone();
    SetDCBrushColor(hdc, rgb(30, 30, 30));
    FillRect(hdc, &r, GetStockObject(DC_BRUSH) as HBRUSH);
    SetDCBrushColor(hdc, if ok { rgb(52, 190, 110) } else { rgb(200, 70, 70) });
    FillRect(hdc, &RECT { left: 0, top: 0, right: sc(6), bottom: r.bottom }, GetStockObject(DC_BRUSH) as HBRUSH);
    let face = w("Segoe UI");
    let big = CreateFontW(-sc(18), 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    let small = CreateFontW(-sc(14), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    let x = sc(24);
    toast_text(hdc, &title, &RECT { left: x, top: sc(10), right: r.right - sc(12), bottom: sc(40) }, rgb(240, 240, 240), big);
    toast_text(hdc, &sub, &RECT { left: x, top: sc(40), right: r.right - sc(12), bottom: r.bottom - sc(8) }, rgb(160, 160, 160), small);
    DeleteObject(big);
    DeleteObject(small);
    EndPaint(h, &ps);
}

unsafe fn show_toast() {
    let h = TOAST_HWND.load(Ordering::Relaxed) as HWND;
    if h.is_null() {
        return;
    }
    let (wd, ht) = (sc(380), sc(76));
    let mut wa: RECT = zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut RECT as *mut c_void, 0);
    SetWindowRgn(h, CreateRoundRectRgn(0, 0, wd + 1, ht + 1, sc(16), sc(16)), 1);
    SetLayeredWindowAttributes(h, 0, 238, LWA_ALPHA);
    SetWindowPos(h, HWND_TOPMOST, wa.right - wd - sc(20), wa.top + sc(20), wd, ht, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    InvalidateRect(h, null(), 1);
    SetTimer(h, 1, 3500, None);
}

unsafe extern "system" fn toast_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_PAINT => {
            paint_toast(h);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_TIMER => {
            KillTimer(h, 1);
            ShowWindow(h, SW_HIDE);
            0
        }
        WM_LBUTTONDOWN => {
            KillTimer(h, 1);
            ShowWindow(h, SW_HIDE);
            thread::spawn(|| open_gallery(false));
            0
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

// ---------------------------------------------------------------- tray / window

unsafe fn tray_data(hwnd: HWND, tip: &str) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    for (i, ch) in tip.encode_utf16().take(127).enumerate() {
        nid.szTip[i] = ch;
    }
    nid
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    let (sv, gl) = match cfg() {
        Some(c) => (hk_name(&c.keys[0]), hk_name(&c.keys[1])),
        None => ("Alt+F10".to_string(), "Alt+F11".to_string()),
    };
    AppendMenuW(menu, MF_STRING, ID_SAVE, w(&format!("Save clip ({sv})")).as_ptr());
    AppendMenuW(menu, MF_STRING, ID_GALLERY, w(&format!("Open clips ({gl})")).as_ptr());
    AppendMenuW(menu, MF_STRING, ID_SETTINGS, w("Settings...").as_ptr());
    AppendMenuW(menu, MF_STRING, ID_QUIT, w("Quit").as_ptr());
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            match wp as i32 {
                HK_SAVE => trigger_save(),
                HK_GALLERY => {
                    thread::spawn(|| open_gallery(false));
                }
                HK_QUIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_TRAY => {
            let ev = (lp as u32) & 0xFFFF;
            if ev == WM_RBUTTONUP || ev == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        WM_COMMAND => {
            match wp & 0xFFFF {
                ID_SAVE => trigger_save(),
                ID_GALLERY => {
                    thread::spawn(|| open_gallery(false));
                }
                ID_SETTINGS => {
                    thread::spawn(|| open_gallery(true));
                }
                ID_QUIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_RELOAD => {
            reload(hwnd);
            0
        }
        WM_TOAST => {
            show_toast();
            0
        }
        WM_HK_PAUSE => {
            unregister_hotkeys(hwnd);
            0
        }
        WM_HK_RESUME => {
            if let Some(c) = cfg() {
                register_hotkeys(hwnd, &c);
            }
            0
        }
        WM_DESTROY => {
            let nid = tray_data(hwnd, "");
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--gallery") {
        gallery::run(Config::load(), args.iter().any(|a| a == "--settings"));
        return;
    }
    unsafe {
        // Single instance
        let name = w("Local\\ClipperSingleInstance");
        let m = CreateMutexW(null(), 0, name.as_ptr());
        if m.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        SetProcessDPIAware(); // crisp toast on scaled displays
    }

    let cfg0 = Config::load();
    let tmp = env::temp_dir().join("clipper");
    let _ = fs::remove_dir_all(&tmp);
    if fs::create_dir_all(&tmp).is_err() || fs::create_dir_all(&cfg0.out).is_err() {
        fatal("Could not create temp or output folder.");
    }
    *CFG.lock().unwrap() = Some(cfg0.clone());
    TMP.set(tmp.clone()).ok();

    // Audio sources (streams must stay alive on this thread)
    let host = cpal::default_host();
    let mut streams: Vec<cpal::Stream> = Vec::new();
    let mut srcs: Vec<Buf> = Vec::new();
    let mut add = |dev: Option<cpal::Device>, loopback: bool| {
        if let Some(d) = dev {
            let b: Buf = Arc::new(Mutex::new(VecDeque::new()));
            if let Some(s) = start_capture(&d, loopback, b.clone()) {
                streams.push(s);
                srcs.push(b);
            }
        }
    };
    if cfg0.desktop_audio {
        add(host.default_output_device(), true);
    }
    if !cfg0.mic.eq_ignore_ascii_case("off") {
        let dev = if cfg0.mic.eq_ignore_ascii_case("default") {
            host.default_input_device()
        } else {
            let want = cfg0.mic.to_lowercase();
            host.input_devices()
                .ok()
                .and_then(|mut it| it.find(|d| d.name().map_or(false, |n| n.to_lowercase().contains(&want))))
        };
        add(dev, false);
    }
    *SRCS.lock().unwrap() = srcs;
    restart_recorder();

    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = w("ClipperWnd");
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null(),
        );
        if hwnd.is_null() {
            fatal("Could not create window.");
        }
        MAIN_HWND.store(hwnd as isize, Ordering::Relaxed);

        // Toast window (hidden until a clip is saved)
        let tclass = w(TOAST_CLASS);
        let mut tw: WNDCLASSEXW = zeroed();
        tw.cbSize = size_of::<WNDCLASSEXW>() as u32;
        tw.lpfnWndProc = Some(toast_proc);
        tw.hInstance = hinst;
        tw.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        tw.lpszClassName = tclass.as_ptr();
        RegisterClassExW(&tw);
        let th = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            tclass.as_ptr(), null(), WS_POPUP, 0, 0, 10, 10, null_mut(), null_mut(), hinst, null(),
        );
        TOAST_HWND.store(th as isize, Ordering::Relaxed);

        let failed = register_hotkeys(hwnd, &cfg0);
        if !failed.is_empty() {
            warn_hotkeys(&failed);
        }

        let nid = tray_data(hwnd, &tip(&cfg0));
        Shell_NotifyIconW(NIM_ADD, &nid);

        // Watchdog: wakes once a second. If ffmpeg dies (display change, driver hiccup...)
        // it is restarted; only repeated failures show an error.
        let hw = hwnd as usize;
        thread::spawn(move || {
            let (mut tries, mut stable) = (0u32, 0u32);
            loop {
                thread::sleep(Duration::from_secs(1));
                let dead = CHILD
                    .lock()
                    .unwrap()
                    .as_mut()
                    .map_or(false, |c| matches!(c.try_wait(), Ok(Some(_))));
                if !dead {
                    stable += 1;
                    if stable >= 120 {
                        tries = 0;
                    }
                    continue;
                }
                stable = 0;
                if tries < 3 {
                    tries += 1;
                    restart_recorder();
                    continue;
                }
                let msg = format!(
                    "Recording stopped (ffmpeg keeps exiting).\n\nffmpeg said:\n{}\n\nFull log: %TEMP%\\clipper_ffmpeg.log",
                    log_tail(5)
                );
                let (m, t) = (w(&msg), w("Clipper"));
                MessageBoxW(null_mut(), m.as_ptr(), t.as_ptr(), MB_ICONERROR);
                PostMessageW(hw as HWND, WM_CLOSE, 0, 0);
                return;
            }
        });

        // Blocks (0% CPU) until a message arrives
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    if let Some(mut c) = CHILD.lock().unwrap().take() {
        let _ = c.kill();
    }
    drop(streams);
    let _ = fs::remove_dir_all(&tmp);
}
