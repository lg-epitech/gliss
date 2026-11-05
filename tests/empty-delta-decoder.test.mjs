import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {deflateSync} from 'node:zlib';
import {Decoder} from '../web/decoder.js';

const fixtures=JSON.parse(await readFile(new URL('../test-support/rust/codec/fixtures.json',import.meta.url),'utf8')).filter(row=>row.mode==='raw');
const bytes=hex=>Uint8Array.from(Buffer.from(hex,'hex'));

for(const wire of ['GLS1','GLB1','GLB2','GLB3'])test(`${wire}: empty deltas preserve pixels, validation and later recovery`,async()=>{
  const empty=({sequence=1,keyframe=false,extra=false}={})=>{
    const body=wire==='GLS1'?new Uint8Array(extra?1:0):deflateSync(new Uint8Array(extra?1:0));
    const packet=new Uint8Array(24+body.length),view=new DataView(packet.buffer);
    packet.set(bytes(fixtures[1].packet_hex).subarray(0,24));
    packet.set([...wire].map(c=>c.charCodeAt(0)));
    view.setUint32(4,sequence,true);view.setUint32(8,42,true);view.setUint16(18,keyframe?1:0,true);
    if(wire!=='GLS1')view.setUint32(20,extra?1:0,true);
    packet.set(body,24);return packet;
  };
  const decoder=new Decoder(),key=await decoder.decode(bytes(fixtures[0].packet_hex).buffer);
  const saved=key.pixels.slice(),packet=empty(),frame=await decoder.decode(packet.buffer);
  assert.deepEqual(frame.pixels,new Uint8ClampedArray(bytes(fixtures[1].expected_pixels_hex)));
  assert.equal(frame.damage,null);assert.equal(frame.sequence,1);assert.equal(frame.inputId,42);
  assert.equal(frame.tiles,0);assert.equal(frame.bytes,packet.length);assert.equal(frame.keyframe,false);
  if(wire!=='GLS1')assert.equal(frame.batch,true);
  if(wire==='GLB2'||wire==='GLB3')assert.equal(frame.global,true);
  if(wire==='GLB3')assert.equal(frame.sparse,true);

  const malformed=[empty({sequence:3}),empty({sequence:2,keyframe:true}),empty({sequence:2,extra:true}),packet.slice(0,-1)];
  if(wire!=='GLS1'){const checksum=empty({sequence:2});checksum[checksum.length-1]^=1;malformed.push(checksum);}
  for(const bad of malformed){
    const reference=decoder.frame;
    await assert.rejects(decoder.decode(bad.buffer));
    assert.equal(decoder.frame,reference);assert.equal(decoder.sequence,1);
    assert.deepEqual(reference.pixels,saved);
  }
  const recovered=await decoder.decode(bytes(fixtures[2].packet_hex).buffer);
  assert.deepEqual(recovered.pixels,new Uint8ClampedArray(bytes(fixtures[2].expected_pixels_hex)));
  assert.deepEqual(frame.pixels,saved);assert.deepEqual(key.pixels,saved);
});
