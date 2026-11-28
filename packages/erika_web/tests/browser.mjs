// Run after building and serving target/web. Install playwright in a disposable
// tools directory; PLAYWRIGHT_MODULE can point at its index.mjs.
import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
const { chromium } = await import(
  process.env.PLAYWRIGHT_MODULE || "playwright"
);
const base = process.env.ERIKA_WEB_URL || "http://127.0.0.1:18765";
const browser = await chromium.launch({
  channel: process.env.ERIKA_BROWSER_CHANNEL || "chrome",
  headless: true,
  args: ["--autoplay-policy=no-user-gesture-required"],
});
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } }),
  errors = [],
  requests = [],
  fontRequests = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("request", (r) => {
  if (new URL(r.url()).pathname.endsWith("/subfont.ttf"))
    fontRequests.push(r.url());
  if (r.url().includes("/fixtures/"))
    requests.push({ url: r.url(), range: r.headers().range });
});
const report = { browser: await browser.version() };
const deadline = setTimeout(() => {
  console.error("Browser regression deadline exceeded");
  browser.close();
}, 90000);
page.on("console", (m) => {
  if (m.type() === "error") console.error(m.text());
});
try {
  await page.goto(base);
  await page.waitForFunction(
    () =>
      window.player &&
      document.querySelector("#status").textContent.includes("请选择"),
    null,
    { timeout: 60000 },
  );
  await page.evaluate(() => {
    window.engineErrors = [];
    player.addEventListener("log", (e) => console.log(e.detail));
    player.addEventListener("error", (e) =>
      engineErrors.push(String(e.detail)),
    );
  });
  await page.locator("#file").setInputFiles("target/web/fixtures/test.mkv");
  await page.waitForFunction(() => player.info, null, { timeout: 30000 });
  assert.equal(await page.evaluate(() => player.info.backend), "webcodecs");
  const activeRenderer = await page.evaluate(() => player.renderer);
  assert.ok(
    activeRenderer === "webgpu" || activeRenderer === "webgl2",
    `unexpected renderer: ${activeRenderer}`,
  );
  report.renderer = activeRenderer;
  if (activeRenderer === "webgpu") {
    const srModes = await page.evaluate(async () => {
      const results = [];
      for (const mode of ["fsr1", "anime4k_s"]) {
        results.push(await player.setUpscaler(mode));
      }
      return results;
    });
    report.upscalerInit = srModes;
    assert.equal(srModes[0].activeBackend, "compute");
    assert.equal(srModes[1].activeBackend, "compute");
  }
  await page.evaluate(async () => {
    await player.addDanmaku(
      JSON.stringify(
        Array.from({ length: 2000 }, (_, i) => ({
          time: i * 0.03,
          text: ["白色 White", "红色 Red", "黑色 Black"][i % 3] + i,
          color: ["#ffffff", "#ff0000", "#000000"][i % 3],
        })),
      ),
    );
    await player.play();
  });
  await page.waitForFunction(
    () =>
      player.currentTime > 1 &&
      player.stats.glyphs > 0 &&
      player.stats.subtitleImages > 0,
    null,
    { timeout: 15000 },
  );
  const before = await page.evaluate(() => ({
    ...player.stats,
    wall: performance.now(),
  }));
  await page.waitForTimeout(1000);
  const after = await page.evaluate(() => ({
    ...player.stats,
    wall: performance.now(),
  }));
  report.playback = { before, after };
  if (activeRenderer === "webgpu") {
    assert.ok(
      after.upscaler && after.upscaler.upscaledFrames > 0,
      `expected WebGPU SR upscaled frames, got ${JSON.stringify(after.upscaler)}`,
    );
    await page.evaluate(() => player.setUpscaler("off"));
  }
  assert.ok(after.decodedVideo > before.decodedVideo);
  assert.ok(after.decodedAudio > before.decodedAudio);
  assert.ok(after.time > before.time);
  assert.ok(after.maxVideoQueue <= 32);
  await page.evaluate(() => player.pause());
  const paused = await page.evaluate(() => player.currentTime);
  await page.waitForTimeout(150);
  assert.equal(await page.evaluate(() => player.currentTime), paused);
  const uploads = await page.evaluate(() => player.stats.atlasUploads);
  await page.evaluate(async () => {
    for (let i = 0; i < 30; i++) {
      await player.configureDanmaku({ opacity: 0.25 + i / 60 });
      await new Promise((r) => setTimeout(r, 16));
    }
  });
  await page.waitForTimeout(300);
  assert.equal(await page.evaluate(() => player.stats.atlasUploads), uploads);
  assert.ok((await page.evaluate(() => player.stats.glyphs)) > 0);
  await page.evaluate(async () => {
    for (let i = 0; i < 30; i++) {
      await player.configureDanmaku({ fontSize: 20 + i });
      await new Promise((r) => setTimeout(r, 16));
    }
  });
  await page.waitForTimeout(400);
  assert.ok((await page.evaluate(() => player.stats.glyphs)) > 0);
  await page.screenshot({ path: "target/web-tests/browser-local.png" });
  console.log("stage: seek 7");
  await page.evaluate(() => player.seek(7));
  await page.waitForFunction(
    () => player.stats.time === 7 && player.stats.subtitleImages > 0,
    null,
    { timeout: 10000 },
  );
  assert.equal(await page.evaluate(() => player.currentTime), 7);
  console.log("stage: disable subtitle");
  await page.evaluate(() => player.selectSubtitle(null));
  await page.waitForFunction(() => player.stats.subtitleImages === 0);
  console.log("stage: external subtitle");
  await page.evaluate(() =>
    player.addSubtitle("1\n00:00:00,000 --> 00:00:12,000\n外部 SRT 字幕\n"),
  );
  await page.waitForFunction(() => player.stats.subtitleImages > 0);
  await page.evaluate(() =>
    player.configureSubtitles({ supersampling: false }),
  );
  console.log("stage: rate");
  await page.evaluate(() => player.setRate(1.5));
  await page.evaluate(() => player.play());
  await page.waitForFunction(() => player.currentTime > 8, null, {
    timeout: 8000,
  });
  await page.evaluate(() => player.pause());
  console.log("stage: remote");
  // Remote MKV is over 8 MiB and must use HTTP ranges, including seeks.
  await page.evaluate((url) => player.open(url), base + "/fixtures/test.mkv");
  await page.evaluate(() => player.setRate(1));
  await page.evaluate(() => player.play());
  await page.waitForFunction(
    () => player.stats.decodedVideo > 0 && player.currentTime > 0.5,
    null,
    { timeout: 10000 },
  );
  await page.evaluate(() => player.seek(10));
  await page.waitForFunction(() => player.currentTime > 10.5, null, {
    timeout: 10000,
  });
  await page.waitForFunction(
    () => !player.playing && player.currentTime >= 12,
    null,
    { timeout: 10000 },
  );
  report.remote = await page.evaluate(() => ({
    time: player.currentTime,
    stats: player.stats,
  }));
  await page.evaluate((url) => player.open(url), base + "/fixtures/test.webm");
  await page.evaluate(() => player.play());
  await page.waitForFunction(
    () => player.currentTime > 1 && player.stats.hasVideo,
    null,
    { timeout: 10000 },
  );
  report.webm = await page.evaluate(() => ({
    info: player.info,
    stats: player.stats,
  }));
  // This source has no font attachment: both text backends must work using
  // the one font embedded in Rust, including after the previous media closes.
  assert.ok(!report.webm.info.tracks.some((track) => track.type === "attachment"));
  await page.evaluate(async () => {
    await player.addSubtitle("1\n00:00:00,000 --> 00:00:12,000\n共享回退字体：字幕 日本語\n");
    await player.addDanmaku('[{"time":0,"text":"共享回退字体：弹幕 日本語","type":"top"}]');
  });
  await page.waitForFunction(
    () => player.stats.subtitleImages > 0 && player.stats.glyphs > 0,
    null,
    { timeout: 10000 },
  );
  report.sharedFont = await page.evaluate(() => ({ ...player.stats }));
  await page.evaluate(() => player.pause());
  await page.screenshot({ path: "target/web-tests/browser-webm.png" });
  console.log("stage: audio tail");
  await page.evaluate(
    (url) => player.open(url),
    base + "/fixtures/audio-tail.mp4",
  );
  await page.evaluate(() => player.play());
  await page.waitForFunction(() => player.currentTime > 3, null, {
    timeout: 10000,
  });
  await page.evaluate(() => player.selectAudio(null));
  assert.equal(await page.evaluate(() => player.info.audioTrack), null);
  await page.evaluate(() => player.selectAudio(1));
  assert.equal(await page.evaluate(() => player.info.audioTrack), 1);
  report.errors = await page.evaluate(() => engineErrors);
  assert.deepEqual(report.errors, []);
  assert.deepEqual(errors, []);
  report.fontRequests = fontRequests;
  assert.deepEqual(fontRequests, [], "fallback font must not be downloaded separately");
  await page.evaluate(() => player.destroy());
  report.ranges = requests.filter((r) => r.range);
  assert.ok(report.ranges.length > 2);
  report.passed = true;
} finally {
  clearTimeout(deadline);
  await writeFile(
    "target/web-tests/browser-report.json",
    JSON.stringify(report, null, 2),
  );
  await browser.close();
}
console.log(JSON.stringify(report, null, 2));
