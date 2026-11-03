// Copy decoded luma plane (slice zero of the caller's view) to a shared R32F
// texture. A shader copy also handles planar NV12/P010 plane SRVs correctly.
Texture2DArray<float> source : register(t0);
RWTexture2D<float> destination : register(u0);
[numthreads(8,8,1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint w,h; destination.GetDimensions(w,h);
    if (id.x<w && id.y<h) destination[id.xy]=source.Load(int4(id.xy,0,0));
}
