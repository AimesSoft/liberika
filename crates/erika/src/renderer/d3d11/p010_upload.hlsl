RWTexture2D<uint> y : register(u0);

[numthreads(16, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint width, height;
    y.GetDimensions(width, height);
    if (id.x >= width || id.y >= height) return;

    const uint2 p = id.xy;
    const uint code = y.Load(p);
    y[p] = (code & 1023u) << 6;
}
