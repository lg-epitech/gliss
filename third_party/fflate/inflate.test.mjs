import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {deflateSync,inflateSync,createDeflateRaw,constants} from 'node:zlib';
import {adler32,combineAdler,boundedFflate,inflateParts} from './inflate.mjs';
import {glissRawInflate} from './fflate-bounded-core.mjs';
const part=async raw=>{const stream=createDeflateRaw({level:1}),chunks=[];stream.on('data',c=>chunks.push(c));await new Promise((resolve,reject)=>{stream.once('error',reject);stream.write(raw);stream.flush(constants.Z_FULL_FLUSH,resolve);});const body=Buffer.concat([...chunks,Buffer.alloc(4)]);body.writeUInt32BE(adler32(raw),body.length-4);stream.destroy();return{body:new Uint8Array(body),raw:raw.length};};
test('bounded guarded fflate checks exact output, end extent, checksum and stored complement',()=>{
  for(const level of[0,1,6,9]){const raw=Uint8Array.from({length:level===0?2048:4096},(_,i)=>(i*i+17*i)%251),body=new Uint8Array(deflateSync(raw,{level}));assert.deepEqual(boundedFflate(body,raw.length),raw);assert.throws(()=>boundedFflate(body,raw.length-1));const bad=body.slice();bad[bad.length-1]^=1;assert.throws(()=>boundedFflate(bad,raw.length));assert.throws(()=>boundedFflate(body.subarray(0,-1),raw.length));const trailing=new Uint8Array(body.length+1);trailing.set(body.subarray(0,-4));trailing.set(body.subarray(-4),body.length-3);assert.throws(()=>boundedFflate(trailing,raw.length));}
  const stored=new Uint8Array(deflateSync(new Uint8Array(32),{level:0}));stored[5]^=1;assert.throws(()=>boundedFflate(stored,32));
});
test('Adler composition matches literal concatenation including empty strings',()=>{const a=new Uint8Array([1,2,255]),b=new Uint8Array(4096).fill(251);assert.equal(combineAdler(adler32(a),adler32(b),b.length),adler32(new Uint8Array([...a,...b])));assert.equal(combineAdler(1,adler32(b),b.length),adler32(b));});
test('Adler component zero is valid and composes without negative modulo',()=>{const raw=new Uint8Array(257).fill(255);raw[256]=240;assert.equal(adler32(raw)&65535,0);assert.equal(combineAdler(adler32(raw),adler32(raw),raw.length),adler32(new Uint8Array([...raw,...raw])));});
test('reserved fixed-Huffman distance and length symbols reject like native zlib',()=>{
  const distance3=new Uint8Array([120,1,3,126,0,0,3,0,1]),distance4=new Uint8Array([120,1,3,126,12,0,0,4,0,1]);
  const bits=[1,1,0],code=(value,count)=>{for(let i=count-1;i>=0;i--)bits.push((value>>>i)&1);};
  code(48,8);code(198,8);code(0,5);code(0,7);const body=new Uint8Array(2+Math.ceil(bits.length/8)+4);body.set([120,1]);bits.forEach((bit,i)=>body[2+(i>>>3)]|=bit<<(i&7));new DataView(body.buffer).setUint32(body.length-4,adler32(new Uint8Array(260)),false);
  for(const[wire,limit]of[[distance3,3],[distance4,4],[body,260]]){assert.throws(()=>inflateSync(wire));assert.throws(()=>boundedFflate(wire,limit));}
});
test('64 independent full-flush parts decode alone, reversed and together',async()=>{
  const raw=Array.from({length:64},(_,n)=>Uint8Array.from({length:4+64*n},(_,i)=>(i*17+n*9)%251)),parts=[];for(const bytes of raw)parts.push(await part(bytes));
  const batch=await inflateParts(parts);for(let n=0;n<64;n++){assert.deepEqual(batch[n],raw[n]);assert.deepEqual((await inflateParts([parts[n]]))[0],raw[n]);}
  assert.deepEqual(await inflateParts(parts.slice().reverse()),raw.slice().reverse());assert.deepEqual(await inflateParts(parts.filter((_,i)=>i%3)),raw.filter((_,i)=>i%3));
});
test('malformed part/checksum/aggregate bounds reject before returning any tiles',async()=>{const good=await part(new Uint8Array(4096).fill(17)),bad={...good,body:good.body.slice()};bad.body[bad.body.length-1]^=1;await assert.rejects(inflateParts([good,bad]));await assert.rejects(inflateParts(Array(65).fill(good)));await assert.rejects(inflateParts([{...good,raw:4097}]));await assert.rejects(inflateParts([{...good,body:good.body.subarray(0,-1)}]));});

