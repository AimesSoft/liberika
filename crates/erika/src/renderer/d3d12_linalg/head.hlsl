// Four-channel head: constant-buffer FP32 FMAs on fp16-quantized operands.
// Low output width underfills wave MMA tiles. Four pixels per thread share
// register windows; channel phases bound the fp32 shared-memory footprint.
cbuffer HeadWeights : register(b1) { float4 HW[REDUCTION]; };
// @DECLARATIONS@
[numthreads(HEAD_COLS,HEAD_ROWS,1)]
void main(uint3 thread:SV_GroupThreadID,uint3 group:SV_GroupID) {
    uint lane=thread.y*HEAD_COLS+thread.x;
    int2 begin=int2(outputX+group.x*HEAD_COLS*2,outputY+group.y*HEAD_ROWS*2);
    int2 local=int2(thread.x*2,thread.y*2);
    float4 a0=0, a1=0, a2=0, a3=0;
    // @MULTIPLY@
    // @EPILOGUE@
}
