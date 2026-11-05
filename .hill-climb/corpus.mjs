import {deflateSync} from 'node:zlib';

export const kinds=['raw','strips','palette','palette-zlib','global-palette','solid','xor','global-xor','copy','sparse','patch','empty'];

function rgba(x,y,phase) {
  return [(x*17+y*13+phase*31)&255,(x*5+y*29+phase*47)&255,(x*7+y*11+phase*59)&255,255];
}
function color(index,phase) {
  return [(index*47+phase*17)&255,(index*83+phase*29)&255,(index*131+phase*43)&255,255];
}
function tile(x,y,w,h,codec,body) {
  const bytes=Buffer.alloc(16+body.length);
  for(const [offset,value] of [[0,x],[2,y],[4,w],[6,h]])bytes.writeUInt16LE(value,offset);
  bytes[8]=codec;bytes.writeUInt32LE(body.length,12);bytes.set(body,16);
  return bytes;
}
function packet(width,height,tiles,kind,keyframe=false) {
  const global=kind.startsWith('global')||kind==='solid'||kind==='patch';
  const body=Buffer.concat(tiles),payload=global?deflateSync(body):body,header=Buffer.alloc(24);
  header.set(global?[71,76,66,kind==='patch'?51:50]:[71,76,83,49]);
  header.writeUInt16LE(width,12);header.writeUInt16LE(height,14);
  header.writeUInt16LE(tiles.length,16);header.writeUInt16LE(keyframe?1:0,18);
  if(global)header.writeUInt32LE(body.length,20);
  return Buffer.concat([header,payload]).toString('base64');
}
export function makeCase(width,height,kind) {
  const tiles=(phase,keyframe=false)=>{
    if(!keyframe&&kind==='empty')return [];
    const result=[];
    const step=kind==='strips'&&!keyframe?width:64;
    for(let y=0;y<height;y+=64)for(let x=0;x<width;x+=step) {
      const w=Math.min(step,width-x),h=Math.min(64,height-y);
      if(!keyframe&&['sparse','patch'].includes(kind)&&(x!==64||y!==64))continue;
      let codec=0,body;
      if(!keyframe&&kind==='copy') {
        codec=5;body=Buffer.alloc(4);
      } else if(!keyframe&&kind==='solid') {
        codec=6;body=Buffer.from(color((x/64+y/64)&15,phase));
      } else if(!keyframe&&kind==='patch') {
        codec=8;body=Buffer.alloc(2+100*6);body.writeUInt16LE(100,0);
        for(let i=0;i<100;i++) {
          body.writeUInt16LE(i*37,2+i*6);body.set(color(i&15,phase),4+i*6);
        }
      } else if(!keyframe&&kind.includes('palette')) {
        codec=kind==='palette-zlib'?3:2;body=Buffer.alloc(66+w*h);body.writeUInt16LE(16,0);
        for(let i=0;i<16;i++)body.set(color(i,phase),2+i*4);
        for(let j=0;j<h;j++)for(let i=0;i<w;i++)body[66+j*w+i]=((x+i)+(y+j)*3)&15;
        if(codec===3)body=deflateSync(body);
      } else {
        body=Buffer.alloc(w*h*4);
        const xor=!keyframe&&['xor','global-xor'].includes(kind);
        for(let j=0;j<h;j++)for(let i=0;i<w;i++) {
          const pixel=rgba(x+i,y+j,xor?1:phase);
          if(xor){const previous=rgba(x+i,y+j,0);for(let c=0;c<4;c++)pixel[c]^=previous[c];}
          body.set(pixel,(j*w+i)*4);
        }
        if(xor){codec=kind==='xor'?4:7;if(codec===4)body=deflateSync(body);}
      }
      result.push(tile(x,y,w,h,codec,body));
    }
    return result;
  };
  return {width,height,kind,key:packet(width,height,tiles(0,true),'raw',true),
    deltas:[1,2].map(phase=>packet(width,height,tiles(phase),kind))};
}

// Independent scalar description of the scene, evaluated outside timing.
export function verifyPixels(frame,{width,height,kind},updates) {
  const phase=(updates-1)%2+1;
  if(frame.width!==width||frame.height!==height||frame.pixels.length!==width*height*4)throw new Error('Frame shape differs');
  const damage=kind==='empty'?null:['sparse','patch'].includes(kind)?{x:64,y:64,width:64,height:64}:{x:0,y:0,width,height};
  if(JSON.stringify(frame.damage)!==JSON.stringify(damage))throw new Error('Frame damage differs');
  for(let y=0;y<height;y++)for(let x=0;x<width;x++) {
    let expected;
    if(kind==='empty')expected=rgba(x,y,0);
    else if(kind==='copy')expected=rgba(x%64,y%64,0);
    else if(kind==='sparse')expected=rgba(x,y,x>=64&&x<128&&y>=64&&y<128?phase:0);
    else if(kind==='patch') {
      const index=(y-64)*64+x-64;
      expected=x>=64&&x<128&&y>=64&&y<128&&index%37===0&&index/37<100?color(index/37&15,phase):rgba(x,y,0);
    } else if(kind==='solid')expected=color((Math.floor(x/64)+Math.floor(y/64))&15,phase);
    else if(kind.includes('palette'))expected=color((x+y*3)&15,phase);
    else expected=rgba(x,y,['xor','global-xor'].includes(kind)?updates%2:phase);
    for(let c=0;c<4;c++)if(frame.pixels[(y*width+x)*4+c]!==expected[c])throw new Error(`Pixel differs: ${kind}/${width}/${x}/${y}/${c}`);
  }
}

export function verifierSource() {return `const rgba=${rgba};const color=${color};window.verifyPixels=${verifyPixels};`;}
