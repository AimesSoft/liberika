// Erika SM 6.10 implicit GEMM. The Rust generator supplies fetch(), weight(),
// bias(), activate(), residual(), and emit(), plus REDUCTION/OUTPUT_CHANNELS.
#include <dx/linalg.h>
using namespace dx::linalg;
#ifdef ACCUMULATE_HALF
#define ACC_TYPE ComponentType::F16
#else
#define ACC_TYPE ComponentType::F32
#endif
using MatA = Matrix<ComponentType::F16, 16, 16, MatrixUse::A, MatrixScope::Wave>;
using MatB = Matrix<ComponentType::F16, 16, 16, MatrixUse::B, MatrixScope::Wave>;
using MatC = Matrix<ACC_TYPE, 16, 16, MatrixUse::Accumulator, MatrixScope::Wave>;
groupshared float16_t tileA[256];
groupshared float16_t tileB[256];
#ifdef ACCUMULATE_HALF
// Matrix Store writes the matrix's component representation; it does not
// convert a half accumulator to float simply because the array is float.
groupshared float16_t tileC[256];
#else
groupshared float tileC[256];
#endif

[WaveSize(32)]
[numthreads(32, 1, 1)]
void main(uint3 group : SV_GroupID, uint lane : SV_GroupIndex) {
    MatC acc = MatC::Splat(0.0f);
    for (uint k = 0; k < REDUCTION; k += 16) {
        for (uint i = lane; i < 256; i += 32) {
            uint pixel = (group.x+group.z*65535) * 16 + i / 16;
            tileA[i] = (float16_t)(k + i % 16 < REDUCTION ? fetch(pixel, k + i % 16) : 0.0f);
            uint channel = group.y * 16 + i % 16;
            tileB[i] = (float16_t)(channel < OUTPUT_CHANNELS && k + i / 16 < REDUCTION
                ? weight(4, channel, k + i / 16) : 0.0f);
        }
        GroupMemoryBarrierWithGroupSync();
        MatA a = MatA::Load(tileA, 0, 16, MatrixLayout::RowMajor);
        MatB b = MatB::Load(tileB, 0, 16, MatrixLayout::RowMajor);
        acc.MultiplyAccumulate(a, b);
        GroupMemoryBarrierWithGroupSync();
    }
    acc.Store(tileC, 0, 16, MatrixLayout::RowMajor);
    GroupMemoryBarrierWithGroupSync();
    for (uint i = lane; i < 256; i += 32) {
        uint pixel = (group.x+group.z*65535) * 16 + i / 16;
        uint channel = group.y * 16 + i % 16;
        int2 p = pixel_position(pixel);
        if (pixel >= outputWidth * outputHeight || channel >= OUTPUT_CHANNELS ||
            any(p < 0) || any(p >= int2(imageWidth, imageHeight))) continue;
        uint variant = border_variant(p);
        float value = tileC[i];
        // Folded weights differ on the global one-pixel ring, never tile edges.
        // Recompute those pixels before consumers can observe this tensor.
        if (HAS_BORDER && variant != 4) {
            value = 0;
            for (uint k = 0; k < REDUCTION; ++k)
                value += (float)(float16_t)fetch(pixel, k) * (float)(float16_t)weight(variant, channel, k);
        }
        value = activate(channel, value + bias(variant, channel) + residual(pixel, channel));
        emit(pixel, channel, value);
    }
}
