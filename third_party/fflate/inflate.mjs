// Component research only. No live codec/default changes.
import {glissRawInflate} from './fflate-bounded-core.mjs';
const require=(ok,message)=>{if(!ok)throw new Error(message);};
const BASE=65521;
export function adler32(bytes){let a=1,b=0;for(const value of bytes){a+=value;b+=a;}return (((b%BASE)<<16)|(a%BASE))>>>0;}
export function combineAdler(first,second,length){
  require(Number.isInteger(length)&&length>=0&&length<=262144,'checksum length');
  const a1=first&65535,a2=second&65535,b1=first>>>16,b2=second>>>16;
  const mod=value=>(value%BASE+BASE)%BASE;
  return ((mod(b1+b2+length*(a1-1))<<16)|mod(a1+a2-1))>>>0;
}
const footer=bytes=>new DataView(bytes.buffer,bytes.byteOffset+bytes.length-4,4).getUint32(0,false);
export function boundedFflate(body,limit){
  require(body instanceof Uint8Array&&body.length>=8&&body.length<=4096&&Number.isInteger(limit)&&limit>0&&limit<=4096,'inflate bounds');
  require((body[0]&15)===8&&(body[0]>>>4)<=7&&!(body[1]&32)&&((body[0]<<8)|body[1])%31===0,'zlib header');
  const raw=body.subarray(2,-4),out=new Uint8Array(limit),{value,state}=glissRawInflate(raw,out,1<<(8+(body[0]>>>4)));
  require(state.b===limit&&value.length===limit&&state.f===1&&!state.l&&Math.ceil(state.p/8)===raw.length,'exact deflate extent');
  require(adler32(value)===footer(body),'zlib checksum');return value;
}
export async function boundedStream(body,limit){
  require(body instanceof Uint8Array&&body.length>0&&body.length<=262155&&Number.isInteger(limit)&&limit>0&&limit<=262144,'stream bounds');
  const reader=new Blob([body]).stream().pipeThrough(new DecompressionStream('deflate')).getReader();
  const out=new Uint8Array(limit);let offset=0;
  try{for(;;){const{done,value}=await reader.read();if(done)break;require(offset+value.length<=limit,'stream output cap');out.set(value,offset);offset+=value.length;}}
  catch(error){await reader.cancel().catch(()=>{});throw error;}finally{reader.releaseLock();}
  require(offset===limit,'stream output exact length');return out;
}
// Each part is an independent nonfinal raw-DEFLATE full-flush stream followed
// by Adler32 of its own uncompressed bytes. Only completed tiles enter a batch.
export async function inflateParts(parts){
  require(Array.isArray(parts)&&parts.length>0&&parts.length<=64,'batch count');
  let size=11,raw=0,combined=1;
  for(const part of parts){require(part.body instanceof Uint8Array&&part.body.length>8&&part.body.length<=4096&&Number.isInteger(part.raw)&&part.raw>0&&part.raw<=4096,'part bounds');
    const data=part.body.subarray(0,-4);require(data.length>=5&&data.at(-4)===0&&data.at(-3)===0&&data.at(-2)===255&&data.at(-1)===255,'full-flush alignment');
    size+=data.length;raw+=part.raw;combined=combineAdler(combined,footer(part.body),part.raw);
  }
  require(size<=262155&&raw<=262144,'batch bytes');const stream=new Uint8Array(size);stream.set([120,1]);let at=2;
  for(const part of parts){const data=part.body.subarray(0,-4);stream.set(data,at);at+=data.length;}
  stream.set([1,0,0,255,255],at);at+=5;new DataView(stream.buffer).setUint32(at,combined,false);
  const output=await boundedStream(stream,raw),tiles=[];at=0;
  for(const part of parts){const tile=output.subarray(at,at+part.raw);require(adler32(tile)===footer(part.body),'part checksum');tiles.push(tile);at+=part.raw;}
  return tiles;
}
