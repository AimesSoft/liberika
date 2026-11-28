// A bounded range cache; large files are never copied wholesale into WASM.
export class Source {
  async open(input) {
    this.input = input;
    this.cache = null;
    this.cacheStart = 0;
    this.controller = new AbortController();
    if (input instanceof Blob) {
      this.size = input.size;
      this.name = input.name || "video.mkv";
      this.reader = new FileReaderSync();
      return;
    }
    this.name =
      new URL(input, globalThis.location.href).pathname.split("/").pop() ||
      "video";
    const response = await fetch(input, {
      headers: { Range: "bytes=0-0" },
      signal: this.controller.signal,
    });
    if (!response.ok)
      throw new Error(`Media request failed: HTTP ${response.status}`);
    const range = response.headers.get("Content-Range");
    if (response.status === 206 && range) {
      this.size = Number(range.split("/")[1]);
      await response.body?.cancel();
    } else {
      this.size = Number(response.headers.get("Content-Length"));
      if (!this.size || this.size > 8 * 1024 * 1024) {
        await response.body?.cancel();
        throw new Error(
          "Large remote files require HTTP Range and an exposed Content-Range header",
        );
      }
      this.cache = new Uint8Array(await response.arrayBuffer());
    }
    if (!Number.isSafeInteger(this.size) || this.size <= 0)
      throw new Error("Invalid media length");
  }
  async read(module, offset, destination, length) {
    length = Math.min(length, 2 * 1024 * 1024);
    if (this.input instanceof Blob) {
      const bytes = new Uint8Array(
        this.reader.readAsArrayBuffer(
          this.input.slice(offset, offset + length),
        ),
      );
      module.HEAPU8.set(bytes, destination);
      return bytes.length;
    }
    if (
      !this.cache ||
      offset < this.cacheStart ||
      offset + length > this.cacheStart + this.cache.length
    ) {
      const end = Math.min(
        this.size - 1,
        offset + Math.max(length, 2 * 1024 * 1024) - 1,
      );
      const response = await fetch(this.input, {
        headers: { Range: `bytes=${offset}-${end}` },
        signal: this.controller.signal,
      });
      if (response.status !== 206) {
        await response.body?.cancel();
        throw new Error("Server did not honor byte range");
      }
      const match = /^bytes (\d+)-(\d+)\/(\d+)$/.exec(
        response.headers.get("Content-Range") || "",
      );
      if (
        !match ||
        Number(match[1]) !== offset ||
        Number(match[2]) !== end ||
        Number(match[3]) !== this.size
      ) {
        await response.body?.cancel();
        throw new Error("Invalid Content-Range response");
      }
      this.cache = new Uint8Array(await response.arrayBuffer());
      this.cacheStart = offset;
      if (this.cache.length !== end - offset + 1)
        throw new Error("Truncated media range");
    }
    const bytes = this.cache.subarray(
      offset - this.cacheStart,
      offset - this.cacheStart + length,
    );
    module.HEAPU8.set(bytes, destination);
    return bytes.length;
  }
  close() {
    this.controller?.abort();
    this.cache = null;
    this.input = null;
  }
}
