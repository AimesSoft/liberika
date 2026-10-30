# Browser playback: MKV, ASS fonts and danmaku

[中文](web.zh.md) | English | [日本語](web.ja.md)

liberika has a browser build containing the existing Rust danmaku engine, the
repository's patched libass with FreeType/HarfBuzz/FriBidi, and a reduced FFmpeg
build for container demuxing. `Player` owns the entire pipeline. Applications
supply a canvas and media source; no separate subtitle or danmaku library is
required.

The backend is integrated into the **Erika Web application**, including its
playback controls, track selection, chapters, screenshots and danmaku settings.
Release 0.3.0 includes `liberika-web-0.3.0.zip`, a complete browser runtime and
`index.html` demo with SHA-256 recorded in `SHA256SUMS`. Extract the whole archive
and serve it over HTTPS or localhost. It is separate from the native bundles
and is not published to npm; source builds remain available.

## MKV subtitles and attached fonts

The browser reads the original container. FFmpeg in WASM discovers its streams
and font attachments, feeds ASS/SSA headers and timed packets into libass, and
registers the attached font data. libass renders the script's fonts, positioning,
outlines and effects. A server can deliver the original MKV with HTTP Range;
this path needs no server-side subtitle extraction, text conversion or subtitle
burn-in. Local files use the same pipeline without a media server.

Subtitles and danmaku share the playback clock and final compositor. Subtitle
rendering uses libass; danmaku parsing, shaping, layout and collision avoidance
reuse the native Rust engine. Both remain inside liberika, so the host controls
one `Player` and one canvas. Subtitle and danmaku supersampling are both enabled
by default at 2x resolution, keeping their displayed size and the video resolution.
Disable them independently with `player.configureSubtitles({ supersampling: false })`
and `player.configureDanmaku({ supersampling: false })`; omitting the field preserves
the current setting.

| Stage | Browser implementation |
|---|---|
| Read and demux | Worker reads File/Blob chunks or HTTP ranges; FFmpeg WASM extracts packets, tracks, chapters and attachments |
| Decode audio/video | Browser WebCodecs receives compressed packets |
| Render subtitles | Patched libass + FreeType/HarfBuzz/FriBidi in WASM, including MKV font attachments |
| Render danmaku | Native Rust DFM engine compiled to WASM |
| Synchronize and present | Web Audio output and media clock; WebGL2 composes video, subtitles and danmaku |

Codec configuration is checked at runtime, including profile and dimensions.
`open(source, { hardwareDecode: false })` requests `prefer-software`; the default
tries `prefer-hardware`, then `no-preference`. These are browser preferences;
`info.hardwarePreference` reports the request, not the selected hardware.
Unsupported codecs produce an explicit error. The WASM build contains no
audio/video software decoder or server-transcoding fallback.

## Build

Prerequisites: Rust (see workspace rust-version), Node, Python 3, CMake, Meson,
Ninja, make, pkg-config, Emscripten **4.0.23**, wasm-bindgen-cli **0.2.126**.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
# Install locally; nothing is added to the shell's startup files.
git clone https://github.com/emscripten-core/emsdk.git target/web-tools/emsdk
target/web-tools/emsdk/emsdk install 4.0.23
target/web-tools/emsdk/emsdk activate 4.0.23
cargo run -p xtask -- web build
python3 scripts/serve_web.py
```

The build fetches the pinned native sources using the existing xtask dependency
manager, applies the same libass patches, compiles all text dependencies and
FFmpeg, builds the Rust WASM, and emits the complete package in `target/web/`.
An existing activated SDK can be supplied with `EMSDK` or `emcc` on PATH.
`web build --skip-native` rebuilds the bridge, Rust and browser package while
reusing previously built Web C libraries. Native dependency outputs remain in
their existing directories; Web libraries live in `target/web-build`.

Open `http://127.0.0.1:18765`. The example accepts local files, external subtitles
and Bilibili XML/Dandanplay JSON comments. `?src=URL` opens remote media. The
included localhost server supports Range requests. The generated directory is
a deployable package, not an npm publication; keep JS, WASM, workers and
license notices together. `build-manifest.json` records output SHA-256 hashes.