// Fixtures deliberately encode exact tree grammars instead of hoping a native
// compressor happens to select a degenerate or malformed alphabet.
class BitFixture{
  constructor(){this.bits=[];}
  lsb(value,length){for(let i=0;i<length;i++)this.bits.push((value>>>i)&1);}
  huffman(value,length){for(let i=length-1;i>=0;i--)this.bits.push((value>>>i)&1);}
  bytes(){const out=new Uint8Array(Math.ceil(this.bits.length/8));this.bits.forEach((bit,index)=>out[index>>>3]|=bit<<(index&7));return out;}
}
const ORDER=[16,17,18,0,8,7,9,6,10,5,11,4,12,3,13,2,14,1,15];
function canonical(lengths){const counts=Array(16).fill(0),next=Array(16).fill(0);for(const length of lengths)if(length)counts[length]++;let code=0;for(let width=1;width<16;width++){code=(code+counts[width-1])*2;next[width]=code;}return lengths.map(length=>length?{code:next[length]++,length}:null);}
const emit=(bits,codes,symbol)=>{const code=codes[symbol];assert.ok(code,`fixture missing symbol ${symbol}`);bits.huffman(code.code,code.length);};
function zlibFixture(deflate,raw,cinfo=7){const out=new Uint8Array(deflate.length+6),cmf=(cinfo<<4)|8;out[0]=cmf;out[1]=(31-((cmf<<8)%31))%31;out.set(deflate,2);new DataView(out.buffer).setUint32(out.length-4,adler32(raw),false);return out;}
function dynamicFixture({literal,distance=[0],codeLengths=Array.from({length:19},(_,i)=>i<16?4:0),data=()=>{},raw=new Uint8Array(),writeLengths=true}){
  const bits=new BitFixture();bits.lsb(1,1);bits.lsb(2,2);bits.lsb(literal.length-257,5);bits.lsb(distance.length-1,5);bits.lsb(15,4);for(const symbol of ORDER)bits.lsb(codeLengths[symbol]??0,3);
  if(writeLengths){const codes=canonical(codeLengths);for(const length of [...literal,...distance])emit(bits,codes,length);}
  data({bits,literal:canonical(literal),distance:canonical(distance)});return zlibFixture(bits.bytes(),raw);
}
function literalTree(entries,length=257){const out=Array(length).fill(0);for(const[symbol,bits]of entries)out[symbol]=bits;return out;}
function acceptsNative(wire,expected){assert.deepEqual(new Uint8Array(inflateSync(wire)),expected);if(expected.length)assert.deepEqual(boundedFflate(wire,expected.length),expected);else{const raw=wire.subarray(2,-4),{value,state}=glissRawInflate(raw,new Uint8Array(0));assert.equal(value.length,0);assert.equal(state.f,1);assert.equal(Boolean(state.l),false);assert.equal(Math.ceil(state.p/8),raw.length);}}
function rejectsNative(wire,limit=1){assert.throws(()=>inflateSync(wire));assert.throws(()=>boundedFflate(wire,limit));}

