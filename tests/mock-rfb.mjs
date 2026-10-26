// Independent RFB peer for end-to-end capture/input tests; never connects to a VM.
import { createServer } from 'node:net';
import { once } from 'node:events';
import { deflateSync, inflateSync } from 'node:zlib';

function word(value){const b=Buffer.alloc(2);b.writeUInt16BE(value);return b;}
function integer(value){const b=Buffer.alloc(4);b.writeInt32BE(value);return b;}
class Reader {
  constructor(socket){this.buffer=Buffer.alloc(0);this.waiters=[];this.closed=false;socket.on('data',data=>{this.buffer=Buffer.concat([this.buffer,data]);this.flush();});socket.on('close',()=>{this.closed=true;this.flush();});}
  flush(){while(this.waiters.length){const next=this.waiters[0];if(this.buffer.length<next.n){if(this.closed){this.waiters.shift().reject(new Error('RFB closed'));continue;}break;}this.waiters.shift();const data=this.buffer.subarray(0,next.n);this.buffer=this.buffer.subarray(next.n);next.resolve(data);}}
  read(n){return new Promise((resolve,reject)=>{this.waiters.push({n,resolve,reject});this.flush();});}
}

export async function startRfb({extendedClipboard=false}={}){
  const inputs=[];const sockets=new Set();const waiters=[];
  const extended=(flags,data=Buffer.alloc(0))=>Buffer.concat([Buffer.from([3,0,0,0]),integer(-(4+data.length)),integer(flags),data]);
  let guestText='';
  const record=input=>{inputs.push(input);for(const waiter of [...waiters])if(waiter.predicate(inputs)){clearTimeout(waiter.timer);waiters.splice(waiters.indexOf(waiter),1);waiter.resolve(inputs);}};
  const server=createServer(socket=>{
    sockets.add(socket);socket.on('close',()=>sockets.delete(socket));socket.on('error',()=>{});
    (async()=>{
      const reader=new Reader(socket);socket.setNoDelay(true);
      socket.write('RFB 003.008\n');await reader.read(12);
      socket.write(Buffer.from([1,1]));await reader.read(1);socket.write(integer(0));await reader.read(1);
      const format=Buffer.from([32,24,0,1,0,255,0,255,0,255,0,8,16,0,0,0]);
      socket.write(Buffer.concat([word(96),word(64),format,integer(7),Buffer.from('fixture')]));
      let color=[12,34,56,0],pending=false,dirty=true;
      const update=()=>{if(!pending||!dirty)return;const pixels=Buffer.alloc(96*64*4);for(let i=0;i<pixels.length;i+=4)pixels.set(color,i);socket.write(Buffer.concat([Buffer.from([0,0]),word(1),word(0),word(0),word(96),word(64),integer(0),pixels]));pending=false;dirty=false;};
      while(!socket.destroyed){
        const type=(await reader.read(1))[0];
        if(type===0)await reader.read(19);
        else if(type===2){const header=await reader.read(3);const encodings=await reader.read(header.readUInt16BE(1)*4);if(extendedClipboard){if(!encodings.includes(Buffer.from([0xc0,0xa1,0xe5,0xce])))throw new Error('Extended clipboard not advertised');socket.write(extended(0x1b000001,integer(0)));}}
        else if(type===3){await reader.read(9);pending=true;update();}
        else if(type===4){const data=await reader.read(7);const keysym=data.readUInt32BE(3);const down=Boolean(data[0]);record({type:'key',keysym,down});if(down){color=[31,67,129,0];dirty=true;update();}}
        else if(type===5){const data=await reader.read(5);record({type:'pointer',buttons:data[0],x:data.readUInt16BE(1),y:data.readUInt16BE(3)});}
        else if(type===6){
          const header=await reader.read(7);const length=header.readInt32BE(3);
          if(Math.abs(length)>2101248)throw new Error('Clipboard too large');
          const data=await reader.read(Math.abs(length));
          if(length>=0){record({type:'clipboard',text:data.toString('latin1')});continue;}
          const flags=data.readUInt32BE(0);
          if(flags&0x01000000)record({type:'clipboard-caps',flags});
          else if(flags===0x08000001)socket.write(extended(0x02000001));
          else if(flags===0x02000001){const text=Buffer.from(guestText+'\0');socket.write(extended(0x10000001,deflateSync(Buffer.concat([integer(text.length),text]))));}
          else if(flags===0x10000001){const payload=inflateSync(data.subarray(4));const size=payload.readUInt32BE(0);if(size!==payload.length-4)throw new Error('Bad text length');record({type:'clipboard',text:payload.subarray(4).toString('utf8')});}
        }
        else throw new Error(`Unexpected RFB command ${type}`);
      }
    })().catch(()=>socket.destroy());
  });
  server.listen(0,'127.0.0.1');await once(server,'listening');
  return{sendClipboard:text=>{guestText=text;for(const socket of sockets)socket.write(extended(0x08000001));},address:`127.0.0.1:${server.address().port}`,inputs,waitFor:(predicate,timeout=5000)=>{if(predicate(inputs))return Promise.resolve(inputs);return new Promise((resolve,reject)=>{const waiter={predicate,resolve,reject,timer:setTimeout(()=>{waiters.splice(waiters.indexOf(waiter),1);reject(new Error(`Input wait expired; received ${inputs.length} messages`));},timeout)};waiters.push(waiter);});},stop:async()=>{for(const socket of sockets)socket.destroy();for(const waiter of waiters){clearTimeout(waiter.timer);waiter.reject(new Error('Fixture stopped'));}await new Promise(resolve=>server.close(resolve));}};
}
