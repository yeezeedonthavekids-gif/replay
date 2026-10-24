# Replay

A lightweight instant-replay recorder for Windows. Replay sits in the tray, records
while you play, keeps the last 30 seconds to 3 minutes in memory, and saves it as an MP4
when you press **Alt+F10**.

- **One small `.exe`** (~0.7 MB). No installer, no runtime, no bundled FFmpeg, no web view.
  It uses only what ships with Windows.
- **Hardware encoding** on NVIDIA, AMD and Intel GPUs through their Media Foundation
  encoders, with the built-in Windows software encoder as a fallback.
- **Costs nothing when you're not playing.** Recording starts when a game is in the
  foreground and stops when it exits. While idle, Replay is a tray icon checking the
  foreground window once a second.
- **Saving is instant.** The buffer already holds compressed video, so a save only copies
  packets into an MP4. Nothing is re-encoded, and recording never pauses.
- **Records the game, not your desktop.** By default only the game's window is captured
  (Windows.Graphics.Capture), like Medal. Other windows, notifications, Discord and your
  second monitor never end up in clips, even if they overlap the game. Windowed games are
  cropped to the game area, without the title bar.
- **Doesn't touch your game.** Nothing is injected into the game process, and game
  processes are only opened with the limited query rights Task Manager uses.
- **No network access, no account, no telemetry.**

## Using it

1. Run `replay.exe`. The first time, Settings opens and a tray icon appears.
2. Play a game. The tray icon turns red while recording.
3. Press **Alt+F10** to save. You get a chime and a notification. Click the notification to
   show the file.

Clips are saved to `Videos\Replay\<Game>\<Game> <date> <time>.mp4`.

Left-click the tray icon to open **Clips**: play, rename (F2), delete to the Recycle Bin,
show in folder, or **Trim**. Trimming is lossless and frame-accurate. It cuts at a keyframe
and uses an MP4 edit list to start exactly where you chose, so it takes about a second and
doesn't reduce quality. "Replace original" moves the original to the Recycle Bin, so it can
be undone.

Right-click the tray icon for:

- Save clip
- Clips
- Open clips folder
- Always record (whole screen)
- Don't record *app* (shown when an app was auto-detected as a game)
- Pause
- Settings
- Exit

If a game closes or crashes, its replay can still be saved for 2 minutes afterwards.

### Settings

| Setting | Options | Default |
|---|---|---|
| Record | When a game is running / Always (whole screen) | When a game is running |
| Capture | Game window only / Entire display | Game window only |
| Replay length | 30 s, 45 s, 1, 1.5, 2, 3 min | 1 min |
| Save hotkey | any combination with Ctrl/Alt, or a function key | Alt+F10 |
| Resolution | Native, 1440p, 1080p, 720p (never upscales) | 1080p |
| Frame rate | 30, 60, 120 fps | 60 |
| Quality | Low, Medium, High, Very high | High (about 15 Mbps at 1080p60) |
| Encoder | Automatic (GPU first) / Software | Automatic |
| Audio | All system audio / Game audio only / None, plus microphone | All system audio |

Settings shows how much memory the buffer uses for your choices, along with a live status
line: what's being recorded, the encoder, buffer length, dropped frames, and Replay's own
CPU and memory use.

"Game audio only" records just the game process and its children, so voice chat and music
stay out of clips. It needs Windows 10 version 2004 or newer.

### How games are detected

Checked in this order:

1. your **Never record** list;
2. your **Always record** list (Settings → Add game...);
3. a built-in list of about 80 popular games (Fortnite, Rocket League, VALORANT, CS2, ...);
4. any other app whose window covers the whole monitor (fullscreen or borderless), except
   known non-games such as browsers, video players and launchers.

Games running in a window that aren't on a list need to be added once in Settings.

## Requirements

- Windows 10 (1903 or newer) or Windows 11, 64-bit.
- A GPU with a hardware H.264 encoder is recommended. Any NVIDIA GTX 600+, AMD GCN+ or
  Intel HD 4000+ has one. Without it, the software encoder is used, which costs much more
  CPU.
- Windows **N** editions need the free *Media Feature Pack* (it provides Media Foundation).

## Status and known limitations

This is version 0.1. What has been verified so far:

- **Core logic** (replay buffer, H.264 parsing, MP4 writer and reader, trimming, settings,
  game rules) is covered by unit tests. The MP4 tests run real H.264/AAC from ffmpeg
  through the code and check the result with ffprobe and a full decode.
