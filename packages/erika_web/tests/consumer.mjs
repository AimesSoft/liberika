// Checks the npm package the way a site consumes it. Serve the Vite build of
// tests/consumer (with the fixtures copied to its fixtures/) at ERIKA_WEB_URL
// and the staged package itself, with CORS, at ERIKA_WEB_CDN_URL.
import assert from "node:assert/strict";
const { chromium } = await import(
  process.env.PLAYWRIGHT_MODULE || "playwright"
);
const base = process.env.ERIKA_WEB_URL || "http://127.0.0.1:18766",
  cdn = process.env.ERIKA_WEB_CDN_URL || "http://127.0.0.1:18767";
const browser = await chromium.launch({
  channel: process.env.ERIKA_BROWSER_CHANNEL || "chrome",
  headless: true,
  args: ["--autoplay-policy=no-user-gesture-required"],
});
const deadline = setTimeout(() => {
  console.error("npm consumer deadline exceeded");
  browser.close();
}, 90000);

// Opens the MKV, then waits until video, its ASS subtitles and danmaku are all
// on screen: the worker, both WASM files and their URLs survived the trip.
async function plays(page) {
  await page.evaluate(async (media) => {
    window.engineErrors = [];
    player.addEventListener("error", (e) => engineErrors.push(String(e.detail)));
    await player.open(media);
    await player.addDanmaku(
      JSON.stringify(
        Array.from({ length: 200 }, (_, i) => ({ time: i * 0.05, text: "弹幕" + i })),
      ),
    );
    await player.play();
  }, `${base}/fixtures/test.mkv`);
  await page.waitForFunction(
    () =>
      player.currentTime > 1 &&
      player.stats.glyphs > 0 &&
      player.stats.subtitleImages > 0,
    null,
    { timeout: 30000 },
  );
  assert.equal(await page.evaluate(() => player.info.backend), "webcodecs");
  assert.deepEqual(await page.evaluate(() => engineErrors), []);
  await page.evaluate(() => player.destroy());
}

async function scenario(name, prepare) {
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } }),
    errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("console", (m) => {
    if (m.type() === "error") console.error(`[${name}] ${m.text()}`);
  });
  await page.goto(base);
  await page.evaluate(() => playerReady);
  await prepare(page);
  await plays(page);
  assert.deepEqual(errors, []);
  await page.close();
  console.log(`ok - ${name}`);
}

try {
  await scenario("bundled by Vite", async () => {});
  // A page on one origin importing the package from another, as from a CDN.
  await scenario("imported across origins", async (page) => {
    const workers = page.workers().length;
    await page.evaluate(async (url) => {
      await player.destroy();
      const canvas = document.createElement("canvas");
      canvas.style.cssText = "width: 640px; height: 360px";
      document.body.append(canvas);
      const { Player } = await import(url);
      window.player = await Player.create({ canvas });
    }, `${cdn}/index.js`);
    assert.ok(page.workers().length >= workers);
    assert.ok(page.workers().some((w) => w.url().startsWith("blob:")));
  });
  // The host serves worker.js itself and says where.
  await scenario("explicit workerURL", async (page) => {
    await page.evaluate(async (url) => {
      await player.destroy();
      const canvas = document.createElement("canvas");
      canvas.style.cssText = "width: 640px; height: 360px";
      document.body.append(canvas);
      const { Player } = await import(`${url}/index.js`);
      window.player = await Player.create({ canvas, workerURL: `${url}/worker.js` });
    }, `${base}/liberika`);
    assert.ok(page.workers().some((w) => w.url() === `${base}/liberika/worker.js`));
  });
} finally {
  clearTimeout(deadline);
  await browser.close();
}
