// GLS1/GLB1/GLB2/GLB3 lossless decoder. Errors preserve the reference frame.
const MAX_PIXELS = 16777216;
const HOST_LITTLE_ENDIAN = new Uint8Array(new Uint32Array([0x01020304]).buffer)[0] === 4;

async function inflate(bytes, max) {
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('deflate'));
  const reader = stream.getReader();
  const chunks = [];
  let size = 0;
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      size += value.length;
      if (size > max) throw new Error('Decompressed tile exceeds its limit');
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  }
  const output = new Uint8Array(size);
  let pos = 0;
  for (const chunk of chunks) { output.set(chunk, pos); pos += chunk.length; }
  return output;
}

export class Decoder {
  constructor() { this.reset(); }
  reset() { this.frame = null; this.sequence = null; }
  async decode(buffer) {
    if (!(buffer instanceof ArrayBuffer) || buffer.byteLength < 24 || buffer.byteLength > MAX_PIXELS*5+1048576) throw new Error('Invalid packet size');
    let view = new DataView(buffer);
    let bytes = new Uint8Array(buffer);
    const wireLength=bytes.length;
    let global=false;
    let sparse=false;
    if(bytes[0]===71&&bytes[1]===76&&bytes[2]===66&&(bytes[3]===49||bytes[3]===50||bytes[3]===51)){
      sparse=bytes[3]===51;global=bytes[3]!==49;
      const width=view.getUint16(12,true),height=view.getUint16(14,true),count=view.getUint16(16,true),length=view.getUint32(20,true);
      if(!width||!height||width*height>MAX_PIXELS||length<count*16||length>width*height*4+count*16)throw new Error('Invalid batch expansion size');
      if(global&&view.getUint16(18,true)>1)throw new Error('Invalid global flags');
      const body=await inflate(bytes.subarray(24),length);
      if(body.length!==length)throw new Error('Batch expansion length mismatch');
      let pos=0;
      for(let tile=0;tile<count;tile++){
        if(pos+16>length)throw new Error('Truncated batch tile');
        const codec=body[pos+8];if(global?!([0,2,5,6,7].includes(codec)||sparse&&codec===8):codec!==0&&codec!==5)throw new Error('Nested compression in batch');
        const size=new DataView(body.buffer,body.byteOffset+pos+12,4).getUint32(0,true);pos+=16+size;
        if(pos>length)throw new Error('Truncated batch body');
      }
      if(pos!==length)throw new Error('Trailing batch body');
      const expanded=new Uint8Array(24+length);expanded.set(bytes.subarray(0,24));expanded[2]=83;expanded[3]=49;new DataView(expanded.buffer).setUint32(20,0,true);expanded.set(body,24);
      if(!global)return{...await this.decode(expanded.buffer),bytes:bytes.length,batch:true};
      buffer=expanded.buffer;view=new DataView(buffer);bytes=expanded;
    }
    if (bytes[0] !== 71 || bytes[1] !== 76 || bytes[2] !== 83 || bytes[3] !== 49) throw new Error('Unknown wire version');
    const sequence = view.getUint32(4, true), inputId = view.getUint32(8, true);
    const width = view.getUint16(12, true), height = view.getUint16(14, true);
    const count = view.getUint16(16, true), flags = view.getUint16(18, true);
    const keyframe = Boolean(flags & 1);
    const pixels = width * height;
    if (!width || !height || pixels > MAX_PIXELS || flags > 1 || view.getUint32(20, true)) throw new Error('Invalid frame header');
    if (!keyframe && (!this.frame || this.frame.width !== width || this.frame.height !== height || sequence !== ((this.sequence + 1) >>> 0))) throw new Error('Missing reference frame');
    const previous = keyframe ? null : this.frame.pixels;
    const output = keyframe ? new Uint8ClampedArray(pixels * 4) : previous.slice();
    const coverage = new Uint8Array(pixels);
    let pos = 24, touched = 0;
    let left=width,top=height,right=0,bottom=0;
    for (let tile = 0; tile < count; tile++) {
      if (pos + 16 > bytes.length) throw new Error('Truncated tile header');
      const x = view.getUint16(pos, true), y = view.getUint16(pos + 2, true);
      const w = view.getUint16(pos + 4, true), h = view.getUint16(pos + 6, true);
      const codec = view.getUint8(pos + 8), length = view.getUint32(pos + 12, true);
      const allowed=global?[0,2,5,6,7].includes(codec)||sparse&&codec===8:codec<=5;
      if (!w || !h || x + w > width || y + h > height || !allowed || view.getUint8(pos+9) || view.getUint16(pos+10, true) || pos+16+length > bytes.length || global&&length>w*h*4) throw new Error('Invalid tile');
      left=Math.min(left,x);top=Math.min(top,y);right=Math.max(right,x+w);bottom=Math.max(bottom,y+h);
      touched += w*h;
      if (touched > pixels*2) throw new Error('Excessive tile coverage');
      pos += 16;
      const payload = bytes.subarray(pos, pos + length);
      pos += length;
      let decoded;
      if (codec === 0 || codec === 7) decoded = payload;
      else if (codec === 1 || codec === 4) decoded = await inflate(payload, w*h*4);
      else if (codec === 2 || codec === 3) {
        const body = codec === 3 ? await inflate(payload, 2+256*4+w*h) : payload;
        if (body.length < 2) throw new Error('Truncated palette');
        const colors = body[0] | (body[1] << 8);
        if (!colors || colors > 256 || body.length !== 2+colors*4+w*h) throw new Error('Invalid palette size');
        decoded = new Uint8Array(w*h*4);
        if(global){
          if(colors<2||colors>w*h)throw new Error('Invalid global palette');
          // Both word views use the host byte order, preserving RGBA on either
          // endian architecture without a subarray allocation for every pixel.
          const paletteBytes=body.slice(2,2+colors*4),palette=new Uint32Array(paletteBytes.buffer);
          const outputWords=new Uint32Array(decoded.buffer),used=new Uint8Array(colors);
          for(let p=0;p<w*h;p++){
            const index=body[2+colors*4+p];if(index>=colors)throw new Error('Invalid palette index');
            outputWords[p]=palette[index];used[index]=1;
          }
          if(used.includes(0))throw new Error('Unused global palette entry');
        }else{
          for (let p = 0; p < w*h; p++) {
            const index = body[2+colors*4+p];
            if (index >= colors) throw new Error('Invalid palette index');
            decoded.set(body.subarray(2+index*4, 6+index*4), p*4);
          }
        }
      } else if(codec===6){
        if(payload.length!==4)throw new Error('Invalid solid tile');
      } else if(codec===8){
        if(!previous||w*h>65536||payload.length<2)throw new Error('Invalid sparse patch bound');
        const patch=new DataView(payload.buffer,payload.byteOffset,payload.length),edits=patch.getUint16(0,true);
        if(!edits||edits>w*h||payload.length!==2+edits*6)throw new Error('Invalid sparse patch count or length');
        const words=new Uint32Array(output.buffer);let last=-1;
        for(let edit=0;edit<edits;edit++){
          const offset=2+edit*6,index=patch.getUint16(offset,true);
          if(index>=w*h||index<=last)throw new Error('Sparse offsets must be in-range and strictly increasing');
          last=index;const destination=(y+Math.floor(index/w))*width+x+index%w;
          words[destination]=patch.getUint32(offset+2,HOST_LITTLE_ENDIAN);
        }
      } else {
        if (!previous || payload.length !== 4) throw new Error('Invalid copy tile');
        const sx = payload[0] | (payload[1]<<8), sy = payload[2] | (payload[3]<<8);
        if (sx+w > width || sy+h > height) throw new Error('Copy outside reference frame');
        decoded = new Uint8Array(w*h*4);
        for (let row=0; row<h; row++) decoded.set(previous.subarray(((sy+row)*width+sx)*4, ((sy+row)*width+sx+w)*4), row*w*4);
      }
      if (codec!==6&&codec!==8&&decoded.length !== w*h*4) throw new Error('Invalid tile pixel length');
      if ((codec === 4||codec===7) && !previous) throw new Error('Keyframe contains a temporal tile');
      const solidWord=codec===6?new Uint32Array(payload.slice().buffer)[0]:0;
      for (let row=0; row<h; row++) {
        const dest = ((y+row)*width+x)*4, source=row*w*4;
        if(codec===6)new Uint32Array(output.buffer,dest,w).fill(solidWord);
        else if (codec === 4||codec===7) { for (let p=0; p<w*4; p++) output[dest+p] = previous[dest+p] ^ decoded[source+p]; }
        else if(codec!==8)output.set(decoded.subarray(source,source+w*4), dest);
        const begin=(y+row)*width+x,end=begin+w;
        for(let p=begin;p<end;p++){if(coverage[p])throw new Error('Overlapping tiles');coverage[p]=1;}
      }
    }
    if (pos !== bytes.length || (keyframe && coverage.includes(0))) throw new Error('Incomplete frame');
    this.sequence=sequence;
    this.frame={width,height,pixels:output};
    return { ...this.frame, sequence, inputId, keyframe, tiles:count, bytes:wireLength, ...(global?{batch:true,global:true}:{}), ...(sparse?{sparse:true}:{}), damage:count?{x:left,y:top,width:right-left,height:bottom-top}:null };
  }
}