- **The app shell was smoke-tested under Wine.** That covers the tray and menu, game
  detection (known game and fullscreen heuristic), game-exit handling, Settings, Clips,
  Trim (output verified frame-accurate), "Don't record", and the hotkey. Error handling was
  checked where Wine lacks screen capture and encoders: clear messages, retries, no crash.
- **Verified on real Windows by the first user:** display capture records.
- **Not yet verified on real Windows hardware:** game-window capture (new in this
  version), hardware/software encoding across GPUs, and audio capture. Wine implements none of these, so this code is compiled but
  untested. Please run the self-test below on your PC and report the output.

Known limitations:

- Game-window capture needs Windows 10 version 1903 or newer. On older versions Replay
  records the game's whole display instead and says so.
- On Windows 10, Windows draws a yellow border around the game window while it's being
  captured. The border is not in the recording. Windows 11 lets Replay hide it.
- The system mouse cursor isn't recorded (games draw their own cursor, which is).
- HDR displays are captured as SDR without tone mapping, so colors can look washed out.
- On hybrid-GPU laptops, the display must be driven by the GPU Replay runs on. Replay
  explains this if capture fails.
- Exclusive-fullscreen games normally work with Desktop Duplication on Windows 10/11, but
  aren't tested yet.

### Self-test

```
replay.exe --selftest 5 C:\path\test.mp4             (GPU encoder if available)
replay.exe --selftest 5 C:\path\test.mp4 --software  (force the software encoder)
```

This records the main display for 5 seconds without any UI, writes the clip, and prints
which encoder was used, frames captured and dropped, and any errors. The same output goes
to the log at `%LOCALAPPDATA%\Replay\replay.log`.

## Building

On Windows, with Rust installed:

```
cargo build --release          # target\release\replay.exe
cargo test                     # unit tests (MP4 tests also need ffmpeg on PATH)
```

Cross-compiling from Linux:

```
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

`tools/make_icons.py` (needs Pillow) regenerates `assets/*.ico`.

## How it works

```
video thread (paced at the target fps):
  game window (Windows.Graphics.Capture) or whole display (DXGI Desktop Duplication)
    -> D3D11 video processor (crop, scale, BGRA -> NV12)
    -> Media Foundation H.264 encoder (NVENC / AMF / QSV, or the Windows software encoder)
    -> PacketRing

audio thread:
  WASAPI loopback / game-only process loopback / mic
    -> 48 kHz mix -> AAC encoder -> PacketRing

hotkey:
  snapshot the ring (copies references, not data) -> MP4 writer on a worker thread
```

- **Frames stay on the GPU** from capture to encoder. The encoder reads D3D11 textures
  directly.
- **The ring stores encoded packets.** One minute of 1080p60 is about 110 MB. It drops whole
  keyframe groups (one per second) from the front, so the oldest frame is always decodable.
- **Audio and video share the QPC clock.** Each audio device's packets are placed on that
  timeline, and silent gaps are filled, so sound stays in sync even when devices go quiet
  or get unplugged.
- **The MP4 writer** produces fast-start files (moov before mdat), so clips play right away
  in browsers and Discord. Edit lists align audio and video exactly.
- **Failures are contained.** If capture or the encoder fails (driver reset, UAC prompt,
  resolution change), the pipeline restarts with backoff and the tray shows why. Stopping a
  session never blocks on a hung driver call for more than 4 seconds.

### Source layout

```
src/core/      platform-independent, unit-tested
  ring.rs        replay buffer of encoded packets
  h264.rs        Annex B → MP4 sample conversion, avcC
  mp4.rs         MP4 writer and reader
  clip.rs        ring → MP4, lossless trim
  settings.rs    settings file, bitrate/size rules
  games.rs       game detection rules and known-game list
  resample.rs    audio resampler
src/win/       Windows
  capture.rs     game-window capture (WGC), Desktop Duplication, GPU colour conversion
  encoder.rs     Media Foundation H.264 (hardware async MFTs, software fallback)
  audio.rs       WASAPI capture, mixing, AAC
  recorder.rs    recording session (threads, pacing, recovery)
  detect.rs      foreground window / process inspection
  app.rs         tray app: lifecycle, hotkey, saving, notifications
  settings_ui.rs, clips_ui.rs, trim_ui.rs, ui.rs   native Win32 windows
  selftest.rs    --selftest
```

## Roadmap

- Verify capture and encoding across NVIDIA, AMD and Intel GPUs; tune defaults from real
  measurements.
- Draw the mouse cursor (for desktop recording).
- HDR → SDR tone mapping.
- Export sized for Discord's upload limit.
- Separate audio tracks (game / microphone).
