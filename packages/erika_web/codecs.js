export function videoConfig(track) {
  const e = track.extra,
    hex = (n) => n.toString(16).padStart(2, "0");
  let codec;
  switch (track.codec) {
    case "h264":
      if (e.length < 4 || e[0] !== 1)
        throw new Error("H.264 needs AVC configuration data");
      codec = "avc1." + [e[1], e[2], e[3]].map(hex).join("");
      break;
    case "hevc": {
      if (e.length < 13 || e[0] !== 1)
        throw new Error("HEVC needs hvcC configuration data");
      const space = ["", "A", "B", "C"][e[1] >> 6],
        profile = e[1] & 31;
      let compatibility = 0;
      for (let bit = 0; bit < 32; bit++)
        if (e[2 + (bit >> 3)] & (1 << (7 - (bit & 7))))
          compatibility = (compatibility | (1 << bit)) >>> 0;
      let constraints = Array.from(e.subarray(6, 12));
      while (constraints.length && constraints.at(-1) === 0) constraints.pop();
      codec = `hvc1.${space}${profile}.${compatibility.toString(16)}.${e[1] & 32 ? "H" : "L"}${e[12]}${constraints.length ? "." + constraints.map(hex).join(".") : ""}`;
      break;
    }
    case "vp8":
      codec = "vp8";
      break;
    case "vp9":
      codec = `vp09.${String(Math.max(0, track.profile)).padStart(2, "0")}.10.${String(track.bitDepth || 8).padStart(2, "0")}`;
      break;
    case "av1": {
      const profile = e.length >= 3 ? e[1] >> 5 : 0,
        level = e.length >= 3 ? e[1] & 31 : 8,
        depth =
          e.length >= 3
            ? e[2] & 64
              ? 12
              : e[2] & 32
                ? 10
                : 8
            : track.bitDepth || 8;
      codec = `av01.${profile}.${String(level).padStart(2, "0")}${e.length >= 3 && e[2] & 128 ? "H" : "M"}.${String(depth).padStart(2, "0")}`;
      break;
    }
    default:
      throw new Error(
        `Browser video codec mapping unavailable: ${track.codec}`,
      );
  }
  const config = {
    codec,
    codedWidth: track.width,
    codedHeight: track.height,
    hardwareAcceleration: "prefer-hardware",
  };
  if (e.length && !["vp8", "vp9"].includes(track.codec)) config.description = e;
  return config;
}
export function audioConfig(track) {
  const codecs = {
    aac: "mp4a.40.2",
    opus: "opus",
    vorbis: "vorbis",
    mp3: "mp3",
    flac: "flac",
    alac: "alac",
  };
  let codec = codecs[track.codec];
  if (!codec)
    throw new Error(`Browser audio codec mapping unavailable: ${track.codec}`);
  if (track.codec === "aac" && track.extra.length)
    codec = `mp4a.40.${track.extra[0] >> 3}`;
  const config = {
    codec,
    sampleRate: track.sampleRate,
    numberOfChannels: track.channels,
  };
  if (track.extra.length) config.description = track.extra;
  return config;
}
export async function supportedVideo(track, Decoder = globalThis.VideoDecoder, hardware = true) {
  if (!Decoder)
    throw new Error("This browser does not expose WebCodecs VideoDecoder");
  const config = videoConfig(track);
  for (const hardwareAcceleration of (hardware ? ["prefer-hardware", "no-preference"] : ["prefer-software"])) {
    const result = await Decoder.isConfigSupported({
      ...config,
      hardwareAcceleration,
    });
    if (result.supported) return result.config;
  }
  throw new Error(
    `Browser cannot decode ${config.codec} (${track.width}×${track.height})`,
  );
}
export async function supportedAudio(track, Decoder = globalThis.AudioDecoder) {
  if (!Decoder)
    throw new Error("This browser does not expose WebCodecs AudioDecoder");
  const config = audioConfig(track),
    result = await Decoder.isConfigSupported(config);
  if (!result.supported)
    throw new Error(`Browser cannot decode audio ${config.codec}`);
  return result.config;
}

/** A fresh decoder starts at random access. HEVC open GOPs may put RASL
 * pictures after a CRA in decode order; they reference the previous GOP and
 * must be discarded on random access (NoRaslOutputFlag), not sent to Safari's
 * VideoToolbox decoder. Subsequent GOPs are decoded normally. */
export function videoPacketGate(track) {
  let waitingForKey = true, leading = false;
  const hevc = track.codec === "hevc";
  const lengthSize = hevc && track.extra.length > 21 ? (track.extra[21] & 3) + 1 : 4;
  return (packet) => {
    if (waitingForKey) {
      if (packet.type !== "key") return false;
      waitingForKey = false;
      leading = hevc;
      return true;
    }
    if (!leading) return true;
    const data = packet.data;
    for (let offset = 0; offset + lengthSize < data.length;) {
      let size = 0;
      for (let i = 0; i < lengthSize; i++) size = size * 256 + data[offset++];
      if (size < 2 || offset + size > data.length) break;
      const type = (data[offset] >> 1) & 63;
      if (type === 8 || type === 9) return false; // RASL_N / RASL_R
      if (type <= 31 && type !== 6 && type !== 7) leading = false;
      offset += size;
    }
    return true;
  };
}
