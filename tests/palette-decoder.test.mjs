import test from 'node:test';
import assert from 'node:assert/strict';
import {deflateSync} from 'node:zlib';
import {Decoder} from '../web/decoder.js';

function packet({colors, indices, codec=2, global=false, sequence=0, keyframe=true}) {
  const palette=Uint8Array.from(colors.flat()),body=new Uint8Array(2+palette.length+indices.length);
  new DataView(body.buffer).setUint16(0,colors.length,true);
  body.set(palette,2);body.set(indices,2+palette.length);
  const payload=codec===3?deflateSync(body):body;
  const tile=new Uint8Array(16+payload.length),header=new DataView(tile.buffer);
  header.setUint16(4,indices.length,true);header.setUint16(6,1,true);
  header.setUint8(8,codec);header.setUint32(12,payload.length,true);tile.set(payload,16);
  const encoded=global?deflateSync(tile):tile,bytes=new Uint8Array(24+encoded.length),view=new DataView(bytes.buffer);
  bytes.set(global?[71,76,66,50]:[71,76,83,49]);
  view.setUint32(4,sequence,true);view.setUint16(12,indices.length,true);view.setUint16(14,1,true);
  view.setUint16(16,1,true);view.setUint16(18,keyframe?1:0,true);
  if(global)view.setUint32(20,tile.length,true);
  bytes.set(encoded,24);return bytes.buffer;
}

const colors=[[17,83,249,0],[201,7,61,127],[44,199,131,255]];

for(const codec of [2,3]) {
  test(`palette codec ${codec}: exact RGBA, one-color and unused legacy entries`,async()=>{
    const largest=Array.from({length:256},(_,index)=>[index,255-index,index*3&255,index*7&255]);
    for(const [entries,indices] of [[colors,[2,0,1,2]],[colors,[0]],
      [colors.slice(0,1),[0,0]],[largest,[255,128,0,1]]]) {
      const frame=await new Decoder().decode(packet({colors:entries,indices,codec}));
      assert.deepEqual([...frame.pixels],indices.flatMap(index=>entries[index]));
    }
  });

  test(`palette codec ${codec}: invalid index leaves the temporal reference unchanged`,async()=>{
    const decoder=new Decoder();
    await decoder.decode(packet({colors,indices:[2,0,1,2],codec}));
    const reference=decoder.frame,saved=reference.pixels.slice();
    await assert.rejects(decoder.decode(packet({colors,indices:[0,1,2,3],codec,sequence:1,keyframe:false})),/Invalid palette index/);
    assert.equal(decoder.frame,reference);assert.equal(decoder.sequence,0);
    assert.deepEqual(reference.pixels,saved);
    const recovered=await decoder.decode(packet({colors,indices:[1,2,0,1],codec,sequence:1,keyframe:false}));
    assert.deepEqual([...recovered.pixels],[1,2,0,1].flatMap(index=>colors[index]));
  });
}

test('global palettes retain stricter color-count and used-entry rules',async()=>{
  const decoder=new Decoder();
  const valid={colors,indices:[2,0,1,2,0,1,2,1],global:true};
  const frame=await decoder.decode(packet(valid));
  assert.deepEqual([...frame.pixels],valid.indices.flatMap(index=>colors[index]));
  const reference=decoder.frame,saved=reference.pixels.slice();
  for(const malformed of [{colors:colors.slice(0,1),indices:Array(8).fill(0)},
    {colors,indices:Array(8).fill(0)},{colors:Array(9).fill(colors[0]),indices:Array(8).fill(0)}]) {
    await assert.rejects(decoder.decode(packet({...malformed,global:true,sequence:1,keyframe:false})));
    assert.equal(decoder.frame,reference);assert.equal(decoder.sequence,0);
    assert.deepEqual(reference.pixels,saved);
  }
});
