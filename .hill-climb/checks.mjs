import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {deflateSync,inflateSync} from 'node:zlib';
import {withDecoder} from './browser.mjs';

const fixtures=JSON.parse(await readFile('test-support/rust/codec/fixtures.json','utf8'));
// Derive metadata from the native wire independently of the candidate decoder.
for(const r of fixtures) {
  const packet=Buffer.from(r.packet_hex,'hex'),batch=packet[2]===66;
  const body=batch?inflateSync(packet.subarray(24)):packet.subarray(24),count=packet.readUInt16LE(16);
  let pos=0,left=r.width,top=r.height,right=0,bottom=0;
  for(let i=0;i<count;i++) {
    const x=body.readUInt16LE(pos),y=body.readUInt16LE(pos+2),w=body.readUInt16LE(pos+4),h=body.readUInt16LE(pos+6);
    left=Math.min(left,x);top=Math.min(top,y);right=Math.max(right,x+w);bottom=Math.max(bottom,y+h);
    pos+=16+body.readUInt32LE(pos+12);
  }
  assert.equal(pos,body.length);
  r.expectedMetadata={sequence:packet.readUInt32LE(4),inputId:packet.readUInt32LE(8),keyframe:!!(packet.readUInt16LE(18)&1),tiles:count,bytes:packet.length};
  r.expectedDamage=count?{x:left,y:top,width:right-left,height:bottom-top}:null;
}
const wrap=(body,version,count=1,length=body.length,flags=0)=>{
  const header=Buffer.alloc(24);header.set([71,76,66,version]);header.writeUInt32LE(1,4);
  header.writeUInt16LE(4,12);header.writeUInt16LE(4,14);header.writeUInt16LE(count,16);
  header.writeUInt16LE(flags,18);header.writeUInt32LE(length,20);
  return Buffer.concat([header,deflateSync(body)]).toString('hex');
};
const batchTile=(codec,body)=>{
  const tile=Buffer.alloc(16+body.length);tile.writeUInt16LE(4,4);tile.writeUInt16LE(4,6);
  tile[8]=codec;tile.writeUInt32LE(body.length,12);tile.set(body,16);return tile;
};
const raw=batchTile(0,Buffer.alloc(64,127)),patch=Buffer.alloc(14);
patch.writeUInt16LE(2,0);patch.writeUInt16LE(0,2);patch.set([17,29,43,255],4);
patch.writeUInt16LE(16,8);patch.set([71,83,97,255],10);
const duplicate=Buffer.from(patch);duplicate.writeUInt16LE(0,8);
const compressedMalformed=[wrap(raw,49,1,raw.length+1),wrap(raw,50,1,raw.length,2),
  wrap(batchTile(3,Buffer.alloc(64)),50),wrap(batchTile(8,patch),51),wrap(batchTile(8,duplicate),51),
  wrap(raw,50,2),wrap(Buffer.concat([raw,Buffer.from([0])]),50)];
