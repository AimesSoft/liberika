import test from "node:test";
import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import { initSync, WebDanmaku, fallbackFont } from "../../../target/web/erika_core.js";
import { supportedVideo, videoConfig, audioConfig, videoPacketGate } from "../codecs.js";
import { toASS } from "../index.js";
const wasmBytes = await readFile(
  new URL("../../../target/web/erika_core_bg.wasm", import.meta.url),
);
initSync({ module: wasmBytes });

test("HEVC random access skips only undecodable leading RASL pictures", () => {
  for (const lengthSize of [1, 2, 4]) {
    const extra = new Uint8Array(23); extra[21] = lengthSize - 1;
    const gate = videoPacketGate({ codec: "hevc", extra });
    const packet = (nal, type = "delta") => {
      const data = new Uint8Array(lengthSize + 2);
      data[lengthSize - 1] = 2;
      data[lengthSize] = nal << 1; data[lengthSize + 1] = 1;
      return { type, data };
    };
    assert.equal(gate(packet(1)), false, "wait for the first random access point");
    assert.equal(gate(packet(21, "key")), true, "CRA is decodable");
    assert.equal(gate(packet(39)), true, "keep prefix SEI");
    assert.equal(gate(packet(9)), false);
    assert.equal(gate(packet(8)), false);
    assert.equal(gate(packet(6)), true, "RADL is decodable");
    assert.equal(gate(packet(1)), true);
    assert.equal(gate(packet(21, "key")), true);
    assert.equal(gate(packet(9)), true, "later GOPs have their references");
  }
  const avc = videoPacketGate({ codec: "h264", extra: new Uint8Array() });
  assert.equal(avc({type:"delta"}), false);
  assert.equal(avc({type:"key"}), true);
  assert.equal(avc({type:"delta"}), true);
});

test("the complete fallback font is packaged once and safely shared with libass", async () => {
  const font = await readFile(
    new URL("../../../crates/erika/assets/subfont.ttf", import.meta.url),
  );
  const offset = wasmBytes.indexOf(font);
  assert.ok(offset >= 0, "Rust WASM must retain the complete fallback font");
  assert.equal(wasmBytes.indexOf(font, offset + 1), -1);
  const shared = fallbackFont();
  assert.ok(shared instanceof Uint8Array);
  assert.deepEqual(Buffer.from(shared), font);
  shared.fill(0);
  assert.deepEqual(Buffer.from(fallbackFont()), font, "JS must not mutate Rust's font");
  await assert.rejects(
    access(new URL("../../../target/web/subfont.ttf", import.meta.url)),
    { code: "ENOENT" },
  );
});

