// Deterministic TCP impairment MODEL. Byte loss would corrupt TCP; this models
// retransmission stalls instead. Real packet loss requires the isolated netem rig.
import { createServer, createConnection } from 'node:net';
import { once } from 'node:events';
import { performance } from 'node:perf_hooks';

export const PROFILES=Object.freeze({
  loopback:{rttMs:0,jitterMs:0,mbps:0,lossStallProbability:0},
  wan:{rttMs:40,jitterMs:5,mbps:20,lossStallProbability:0},
  constrained:{rttMs:80,jitterMs:15,mbps:4,lossStallProbability:0.01,retransmitMs:200},
});

export async function startProxy({targetPort,profile=PROFILES.loopback,port=0,seed=0x6a155eed}){
  let state=seed>>>0;
  const random=()=>{state^=state<<13;state^=state>>>17;state^=state<<5;return(state>>>0)/4294967296;};
  const sockets=new Set();const stats={upBytes:0,downBytes:0,modeledStalls:0,queueOverflows:0};
  const server=createServer(client=>{
    const upstream=createConnection({host:'127.0.0.1',port:targetPort});
    for(const socket of [client,upstream]){sockets.add(socket);socket.setNoDelay(true);socket.on('close',()=>sockets.delete(socket));socket.on('error',()=>{client.destroy();upstream.destroy();});}
    client.on('end',()=>upstream.end());upstream.on('end',()=>client.end());
    const forward=(from,to,direction)=>{
      let nextDelivery=performance.now();let queueBytes=0;let chain=Promise.resolve();
      from.on('data',chunk=>{
        stats[direction]+=chunk.length;queueBytes+=chunk.length;
        if(queueBytes>8*1024*1024){stats.queueOverflows++;client.destroy();upstream.destroy();return;}
        const now=performance.now();
        const serialization=profile.mbps?chunk.length*8/(profile.mbps*1000):0;
        const delay=Math.max(0,profile.rttMs/2+(random()*2-1)*profile.jitterMs);
        const stall=random()<profile.lossStallProbability?(profile.retransmitMs||200):0;
        if(stall)stats.modeledStalls++;
        nextDelivery=Math.max(now+delay+stall,nextDelivery)+serialization;
        const due=nextDelivery;
        chain=chain.then(async()=>{
          await new Promise(resolve=>setTimeout(resolve,Math.max(0,due-performance.now())));
          queueBytes-=chunk.length;if(to.destroyed)return;
          if(!to.write(chunk))await Promise.race([once(to,'drain'),once(to,'close')]).catch(()=>{});
        }).catch(()=>{client.destroy();upstream.destroy();});
      });
    };
    forward(client,upstream,'upBytes');forward(upstream,client,'downBytes');
  });
  await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(port,'127.0.0.1',resolve);});
  const address=server.address();
  return{port:address.port,url:`http://127.0.0.1:${address.port}`,stats,stop:async()=>{for(const socket of sockets)socket.destroy();await new Promise(resolve=>server.close(resolve));}};
}
