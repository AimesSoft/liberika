// One group caches its input windows. Weights are cached, streamed through
// shared K blocks, or loaded directly, according to the selected geometry.
// Waves reuse fragments across MMAs without materializing device-memory im2col.
#include <dx/linalg.h>
using namespace dx::linalg;
#ifdef ACCUMULATE_HALF
#define ACC_TYPE ComponentType::F16
#else
#define ACC_TYPE ComponentType::F32
#endif
using MatA = Matrix<ComponentType::F16,16,16,MatrixUse::A,MatrixScope::Wave>;
using MatB = Matrix<ComponentType::F16,16,MATRIX_N,MatrixUse::B,MatrixScope::Wave>;
using MatA32 = Matrix<ComponentType::F16,16,32,MatrixUse::A,MatrixScope::Wave>;
using MatB32 = Matrix<ComponentType::F16,32,MATRIX_N,MatrixUse::B,MatrixScope::Wave>;
using MatA64 = Matrix<ComponentType::F16,16,64,MatrixUse::A,MatrixScope::Wave>;
using MatB64 = Matrix<ComponentType::F16,64,MATRIX_N,MatrixUse::B,MatrixScope::Wave>;
using MatC = Matrix<ACC_TYPE,16,MATRIX_N,MatrixUse::Accumulator,MatrixScope::Wave>;
// @DECLARATIONS@
[WaveSize(32)]
[numthreads(GROUP_THREADS,1,1)]
void main(uint3 group:SV_GroupID,uint thread:SV_GroupIndex) {
    uint wave=thread/32;
    int2 begin=int2(outputX+group.x*16,outputY+group.y*GROUP_HEIGHT);
    uint channelBase=group.z*CHANNEL_BLOCK;
    // @STAGE@
    GroupMemoryBarrierWithGroupSync();
    // @ACCUMULATORS@
    // @MULTIPLY@
    // @EPILOGUE@
}
