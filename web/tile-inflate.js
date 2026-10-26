// CPU GLT2 zlib inflate. Guarded fflate0.8.3 fork; see vendor/fflate-provenance.json.
import {glissRawInflate} from './vendor/fflate-bounded-core.js';
const require=(ok,message)=>{if(!ok)throw new Error(message);};
const BASE=65521;
export function adler32(bytes){let a=1,b=0;for(const value of bytes){a+=value;b+=a;}return (((b%BASE)<<16)|(a%BASE))>>>0;}
const footer=bytes=>new DataView(bytes.buffer,bytes.byteOffset+bytes.length-4,4).getUint32(0,false);
export function boundedFflate(body,limit){
  require(body instanceof Uint8Array&&body.length>=8&&body.length<=4096&&Number.isInteger(limit)&&limit>0&&limit<=4096,'inflate bounds');
  require((body[0]&15)===8&&(body[0]>>>4)<=7&&!(body[1]&32)&&((body[0]<<8)|body[1])%31===0,'zlib header');
  const raw=body.subarray(2,-4),out=new Uint8Array(limit),{value,state}=glissRawInflate(raw,out,1<<(8+(body[0]>>>4)));
  require(state.b===limit&&value.length===limit&&state.f===1&&!state.l&&Math.ceil(state.p/8)===raw.length,'exact deflate extent');
  require(adler32(value)===footer(body),'zlib checksum');return value;
}