## Embed

Copy the generated runtime into your site's `liberika/` directory, preserving
its relative paths. The following page lets a user select a local MKV and then
start playback. Embedded ASS and font attachments are loaded automatically.

```html
<input id="media" type="file">
<button id="play" disabled>Play</button>
<canvas id="video" style="width:100%;height:60vh"></canvas>
<pre id="status"></pre>
<script type="module">
import { Player } from './liberika/index.js';
const fileInput = document.querySelector('#media');
const playButton = document.querySelector('#play');
const status = document.querySelector('#status');
const showError = error => { status.textContent = error.message; };
const player = await Player.create({ canvas: document.querySelector('#video') });
player.addEventListener('error', event => showError(event.detail));
fileInput.onchange = async () => {
  const file = fileInput.files[0];
  if (!file) return;
  fileInput.disabled = true;
  playButton.disabled = true;
  try {
    const info = await player.open(file); // File / Blob, or a CORS-accessible URL
    status.textContent = `${file.name}: ${info.duration.toFixed(1)} s`;
    playButton.disabled = false;
  } catch (error) {
    showError(error);
  } finally {
    fileInput.disabled = false;
  }
};
// Call play() from a user gesture to satisfy browser audio policy.
playButton.onclick = () => player.play().catch(showError);
// In your application's unmount handler: await player.destroy();
</script>
```

If player creation is deferred through a file picker, WASM, or a command queue,
prepare audio at page setup and pass that session to the player:

```js
import { AudioSession, Player } from './liberika/index.js';
const audioSession = new AudioSession(); // before the user's first click
// Later, once the canvas exists:
const player = await Player.create({ canvas, audioSession });
```

The session resumes audio synchronously on trusted input and keeps priming silent.
`play()` requests playback; the `play` event confirms the audio clock started.
Browser activation must never block the media command queue: seek, pause and
close still complete while audio permission is pending. `destroy()` also closes
the session and removes its input listeners. Each player owns one session.

For subtitles, danmaku and controls, call the same instance after `open()`:

```js
await player.addDanmaku(await danmakuFile.text()); // Bilibili XML / Dandanplay JSON
await player.addSubtitle(await subtitleFile.text()); // ASS / SSA / SRT / WebVTT
await player.configureDanmaku({ opacity: 0.8, fontSize: 30, delay: 0, supersampling: true });
await player.configureSubtitles({ scale: 1, supersampling: true });
```

| Operation | API and behavior |
|---|---|
| Playback | `play()`, `pause()`, `seek(seconds)`; seek retains the paused/playing state |
| Volume / rate | `setVolume(0…1)`, `setRate(0.25…4)` |
| Tracks | `info.tracks`, `selectAudio(id)`, `selectSubtitle(id)`; `null` disables audio/subtitles |
| Chapters | `info.chapters` supplies a `title` and `start`/`end` times in seconds (`end` may be `null`); seek to a chapter's `start` |
| External subtitles | `addSubtitle(text)` replaces the active subtitle script; the host retains scripts for external-track switching |
| Additional fonts | `addFont(name, bytes)` after opening media; embedded fonts already load automatically for ASS |
| Danmaku files | `addDanmaku(text)` appends comments and returns the number added by that call |
| Live local comment | `sendDanmaku(text)` inserts a comment at the current playback position; network publishing belongs to the host |
| Danmaku timing | `configureDanmaku({ delay: seconds })`, from −60 to 60; positive values delay comments |
| Screenshot | `capture(width?)` returns a PNG Blob of video with subtitles and danmaku |
| Close / reuse | `close()` clears the current media; `open()` reuses the player for another source |
| Dispose | `destroy()` releases the Worker, decoders, frames, GPU resources and AudioContext |