test("browser danmaku preserves atlas on paint changes and previews size immediately", () => {
  const d = new WebDanmaku();
  try {
    assert.equal(
      d.load(
        JSON.stringify([
          { time: 0, text: "白色 White", type: "top", color: "#ffffff" },
        ]),
      ),
      1,
    );
    const first = d.frame(0.5, 1280, 720, 1, 0);
    assert.ok(first.glyphs.length > 0);
    assert.ok(first.fill.length > 0);
    d.configure(JSON.stringify({ opacity: 0.4 }), 5);
    const opacity = d.frame(0.5, 1280, 720, 1, 6);
    assert.equal(opacity.fill, undefined);
    assert.deepEqual(opacity.glyphs, first.glyphs);
    assert.ok(Math.abs(opacity.groups[5] - 0.4) < 1e-6);
    d.configure(JSON.stringify({ fontSize: 60 }), 10);
    const preview = d.frame(0.5, 1280, 720, 1, 11);
    assert.equal(preview.fill, undefined);
    assert.ok(preview.glyphs[2] > first.glyphs[2]);
    const final = d.frame(0.5, 1280, 720, 1, 150);
    assert.ok(final.glyphs.length > 0);
    assert.ok(final.fill.length > 0);
  } finally {
    d.free();
  }
});
test("black comments use white outline, saturated red keeps black outline", () => {
  const d = new WebDanmaku();
  try {
    d.load(
      JSON.stringify([
        { time: 0, text: "黑", type: "top", color: "#000000" },
        { time: 0, text: "红", type: "bottom", color: "#ff0000" },
      ]),
    );
    const p = d.frame(1, 1280, 720, 1, 0);
    assert.equal(p.groups.length, 32);
    for (let i = 0; i < 32; i += 16) {
      const black = p.groups[i + 2] === 0;
      assert.equal(p.groups[i + 6], black ? 1 : 0);
    }
  } finally {
    d.free();
  }
});
test("seek resets placement and unsupported inputs fail explicitly", () => {
  const d = new WebDanmaku();
  try {
    assert.throws(() => d.load("{"));
    d.load('[{"time":50,"text":"later"}]');
    assert.equal(d.frame(0, 1280, 720, 1, 0).glyphs.length, 0);
    d.reset();
    assert.ok(d.frame(51, 1280, 720, 1, 1).glyphs.length > 0);
    assert.throws(() => d.frame(NaN, 1280, 720, 1, 0));
  } finally {
    d.free();
  }
});
test("capability selection falls back to browser-managed decode, never assumes hardware", async () => {
  const calls = [];
  const decoder = {
    isConfigSupported: async (c) => {
      calls.push(c.hardwareAcceleration);
      return {
        supported: c.hardwareAcceleration === "no-preference",
        config: c,
      };
    },
  };
  const result = await supportedVideo(
    {
      codec: "h264",
      extra: new Uint8Array([1, 100, 0, 40]),
      width: 1920,
      height: 1080,
    },
    decoder,
  );
  assert.deepEqual(calls, ["prefer-hardware", "no-preference"]);
  assert.equal(result.codec, "avc1.640028");
  assert.equal(result.hardwareAcceleration, "no-preference");
  await assert.rejects(
    () => supportedVideo({ codec: "mpeg4", extra: new Uint8Array() }, decoder),
    /unavailable/,
  );
});
test("audio codec configuration and external subtitle conversion", () => {
  assert.equal(
    audioConfig({
      codec: "aac",
      extra: new Uint8Array([0x12, 0x10]),
      sampleRate: 44100,
      channels: 2,
    }).codec,
    "mp4a.40.2",
  );
  const ass = toASS("1\n00:00:01,100 --> 00:00:03,200\n你好\n第二行\n");
  assert.match(ass, /0:00:01.10,0:00:03.20/);
  assert.match(ass, /你好\\N第二行/);
  assert.equal(toASS(ass), ass);
  assert.throws(() => toASS("invalid"));
});

test('appending comments preserves mounted tracks and invalid input is atomic', () => {
  const d = new WebDanmaku();
  try {
    d.load('[{"time":0,"text":"first","type":"top"}]');
    assert.equal(d.append('[{"time":0,"text":"second","type":"bottom"}]'), 1);
    assert.equal(d.frame(1, 1280, 720, 1, 0).groups.length, 32);
    assert.throws(() => d.append('{'));
    assert.equal(d.frame(1, 1280, 720, 1, 1).groups.length, 32);
    d.send('own comment', 0, 0xfe0502);
    // The viewer's comment adds its red text and its white border.
    const own = d.frame(1, 1280, 720, 1, 2).groups;
    assert.equal(own.length, 64);
    const paints = [];
    for (let i = 0; i < own.length; i += 16) paints.push(Array.from(own.subarray(i + 2, i + 5)));
    assert.ok(paints.some(([r, g, b]) => r === 1 && g === 1 && b === 1));
    assert.ok(paints.some(([r, g, b]) => Math.abs(r - 0xfe / 255) < 1e-6 && Math.abs(g - 5 / 255) < 1e-6 && Math.abs(b - 2 / 255) < 1e-6));
    d.load('[]');
    assert.equal(d.frame(1, 1280, 720, 1, 3).groups.length, 0);
  } finally { d.free(); }
});

test('disabling hardware decode asks only for software decoding', async () => {
  const calls = [];
  const decoder = { isConfigSupported: async config => {
    calls.push(config.hardwareAcceleration);
    return { supported: true, config };
  }};
  await supportedVideo({codec:'vp8',extra:new Uint8Array(),width:640,height:360},decoder,false);
  assert.deepEqual(calls,['prefer-software']);
});
