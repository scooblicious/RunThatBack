# Clipper

A tiny, lightweight instant-replay clipper for Windows, written in Rust. It sits in the system tray, keeps the last
few minutes of your screen in a rolling buffer, and saves a clip when you press a hotkey, like NVIDIA ShadowPlay.

## Features
- Instant replay on a hotkey (default **Alt+F10**), with a sound and a top-right notification
- Records desktop audio and microphone
- Detects whether a clip is from a game or the desktop and names it (`Raft_2026-11-22_15-04-33_a3f9.mp4`)
- Dark gallery (**Alt+F11**) with thumbnails, category tabs, built-in player, playback speed
- Trim and cut with multiple draggable selections; multi-select delete
- Settings: clip length, resolution, frame rate, bit rate, save folder, rebindable hotkeys
- Uses NVIDIA / AMD / Intel hardware encoding when available, with automatic fallbacks

## Install
Download `Clipper-Setup-x.y.z.exe` from the [Releases](../../releases) page and run it.

## Build from source
Requires Windows, [Rust](https://rustup.rs) and the Visual Studio C++ Build Tools.

```
build_installer.bat        # builds the exe, downloads ffmpeg, and creates installer\Output\Clipper-Setup-x.y.z.exe
```

Or just `cargo build --release` and put an `ffmpeg.exe` next to `target\release\clipper.exe`.

## How it works
`ffmpeg` does GPU screen capture and hardware encoding into a ring of short segments. Clipper (Rust) handles the tray
icon, hotkeys, audio mixing (WASAPI), clip naming, the gallery (GDI + Media Foundation) and trimming. A hotkey press joins
the newest segments without re-encoding.

## Notes
- The installer bundles an unmodified GPL build of ffmpeg from <https://www.gyan.dev/ffmpeg/builds/>.
