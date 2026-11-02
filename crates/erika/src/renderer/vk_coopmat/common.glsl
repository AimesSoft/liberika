#extension GL_EXT_shader_16bit_storage : require
#extension GL_EXT_shader_explicit_arithmetic_types_float16 : require

layout(set=0, binding=0, std430) buffer Arena { float16_t features[]; };
layout(set=0, binding=1, std430) readonly buffer Weights { float16_t weights[]; };
layout(set=0, binding=2, std430) readonly buffer Metadata { uint data[]; };
layout(set=0, binding=3, r32f) uniform readonly image2D luma;
layout(set=0, binding=4, r16f) uniform writeonly image2D result;
layout(push_constant) uniform Params {
    uvec4 size; // image width/height, arena tile width/height
    ivec4 origin; // arena origin, core origin
    uvec4 core; // core width/height, layer header offset, clamp padding
    vec4 norm; // input offset, input scale
    uvec4 region; // local output x/y, width/height (shrinks with tensor halo)
} p;

#ifdef SPECIALIZED_LAYER
layout(constant_id=5) const uint H0=0;
layout(constant_id=6) const uint H1=0;
layout(constant_id=7) const uint H2=0;
layout(constant_id=8) const uint H3=0;
layout(constant_id=9) const uint H4=0;
layout(constant_id=10) const uint H5=0;
layout(constant_id=11) const uint H6=0;
layout(constant_id=12) const uint H7=0;
layout(constant_id=13) const uint H8=0;
layout(constant_id=14) const uint H9=0;
layout(constant_id=15) const uint H10=0;
layout(constant_id=16) const uint H11=0;
layout(constant_id=17) const uint H12=0;
layout(constant_id=18) const uint H13=0;
layout(constant_id=19) const uint H14=0;
layout(constant_id=20) const uint H15=0;
uint h(uint i) {
    const uint header[16]=uint[](H0,H1,H2,H3,H4,H5,H6,H7,H8,H9,H10,H11,H12,H13,H14,H15);
    return header[i];
}
#else
uint h(uint i) { return data[p.core.z+i]; }
#endif
uint plane() { return p.size.z*p.size.w; }
// [channel block][pixel][16 channels], with tensor bases aligned to 16.
uint feature_index(uint channel,uint pixel) {
    return (channel/16)*plane()*16+pixel*16+channel%16;
}
bool inside(ivec2 q) { return all(greaterThanEqual(q,ivec2(0))) && all(lessThan(q,ivec2(p.size.xy))); }
float fetch(uint channel, ivec2 q) {
    if (channel == 0xffffffffu) return 0.0;
    if (p.core.w != 0) q=clamp(q,ivec2(0),ivec2(p.size.xy)-1);
    else if (!inside(q)) return 0.0;
    q-=p.origin.xy;
    if (any(lessThan(q,ivec2(0))) || any(greaterThanEqual(q,ivec2(p.size.zw)))) return 0.0;
    return float(features[feature_index(channel,uint(q.y)*p.size.z+uint(q.x))]);
}
float operand(uint k, ivec2 q) {
    uint d=h(4)+4*k;
    float v=fetch(data[d],q+ivec2(int(data[d+1]),int(data[d+2])));
    uint t=data[d+3];
    return t==1 ? max(v,0.0) : t==2 ? max(-v,0.0) : v;
}
float raw(ivec2 q) { return imageLoad(luma,clamp(q,ivec2(0),ivec2(p.size.xy)-1)).x; }
float base(ivec2 q, ivec2 sub) {
    vec2 at=vec2(q)-0.25+0.5*vec2(sub);
    ivec2 lo=ivec2(floor(at)); vec2 f=fract(at);
    return mix(mix(raw(lo),raw(lo+ivec2(1,0)),f.x),
               mix(raw(lo+ivec2(0,1)),raw(lo+ivec2(1,1)),f.x),f.y);
}
void epilogue(float v, uint channel, uint pixel, uint variant) {
    if (pixel>=plane()) return;
    uvec2 at=uvec2(pixel%p.size.z,pixel/p.size.z);
    if(any(lessThan(at,p.region.xy)) || any(greaterThanEqual(at,p.region.xy+p.region.zw))) return;
    if (channel>=h(1)) {
        if ((h(12)&1u)==0) features[feature_index(h(0)+channel,pixel)]=float16_t(0.0);
        return;
    }
    ivec2 q=p.origin.xy+ivec2(pixel%p.size.z,pixel/p.size.z);
    if (!inside(q)) {
        if ((h(12)&1u)==0) features[feature_index(h(0)+channel,pixel)]=float16_t(0.0);
        return;
    }
    v+=uintBitsToFloat(data[h(5)+variant*h(11)+channel]);
    if(h(6)!=0xffffffffu) v+=fetch(h(6)+channel,q);
    uint a=h(7);
    if(a==1) v=max(v,0.0);
    else if(a==2) v=v/(1.0+exp(-v));
    else if(a==3) v=max(v,0.0)+uintBitsToFloat(data[h(8)+channel])*min(v,0.0);
    if ((h(12)&1u)==0) features[feature_index(h(0)+channel,pixel)]=float16_t(v);
    else if(all(greaterThanEqual(q,p.origin.zw)) && all(lessThan(q,p.origin.zw+ivec2(p.core.xy)))) {
        ivec2 sub=(h(12)&2u)!=0 ? ivec2(channel/2,channel%2) : ivec2(channel%2,channel/2);
        v=v/p.norm.y+((h(12)&4u)!=0 ? base(q,sub) : p.norm.x);
        if((h(12)&8u)!=0) v=clamp(v,0.0,1.0);
        imageStore(result,q*2+sub,vec4(v));
    }
}
