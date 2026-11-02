Texture2DArray<float> source : register(t0);
RWTexture2D<float> target : register(u0);
[numthreads(8,8,1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint w,h; target.GetDimensions(w,h);
    if(id.x<w && id.y<h) target[id.xy]=source.Load(int4(id.xy,0,0));
}
