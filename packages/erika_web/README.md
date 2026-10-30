# liberika Web

An embeddable browser player for MKV with ASS subtitles, attached fonts and
danmaku. The backend is part of liberika and is integrated into Erika Web.

Download `liberika-web-0.3.2.zip` from the 0.3.2 Release, or
build from current source with `cargo run -p xtask -- web build`; deploy the
complete runtime in `target/web/`, preserving its relative paths. This source
directory alone is not a deployable package, and no npm release is available.

[Build, API and browser requirements](../../docs/web.md) ·
[中文说明](../../docs/web.zh.md)

The package includes the native Rust danmaku engine, patched libass, text
libraries and FFmpeg demuxer as WASM. Browser WebCodecs decodes audio/video.
Web Audio supplies audio output and the clock; WebGL2 composites video,
subtitles and danmaku into one canvas. Embedded ASS fonts load automatically,
and subtitle/danmaku supersampling is independent of the video resolution.
The complete fallback font is embedded once in the Rust WASM and supplied to
libass by the worker; no standalone font download is required.

```js
import { AudioSession, Player } from './liberika/index.js';

const audioSession = new AudioSession(); // page setup, before the first click
const player = await Player.create({ canvas, audioSession });
const info = await player.open(fileOrURL);
// In a user-gesture handler:
await player.play();
```

Preparing audio before a file picker or asynchronous host command preserves
Safari's user activation. `play()` requests playback; the `play` event confirms
it started. Pending audio activation does not block seek, pause or close.

`Player` also supports pause/seek, tracks, chapters, external subtitles,
additional fonts, appended danmaku files, live style settings and PNG capture.
Keep the instance and canvas across UI transitions; call `close()` to clear
media or `destroy()` when removing the player. See `index.d.ts` for the full API.

Serve over HTTPS or localhost. Remote media needs CORS when cross-origin;
large files need HTTP Range and an exposed `Content-Range`. Playback requires
WebCodecs video/audio decoding, OffscreenCanvas, WebGL2 and Web Audio.
Chrome regression and Safari 27.0.1 HEVC Main 10 / Opus / ASS playback and seeking
have been verified; detailed codec requirements and current limits are
in the browser guide above.
