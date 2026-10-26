// Independent bounded moving RFB peer, used only by the explicit browser case.
import {createServer} from 'node:net';
import {once} from 'node:events';
const width=512,height=288;
const u16=value=>{const b=Buffer.alloc(2);b.writeUInt16BE(value);return b;};
const i32=value=>{const b=Buffer.alloc(4);b.writeInt32BE(value);return b;};
class Reader{
  constructor(socket){this.buffer=Buffer.alloc(0);this.waiters=[];this.closed=false;
    socket.on('data',bytes=>{if(this.buffer.length+bytes.length>1048576){socket.destroy();return;}this.buffer=Buffer.concat([this.buffer,bytes]);this.flush();});
    socket.on('close',()=>{this.closed=true;this.flush();});}
  flush(){while(this.waiters.length){const item=this.waiters[0];if(this.buffer.length<item.length){if(this.closed){this.waiters.shift().reject(new Error('Owned moving peer closed'));continue;}break;}
    this.waiters.shift();const bytes=this.buffer.subarray(0,item.length);this.buffer=this.buffer.subarray(item.length);item.resolve(bytes);}}
  read(length){if(length>1048576||this.waiters.length>=2)return Promise.reject(new Error('Owned peer read bound exceeded'));
    return new Promise((resolve,reject)=>{this.waiters.push({length,resolve,reject});this.flush();});}
}
export async function startMotionRfb(){
  const sockets=new Set(),timers=new Set(),inputs=[];let connections=0;
  const server=createServer(socket=>{
    connections++;sockets.add(socket);socket.on('error',()=>{});socket.on('close',()=>sockets.delete(socket));
    (async()=>{
      let timer;const reader=new Reader(socket);socket.setNoDelay(true);
      try{
        socket.write('RFB 003.008\n');await reader.read(12);
        socket.write(Buffer.from([1,1]));await reader.read(1);socket.write(i32(0));await reader.read(1);
        const format=Buffer.from([32,24,0,1,0,255,0,255,0,255,0,8,16,0,0,0]);
        socket.write(Buffer.concat([u16(width),u16(height),format,i32(13),Buffer.from('moving-owned!')]));
        let tick=0,pending=false,dirty=true,color=[12,34,56,0];
        const update=()=>{
          if(!pending||!dirty||socket.destroyed)return;
          if(socket.writableLength>2*1024*1024){socket.destroy();return;}
          const pixels=Buffer.alloc(width*height*4).fill(Buffer.from(color));
          for(let y=64;y<192;y++)for(let x=64;x<256;x++){
            const offset=(y*width+x)*4;
            pixels[offset]=(x*17+y*3+tick*9)&255;pixels[offset+1]=(x*5+y*19+tick*7)&255;
            pixels[offset+2]=(x*7+y*11+tick*13)&255;pixels[offset+3]=0;
          }
          socket.write(Buffer.concat([Buffer.from([0,0]),u16(1),u16(0),u16(0),u16(width),u16(height),i32(0),pixels]));
          pending=false;dirty=false;
        };
        timer=setInterval(()=>{tick++;dirty=true;update();},67);timers.add(timer);
        while(!socket.destroyed){
          const kind=(await reader.read(1))[0];
          if(kind===0)await reader.read(19);
          else if(kind===2){const data=await reader.read(3);await reader.read(data.readUInt16BE(1)*4);}
          else if(kind===3){await reader.read(9);pending=true;update();}
          else if(kind===4){const data=await reader.read(7),keysym=data.readUInt32BE(3),down=Boolean(data[0]);
            inputs.push({type:'key',keysym,down});if(inputs.length>1024){socket.destroy();break;}
            if(down){color=[31,67,129,0];dirty=true;update();}}
          else if(kind===5){const data=await reader.read(5);inputs.push({type:'pointer',buttons:data[0],x:data.readUInt16BE(1),y:data.readUInt16BE(3)});if(inputs.length>1024){socket.destroy();break;}}
          else throw new Error('Owned moving peer received unsupported command');
        }
      }finally{if(timer){clearInterval(timer);timers.delete(timer);}socket.destroy();}
    })().catch(()=>socket.destroy());
  });
  server.listen(0,'127.0.0.1');await once(server,'listening');
  return{address:`127.0.0.1:${server.address().port}`,inputs,get connections(){return connections;},
    stop:async()=>{for(const timer of timers)clearInterval(timer);for(const socket of sockets)socket.destroy();await new Promise(resolve=>server.close(resolve));}};
}
