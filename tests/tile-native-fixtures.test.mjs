import assert from 'node:assert/strict';
import test from 'node:test';
import {readFile,stat} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {gunzipSync} from 'node:zlib';
import {TileReceiver,parseTile} from '../web/tile-decoder.js';

// The retained golden is shared with the Rust exact packet/RGBA regression.
// An explicit override exercises separately generated native fixture JSON.
test('live receiver agrees with all12 actual legacy native RGBA fixtures',async()=>{
  let bytes;
  if(process.env.GLISS_TILE_NATIVE_FIXTURES){
    const path=process.env.GLISS_TILE_NATIVE_FIXTURES;assert.ok((await stat(path)).size<=2097152);
    bytes=await readFile(path);
  }else{
    const compressed=await readFile(new URL('../test-support/rust/tiles/fixtures.json.gz',import.meta.url));
    assert.equal(createHash('sha256').update(compressed).digest('hex'),'53ff72be77e621e15281d099d985da5271a0d07f2f6a80e99c9edb366303d9b9');
    bytes=gunzipSync(compressed,{maxOutputLength:2097152});
    assert.equal(bytes.length,1137198);
    assert.equal(createHash('sha256').update(bytes).digest('hex'),'68f46a1cca18b67dab63931ed48f6a293b8e378cdde37e0936a7927c2f318106');
  }
  const fixture=JSON.parse(bytes.toString('utf8'));assert.equal(fixture.schema_version,1);assert.equal(fixture.steps.length,12);
  let policy=null,receiver=null,frame=null,epoch=null;const transformed=[0,0,0];
  for(const row of fixture.steps){
    if(row.policy!==policy){receiver?.close();policy=row.policy;receiver=new TileReceiver({payloadStride:1168,policy,now:()=>0});frame=null;epoch=null;}
    assert.ok(row.packets.length<=128);
    for(const raw of row.packets){const bytes=Uint8Array.from(raw),h=parseTile(bytes,1168,policy);if(h.kind===2&&h.index===0)transformed[h.policy]++;receiver.receive(bytes);const patch=await receiver.decodeNext();if(!patch)continue;
      if(epoch!==patch.epoch){epoch=patch.epoch;frame={width:patch.frameWidth,height:patch.frameHeight,pixels:new Uint8Array(patch.frameWidth*patch.frameHeight*4)};}
      for(let y=0;y<patch.height;y++)frame.pixels.set(patch.pixels.subarray(y*patch.width*4,(y+1)*patch.width*4),((patch.y+y)*frame.width+patch.x)*4);
    }
    assert.equal(frame.width,row.width);assert.equal(frame.height,row.height);assert.equal(createHash('sha256').update(frame.pixels).digest('hex'),row.rgba_sha256);
  }
  receiver.close();assert.ok(transformed.every(n=>n>0),'actual native fixtures cover all inverse policies');
});