See `packages/erika_web/index.d.ts` for all options. `loaded`, `stats`, `timeupdate`,
`error` and `log` are CustomEvents with data in `detail`. `play`, `pause` and
`ended` are ordinary Events. Each player owns a dedicated worker, WASM instances,
video frames, GPU resources and an AudioContext. Reuse `open()` for another file;
call `destroy()` when removing a player. A canvas is transferred once and cannot
be reused for a new player instance.

Keep that instance and canvas mounted when opening menus or switching player
layouts. Coalesce pending slider updates in the host; opacity changes reuse
the glyph atlas, while font-size changes use the preview/settling path below.

## Deployment

The 2026-10-22 release build's playback runtime measures **10.93 MiB** as files
and **4.56 MiB** when each file is gzip-compressed at level 9. This includes
both WASM modules, their JavaScript runtime and the complete embedded fallback
font; it excludes the host UI, demo, documentation and media. Removing the
second font copy reduced these totals from 17.00 MiB and 7.30 MiB respectively.
Network transfer approaches the compressed size when the server enables gzip;
these figures are package sizes, not runtime memory usage.

- Serve the runtime over HTTPS or localhost, with JavaScript module MIME types
  and `application/wasm` for `.wasm`. Keep the JS, both WASM modules, workers
  and `licenses/` together. The generated `index.html` is an optional demo.
- Give the canvas a nonzero CSS width and height. `Player` observes its size
  and updates its drawing surface automatically.
- Remote media must allow browser requests. Files larger than 8 MiB require
  valid HTTP `206` byte-range responses with a known total length; cross-origin
  servers must expose `Content-Range` through `Access-Control-Expose-Headers`.
  The included `scripts/serve_web.py` supports Range for local testing.
- Deploy each runtime as a versioned directory, or revalidate its entry points
  and resources together, so cached JS does not load a different WASM build.
- SharedArrayBuffer and cross-origin isolation are not required. Start audio
  from a user gesture. Local media comes from a File/Blob chosen by the user.

## Erika Web integration

