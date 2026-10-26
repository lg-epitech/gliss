// Fixed pipeline-v2 observation marker: 24-bit ID + byte checksum, 8x4 cells.
// A cell must be strongly dark/light and nearly gray; uncertain/lossy reads fail.
export function decodeProbe(readPixel) {
  let bits=0;
  for(let bit=0;bit<32;bit++){
    const pixel=readPixel(36+(bit%8)*8,4+Math.floor(bit/8)*8);
    const low=Math.min(pixel[0],pixel[1],pixel[2]),high=Math.max(pixel[0],pixel[1],pixel[2]);
    if(high-low>24||!(high<80||low>176))return null;
    if(low>176)bits=(bits|(1<<bit))>>>0;
  }
  const id=bits&0xffffff,check=(bits>>>24)&255;
  return check===((id&255)^((id>>>8)&255)^((id>>>16)&255)^0xa5)?id:null;
}
