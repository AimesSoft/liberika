// Runs the actual bridge with Node's N-API and an owner-checking C API double.
// This checks dispatch/lifetime logic; it does not validate OHOS SDK behavior.
const assert = require('node:assert/strict');
const { Worker } = require('node:worker_threads');
const native = require(process.argv[2]);
const legacyRuntime = process.argv[3] === 'legacy';
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const keepAlive = setInterval(() => {}, 1000);
(async () => {
  // The full config reaches the presenter; a legacy 3-argument call keeps
  // the default (opaque) video alpha mode.
  const config = id => JSON.parse(native.nativeInvoke(id, 'config', '{}')).value;
  for (const [args, expected] of [[[2, 4.0, 3, 1], [2, 4, 3, 1]], [[2, 4.0, 3], [2, 4, 3, 0]]]) {
    const created = native.nativeCreate(...args);
    assert.deepEqual(config(created), expected);
    native.nativeDestroy(created);
    const asyncCreated = await native.nativeCreateAsync(...args);
    assert.deepEqual(config(asyncCreated), expected);
    native.nativeDestroy(asyncCreated);
  }
  assert.equal(native.nativeCreate(2, 4.0), 0);
  assert.equal(await native.nativeCreateAsync(2, 4.0), 0);
  const id = await native.nativeCreateAsync(0, 1, 0, 0);
  const second = native.nativeCreate(0, 1, 0, 0);
  assert(id > 0 && second > id);
  assert.equal(await native.nativeAttachSurfaceAsync(id, 12n, 4, 4, 1), 0);
  assert.equal(await native.nativeResizeSurfaceAsync(id, 8, 8, 1), 0);
  assert.equal(JSON.parse(await native.nativeInvokeAsync(id, 'open', '{}')).value, 17);
  const modelStatus = legacyRuntime ? 3 : 0;
  assert.equal(native.nativeLoadUpscalerModel(second, 'test', new Uint8Array([1, 2, 3])), modelStatus);
  if (legacyRuntime) assert.match(native.nativeLastError(), /update the native library/);
  assert.equal(await native.nativeLoadUpscalerModelAsync(id, 'test', new Uint8Array([1, 2, 3])), modelStatus);
  if (legacyRuntime) assert.match(native.nativeLastError(), /update the native library/);
  assert.deepEqual(native.nativeRegisterSubtitleMemoryFont(second, new Uint8Array([1, 2, 3])), [0, 42]);
  // Block the owner first: the bridge must copy input bytes before returning.
  const slow = native.nativeInvokeAsync(id, 'slow', '{}');
  const bytes = new Uint8Array([1, 2, 3]);
  const font = native.nativeRegisterSubtitleMemoryFontAsync(id, bytes);
  bytes.fill(99);
  await slow; assert.deepEqual(await font, [0, 42]);
  assert.equal((await native.nativeCaptureFrameAsync(id, 2, 3)).byteLength, 24);
  assert.equal(native.nativeCaptureFrame(id, -1, 3), null);
  assert(JSON.parse(native.nativeRenderTick(id, 1)).ok);
  let events = 0;
  native.nativeStartFrames(id, raw => { assert(JSON.parse(raw).ok); events++; });
  await delay(130); assert(events > 0);
  native.nativeStopFrames(id);
  const stopped = events; await delay(80); assert.equal(events, stopped);
  // One explicit frame can be queued/running; all others merge into it.
  const ticks = await Promise.all(Array.from({ length: 1000 }, () => native.nativeRenderTickAsync(id, 2)));
  assert.equal(ticks.filter(value => value !== null).length, 1);
  // Join with accepted command results still queued to the JS task loop.
  const accepted = native.nativeInvokeAsync(id, 'slow', '{}');
  const cancelledFrame = native.nativeRenderTickAsync(id, 3);
  native.nativeStartFrames(id, () => { throw Error('stale event after destroy'); });
  native.nativeDestroy(id);
  assert.equal(JSON.parse(await accepted).value, 17);
  assert.equal(await cancelledFrame, null);
  native.nativeDestroy(id); // Idempotent; late calls cannot reach a new player.
  assert.equal(native.nativeDetachSurface(id), 1);
  const third = native.nativeCreate(0, 1, 0, 0); assert(third > second);
  native.nativeDestroy(second); native.nativeDestroy(third);
  // Environment cleanup while frames and a promise completion are in flight.
  await new Promise((resolve, reject) => {
    const worker = new Worker(`
      const n = require(${JSON.stringify(process.argv[2])});
      const id = n.nativeCreate(0, 1, 0, 0);
      n.nativeStartFrames(id, () => {});
      n.nativeInvokeAsync(id, 'slow', '{}');
    `, { eval: true });
    worker.on('error', reject); worker.on('exit', code => code ? reject(Error('worker exit ' + code)) : resolve());
  });
  console.log('N-API async/sync ownership, copied inputs, native clock/events, 1000 merged ticks, stale-event suppression, pending-result dispose and environment cleanup passed');
})().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => clearInterval(keepAlive));
