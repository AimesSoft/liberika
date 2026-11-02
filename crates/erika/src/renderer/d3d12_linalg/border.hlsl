// Exact folded-kernel ring. Keep this scalar loop out of the matrix PSO so
// its divergent long reduction does not inflate matrix-register lifetimes.
[numthreads(64,1,1)]
void main(uint3 group:SV_GroupID,uint thread:SV_GroupIndex) {
    uint index=(group.x+group.y*65535)*64+thread;
    uint rows=imageHeight>1 ? 2 : 1;
    uint sides=imageWidth>1 ? 2 : 1;
    uint innerHeight=imageHeight>2 ? imageHeight-2 : 0;
    uint perimeter=imageWidth*rows+innerHeight*sides;
    if(index>=perimeter*OUTPUT_CHANNELS)return;
    uint ch=index%OUTPUT_CHANNELS;
    uint pixel=index/OUTPUT_CHANNELS;
    int2 p;
    if(pixel<imageWidth) p=int2(pixel,0);
    else if(rows==2 && pixel<2*imageWidth) p=int2(pixel-imageWidth,imageHeight-1);
    else {
        pixel-=imageWidth*rows;
        if(pixel<innerHeight)p=int2(0,1+pixel);
        else p=int2(imageWidth-1,1+pixel-innerHeight);
    }
    if(p.x<int(outputX)||p.y<int(outputY)||p.x>=int(outputX+outputWidth)||p.y>=int(outputY+outputHeight))return;
    uint variant=border_variant(p);
    float value=0;
    [loop]for(uint k=0;k<REDUCTION;++k)
        value+=(float)(float16_t)fetch_at(p,k)*(float)(float16_t)weight(variant,ch,k);
    value=activate(ch,value+bias(variant,ch)+residual_at(p,ch));
    emit_at(p,ch,value);
}