[Erika](https://github.com/MCDFsteve/Erika) uses this backend in its `web_app`
target. Its Rust application and Bern UI run in the browser; a Bern Web
extension adapts media commands to liberika's `Player`. The host keeps the same
canvas and worker for player menus and the mini player, passes local Files
directly to the worker, and awaits asynchronous subtitle/danmaku operations.

The extension build runs `cargo run -p xtask -- web build` and packages the
complete runtime with the application. See Erika's `web/player.js`,
`web/build.py` and `docs/web-playback.md` for the adapter and release workflow.
Websites can use the JavaScript API
above directly; Bern is only needed for the Erika host integration.

## Data flow and performance

- Local Blob data is read in the worker in small chunks. Remote files use a
  bounded 2 MiB range cache. Files larger than 8 MiB require HTTP Range; servers
  must expose `Content-Range` for cross-origin requests. The FFmpeg/libass WASM
  module's memory is capped at 512 MiB. No complete-video copy into a virtual
  filesystem is needed.
- Only compressed video packets cross from FFmpeg WASM into WebCodecs. Browser
  VideoFrame objects go directly to WebGL texture upload, without a CPU pixel
  readback into Rust/WASM. The browser controls the underlying GPU copy path.
- Decode queues are bounded and stale frames are closed on seek/open/dispose.
  Seek invalidates asynchronous output using a generation. ASS packets and
  font attachments are extracted by the same demuxer.
- The native Rust DFM engine performs comment parsing, layout, collision
  avoidance, shaping and glyph rasterization. The GPU unions one comment's
  masks and applies its opacity once; dark neutral colors use a white outline,
  saturated colors retain black outlines.
- Opacity updates reuse the atlas. Font-size changes preview using existing
  glyphs, then settle after 120 ms, with a 500 ms upper bound during dragging.
  Atlas updates use dirty rectangles when possible. Display ticks are
  acknowledged so a slow worker does not accumulate stale animation requests.
- Subtitles and danmaku can supersample independently of video. Unchanged ASS
  bitmaps retain their GPU textures. The complete fallback font is embedded
  once in `erika_core_bg.wasm`; the worker registers those bytes with libass on
  each media open. No separate `subfont.ttf` file or font request is needed,
  and incremental builds remove the former standalone asset. The fallback is
  available offline; applications can also provide their own font data.

## Current scope

This backend targets browsers exposing WebCodecs VideoDecoder **and**
AudioDecoder, OffscreenCanvas, WebGL2 and Web Audio in a secure context
(HTTPS or localhost). It uses one worker per player; SharedArrayBuffer and
cross-origin isolation are not required by this build.

Container readers include MP4/MOV, Matroska/WebM, MPEG-TS, AVI, FLV and Ogg.
Browser codec adapters cover H.264/HEVC configuration records, VP8/VP9/AV1, and
AAC/Opus/Vorbis/MP3/FLAC/ALAC; actual support must pass the runtime query.
Some container/codec combinations need additional bitstream conversion and
will be rejected if the necessary codec configuration is absent.

Embedded ASS/SSA and external ASS/SSA, SRT and WebVTT are supported. Plain SRT/VTT
is converted to ASS text; rich VTT positioning/HTML markup is not preserved.
Embedded ASS seeks use five seconds of preroll and retain already observed
events. A first seek into a long event that began before the preroll window can
miss that event; external full ASS scripts do not have this limitation.

Bitmap subtitles, embedded non-ASS subtitle conversion, DRM, live sources with
unknown length, desktop HDR/EDR/Dolby Vision processing and ArtCNN are not part
of this backend. Output is SDR. Playback-rate changes currently change pitch
through Web Audio.

## Verification

On 2026-10-22, the integrated Erika Web application passed browser regression
in Chrome **155.0.8059.85** on macOS using synthetic media. Coverage includes:

- H.264/AAC MKV with embedded ASS and a font attachment; VP9/Opus WebM.
- 2,000 danmaku comments, live opacity/size settings, subtitle switching,
  external SRT, subtitle supersampling and audio-track selection.
- Pause/seek/rate/volume, HTTP Range, container chapters and chapter seeking.
- Menu/mini-player transitions retaining the canvas/worker, PNG capture with
  actual pixel checks, close/reopen, resize and recovery after an invalid file.

The standalone regression also renders Chinese/Japanese subtitles and danmaku
on a WebM with no font attachments, checks that no separate fallback-font request
occurs, and verifies that the shared font bytes match the complete source asset.

Erika's regression lives in `web/browser.test.mjs`; its report and screenshots
are written to `target/web-review/` in that repository. On the same date, Safari
27.0.1 was manually verified with a 1080p HEVC Main 10 / Opus MKV containing two
ASS tracks and five font attachments: opening, subtitles, pause, chapter seeking
and repeated skips. This fixed audio activation blocking the command queue and
HEVC open-GOP RASL pictures causing decoder failure after random access.
Firefox has not yet been verified. Run liberika's standalone regression as follows:

```sh
node --test packages/erika_web/tests/*.test.mjs
python3 scripts/web_test_fixtures.py
python3 scripts/serve_web.py
# In another terminal; install Playwright in a disposable directory:
npm install --prefix target/web-tests playwright@1.58.2
PLAYWRIGHT_MODULE="$PWD/target/web-tests/node_modules/playwright/index.mjs" \
  node packages/erika_web/tests/browser.mjs
```

The browser regression uses installed Chrome by default. Set
`ERIKA_BROWSER_CHANNEL` for another installed Playwright channel and
`ERIKA_WEB_URL` for another local test URL. It checks actual decoded frames and
PCM, embedded/external subtitles, dense danmaku, live opacity/size changes,
pause/seek/rate, HTTP range playback, end of playback, reopen and cleanup.
Reports/screenshots are saved under `target/web-tests/`. Browser/codec support
outside tested configurations remains subject to runtime checks.
