// Fixed pipeline-v3 observer: the unchanged 24-bit ID + checksum, 8x4 cells.
// Strong luminance contrast carries bits; chroma is measured as fidelity separately.
export const PROBE_VERSION = 'pipeline-v3';
export function decodeProbe(readPixel) {
  let bits=0;
  for(let bit=0;bit<32;bit++){
    const pixel=readPixel(36+(bit%8)*8,4+Math.floor(bit/8)*8);
    const luminance=.299*pixel[0]+.587*pixel[1]+.114*pixel[2];
    if(!(luminance<80||luminance>176))return null;
    if(luminance>176)bits=(bits|(1<<bit))>>>0;
  }
  const id=bits&0xffffff,check=(bits>>>24)&255;
  return check===((id&255)^((id>>>8)&255)^((id>>>16)&255)^0xa5)?id:null;
}