const result=await withDecoder(async page=>page.evaluate(async fixtures=>{
  const ensure=(condition,message)=>{if(!condition)throw new Error(message);};
  const fromHex=hex=>Uint8Array.from(hex.match(/../g)||[],s=>parseInt(s,16));
  const [records,compressedMalformed]=fixtures,decoders=new Map(),surfaces=new Map(),previous=new Map();
  for(const r of records) {
    if(!decoders.has(r.mode))decoders.set(r.mode,new window.Decoder());
    const frame=await decoders.get(r.mode).decode(fromHex(r.packet_hex).buffer),expected=fromHex(r.expected_pixels_hex);
    ensure(frame.width===r.width&&frame.height===r.height&&frame.pixels.length===expected.length,'Native fixture shape differs');
    ensure(frame.pixels.every((v,i)=>v===expected[i]),`Native fixture pixels differ: ${r.mode}/${r.sequence}`);
    for(const [name,value] of Object.entries(r.expectedMetadata))ensure(frame[name]===value,`Native metadata differs: ${name}`);
    ensure(JSON.stringify(frame.damage)===JSON.stringify(r.expectedDamage),'Native damage differs');
    const saved=previous.get(r.mode);
    if(saved)ensure(saved.pixels.every((v,i)=>v===saved.copy[i]),'Decode mutated an earlier returned frame');
    previous.set(r.mode,{pixels:frame.pixels,copy:frame.pixels.slice()});
    if(!surfaces.has(r.mode))surfaces.set(r.mode,document.createElement('canvas'));
    const canvas=surfaces.get(r.mode);
    if(canvas.width!==frame.width||canvas.height!==frame.height){canvas.width=frame.width;canvas.height=frame.height;}
    const ctx=canvas.getContext('2d',{alpha:false,willReadFrequently:true}),area=frame.damage;
    if(area)ctx.putImageData(new ImageData(frame.pixels,frame.width,frame.height),0,0,area.x,area.y,area.width,area.height);
    ensure(ctx.getImageData(0,0,frame.width,frame.height).data.every((v,i)=>v===expected[i]),'Damage painting differs from native pixels');
  }
  const packet=(tiles,{keyframe=false,sequence=1,width=4,height=4}={})=>{
    const bytes=new Uint8Array(24+tiles.reduce((n,t)=>n+16+t.body.length,0)),v=new DataView(bytes.buffer);
    bytes.set([71,76,83,49]);v.setUint32(4,sequence,true);v.setUint16(12,width,true);v.setUint16(14,height,true);
    v.setUint16(16,tiles.length,true);v.setUint16(18,keyframe?1:0,true);
    let pos=24;
    for(const t of tiles) {
      for(const [offset,value] of [[0,t.x],[2,t.y],[4,t.w],[6,t.h]])v.setUint16(pos+offset,value,true);
      bytes[pos+8]=t.codec||0;v.setUint32(pos+12,t.body.length,true);bytes.set(t.body,pos+16);pos+=16+t.body.length;
    }
    return bytes;
  };
  const full={x:0,y:0,w:4,h:4,body:Uint8Array.from({length:64},(_,i)=>i*37&255)};
  const small={x:1,y:1,w:1,h:1,body:new Uint8Array([97,83,71,255])};
  const key=packet([full],{keyframe:true,sequence:0}),decoder=new window.Decoder();
  await decoder.decode(key.buffer);
  const malformed=[packet([small,small]),packet([full,full,full]),packet([small],{keyframe:true}),packet([],{keyframe:true}),
    packet([{...full,codec:5,body:new Uint8Array(4)}],{keyframe:true}),packet([],{width:5}),packet([],{sequence:2}),
    key.slice(0,-1),new Uint8Array([...key,0]),packet([],{keyframe:true,width:65535,height:65535})];
  const reserved=key.slice();reserved[18]=2;malformed.push(reserved);
  malformed.push(...compressedMalformed.map(fromHex));
  for(const bytes of malformed) {
    const reference=decoder.frame,saved=reference.pixels.slice(),sequence=decoder.sequence;
    let rejected=false;try{await decoder.decode(bytes.buffer);}catch{rejected=true;}
    ensure(rejected,'Malformed frame accepted');
    ensure(decoder.frame===reference&&decoder.sequence===sequence,'Failed decode changed reference identity');
    ensure(reference.pixels.every((v,i)=>v===saved[i]),'Failed decode mutated pixels');
  }
  const saved=decoder.frame.pixels.slice();await decoder.decode(packet([]).buffer);
  ensure(decoder.sequence===1&&decoder.frame.pixels.every((v,i)=>v===saved[i]),'Empty delta changed pixels');
  await decoder.decode(packet([small],{sequence:2}).buffer);
  ensure(decoder.frame.pixels.slice(20,24).every((v,i)=>v===small.body[i]),'Valid recovery failed');
  await decoder.decode(packet([full],{keyframe:true,sequence:0xffffffff}).buffer);
  await decoder.decode(packet([],{sequence:0}).buffer);ensure(decoder.sequence===0,'Sequence wrap failed');
  return {nativeFixtures:records.length,malformedAtomic:malformed.length,emptyRecoveryAndWrap:true};
},[fixtures,compressedMalformed]),process.argv[2]);
assert.equal(result.nativeFixtures,73);assert.equal(result.malformedAtomic,18);
console.log(JSON.stringify(result));