test('native valid literal-only trees and one-bit EOB/distance exceptions remain accepted',()=>{
  const a=new Uint8Array([65]),literals=literalTree([[65,1],[256,1]]);
  for(const distance of[[0],[0,0],[1]])acceptsNative(dynamicFixture({literal:literals,distance,raw:a,data:({bits,literal})=>{emit(bits,literal,65);emit(bits,literal,256);}}),a);
  const empty=dynamicFixture({literal:literalTree([[256,1]]),data:({bits,literal})=>emit(bits,literal,256)});acceptsNative(empty,new Uint8Array());
  const match=dynamicFixture({literal:literalTree([[65,1],[256,2],[257,2]],258),distance:[1],raw:new Uint8Array(4).fill(65),data:({bits,literal,distance})=>{emit(bits,literal,65);emit(bits,literal,257);emit(bits,distance,0);emit(bits,literal,256);}});acceptsNative(match,new Uint8Array(4).fill(65));
});
test('dynamic HLIT and HDIST counts reject unused reserved slots like native zlib',()=>{
  for(const[count,distance]of[[287,[0]],[288,[0]],[257,Array(31).fill(0)],[257,Array(32).fill(0)]]){
    const wire=dynamicFixture({literal:literalTree([[65,1],[256,1]],count),distance,raw:new Uint8Array([65]),data:({bits,literal})=>{emit(bits,literal,65);emit(bits,literal,256);}});rejectsNative(wire);
  }
});
test('code-length alphabet rejects incomplete, oversubscribed and empty trees',()=>{
  const literal=literalTree([[65,1],[256,1]]),raw=new Uint8Array([65]);
  for(const codeLengths of[Object.assign(Array(19).fill(0),{0:2,1:2}),Object.assign(Array(19).fill(0),{0:1,1:1,2:1})])rejectsNative(dynamicFixture({literal,codeLengths,raw,data:({bits,literal})=>{emit(bits,literal,65);emit(bits,literal,256);}}));
  rejectsNative(dynamicFixture({literal,codeLengths:Array(19).fill(0),raw,writeLengths:false}));
});
test('literal alphabet needs EOB and rejects over/undersubscription beyond one bit',()=>{
  rejectsNative(dynamicFixture({literal:literalTree([[65,2],[256,2]]),raw:new Uint8Array([65]),data:({bits,literal})=>{emit(bits,literal,65);emit(bits,literal,256);}}));
  rejectsNative(dynamicFixture({literal:literalTree([[65,1],[66,1],[256,1]]),raw:new Uint8Array([66]),data:({bits,literal})=>{emit(bits,literal,66);emit(bits,literal,256);}}));
  rejectsNative(dynamicFixture({literal:literalTree([[65,1],[66,1]]),raw:new Uint8Array([65]),data:({bits,literal})=>emit(bits,literal,65)}));
  const singleton=dynamicFixture({literal:literalTree([[256,2]]),data:({bits,literal})=>emit(bits,literal,256)});assert.throws(()=>inflateSync(singleton));assert.throws(()=>glissRawInflate(singleton.subarray(2,-4),new Uint8Array(0)));
});
test('distance alphabet rejects over/undersubscription and use of an empty tree',()=>{
  const literal=literalTree([[65,1],[256,2],[257,2]],258);
  rejectsNative(dynamicFixture({literal,distance:[2],raw:new Uint8Array(4).fill(65),data:({bits,literal,distance})=>{emit(bits,literal,65);emit(bits,literal,257);emit(bits,distance,0);emit(bits,literal,256);}}),4);
  rejectsNative(dynamicFixture({literal,distance:[1,1,1],raw:new Uint8Array(6).fill(65),data:({bits,literal,distance})=>{for(let i=0;i<3;i++)emit(bits,literal,65);emit(bits,literal,257);emit(bits,distance,1);emit(bits,literal,256);}}),6);
  rejectsNative(dynamicFixture({literal,distance:[0],raw:new Uint8Array(4).fill(65),data:({bits,literal})=>{emit(bits,literal,65);emit(bits,literal,257);bits.lsb(0,1);emit(bits,literal,256);}}),4);
});
test('code-length repeats reject missing predecessors and alphabet overflow',()=>{
  const literal=literalTree([[65,1],[256,1]]),first=Object.assign(Array(19).fill(0),{0:1,16:1});
  rejectsNative(dynamicFixture({literal,codeLengths:first,writeLengths:false,raw:new Uint8Array([65]),data:({bits})=>{emit(bits,canonical(first),16);bits.lsb(0,2);}}));
  const overflow=Object.assign(Array(19).fill(0),{0:1,1:2,18:2});
  rejectsNative(dynamicFixture({literal,codeLengths:overflow,writeLengths:false,raw:new Uint8Array([65]),data:({bits})=>{for(let i=0;i<2;i++){emit(bits,canonical(overflow),18);bits.lsb(127,7);}}}));
});
test('fixed matches reject unavailable history and preserve maximum-length overlap',()=>{
  const absent=new BitFixture();absent.lsb(1,1);absent.lsb(1,2);absent.huffman(1,7);absent.huffman(0,5);absent.huffman(0,7);rejectsNative(zlibFixture(absent.bytes(),new Uint8Array(3)),3);
  const overlap=new BitFixture();overlap.lsb(1,1);overlap.lsb(1,2);overlap.huffman(48+65,8);overlap.huffman(197,8);overlap.huffman(0,5);overlap.huffman(0,7);const expected=new Uint8Array(259).fill(65);acceptsNative(zlibFixture(overlap.bytes(),expected),expected);
});
test('native strategies and short streams keep exact fixed-buffer output',()=>{
  for(const length of[1,2,3,32,257,2048,4096])for(const strategy of[constants.Z_DEFAULT_STRATEGY,constants.Z_FIXED,constants.Z_HUFFMAN_ONLY,constants.Z_RLE]){
    const raw=Uint8Array.from({length},(_,i)=>i%7===0?65:66),wire=new Uint8Array(deflateSync(raw,{level:1,strategy}));assert.ok(wire.length<=4096);acceptsNative(wire,raw);assert.throws(()=>boundedFflate(wire,length+1));if(length>1)assert.throws(()=>boundedFflate(wire,length-1));
  }
});

