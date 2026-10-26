import test from 'node:test';
import assert from 'node:assert/strict';
import {decodeProbe} from '../web/probe.js';
import {decodeProbe as decodeV2} from '../test-support/browser/probe-v2.js';
function pixels(id,distortion=0){const checksum=(id&255)^((id>>>8)&255)^((id>>>16)&255)^0xa5;const bits=(id&0xffffff)|(checksum<<24);return(x,y)=>{const bit=Math.floor((x-32)/8)+8*Math.floor(y/8);const value=(bits&(1<<bit))?235-distortion:20+distortion;return[value,value+1,value-1,255];};}
test('marker tolerates contrast-preserving codec error but rejects corrupt or uncertain IDs',()=>{
  for(const id of [0,1,255,256,65535,0xffffff])assert.equal(decodeProbe(pixels(id,30)),id);
  const source=pixels(4711);assert.equal(decodeProbe((x,y)=>x===36&&y===4?[20,20,20,255]:source(x,y)),null);
  assert.equal(decodeProbe(()=>[127,127,127,255]),null);
  assert.equal(decodeProbe(()=>[20,200,20,255]),null);
});
test('recorded native H264 tint carries the correct ID despite the v2 grayscale rejection',()=>{
  const cells=[[15,22,31],[239,231,227],...Array.from({length:6},()=>[20,20,20]),[15,22,31],[28,22,19],...Array.from({length:6},()=>[20,20,20]),[19,19,19],[19,19,19],...Array.from({length:6},()=>[20,20,20]),[220,236,249],[234,234,234],[234,234,234],[20,20,20],[20,20,20],[234,234,234],[20,20,20],[234,234,234]];
  const read=(x,y)=>cells[Math.floor((x-32)/8)+8*Math.floor(y/8)];
  assert.equal(decodeV2(read),null);
  assert.equal(decodeProbe(read),2);
  const corrupted=cells.map(p=>[...p]);corrupted[1]=[20,20,20];
  assert.equal(decodeProbe((x,y)=>corrupted[Math.floor((x-32)/8)+8*Math.floor(y/8)]),null);
});