function fixedHistory(distance,cinfo=7){
  const prefix=Uint8Array.from({length:distance},(_,i)=>i%251),bits=new BitFixture();
  bits.lsb(0,1);bits.lsb(0,2);while(bits.bits.length%8)bits.lsb(0,1);bits.lsb(prefix.length,16);bits.lsb(prefix.length^65535,16);for(const value of prefix)bits.lsb(value,8);
  bits.lsb(1,1);bits.lsb(1,2);bits.huffman(1,7); // fixed symbol257: length3
  const symbol=distance===256?15:16,base=symbol===15?193:257,extra=symbol===15?6:7;bits.huffman(symbol,5);bits.lsb(distance-base,extra);bits.huffman(0,7);
  const raw=new Uint8Array(prefix.length+3);raw.set(prefix);raw.set(prefix.subarray(0,3),prefix.length);return{wire:zlibFixture(bits.bytes(),raw,cinfo),raw};
}
test('declared zlib windows preserve valid small history and bound longer references',t=>{
  for(const[distance,cinfo]of[[256,0],[257,1],[257,7]]){const{wire,raw}=fixedHistory(distance,cinfo);acceptsNative(wire,raw);}
  const{wire,raw}=fixedHistory(257,0);let nativeAccepted=false;try{assert.deepEqual(new Uint8Array(inflateSync(wire)),raw);nativeAccepted=true;}catch(error){assert.match(String(error),/distance|window|data/i);}
  t.diagnostic(`Node zlib ${process.versions.zlib} accepted distance257 under CINFO0: ${nativeAccepted}; strict advertised-window enforcement is build-dependent in zlib.`);
  assert.throws(()=>boundedFflate(wire,raw.length));
  const good=fixedHistory(256,0);for(const maximum of[undefined,null,NaN,0,255,257,65536])assert.throws(()=>glissRawInflate(good.wire.subarray(2,-4),new Uint8Array(good.raw.length),maximum));
});
test('exact guarded fork can be reconstructed from pinned upstream and declared patches',async()=>{
  const base=new URL('./',import.meta.url),manifest=JSON.parse(await readFile(new URL('fflate-patches.json',base),'utf8')),upstream=await readFile(new URL(manifest.upstream,base),'utf8');
  const digest=value=>createHash('sha256').update(value).digest('hex');assert.equal(digest(upstream),manifest.upstream_sha256);let reconstructed=upstream;
  for(const patch of manifest.patches){assert.equal(reconstructed.split(patch.before).length-1,1,'each exact source patch applies once');reconstructed=reconstructed.replace(patch.before,patch.after);}
  reconstructed+=manifest.append;const actual=await readFile(new URL(manifest.fork,base),'utf8');assert.equal(reconstructed,actual);assert.equal(digest(actual),manifest.fork_sha256);
});
