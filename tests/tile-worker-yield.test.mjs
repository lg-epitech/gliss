import assert from 'node:assert/strict';
import test from 'node:test';
import {TaskYield,TileWorkerCore} from '../web/tile-worker.js';

test('two fairness tasks are bounded and close releases pending waiters and both ports',async()=>{
  const delivered=[],closed=[];let port1;const task=new TaskYield(()=>({port1:port1={start(){},close(){closed.push(1);}},port2:{postMessage(){delivered.push(()=>port1.onmessage());},close(){closed.push(2);}}}));
  let first=false,second=false;const a=task.wait().then(()=>first=true),b=task.wait().then(()=>second=true);assert.throws(()=>task.wait());assert.equal(first,false);assert.equal(second,false);
  delivered.shift()();await a;assert.equal(first,true);assert.equal(second,false);assert.equal(task.pending.length,1);task.close();await b;assert.deepEqual(closed,[1,2]);assert.equal(task.pending.length,0);task.close();assert.deepEqual(closed,[1,2]);await task.wait();
});
test('real task channel resumes both roles and explicitly retires its ports',async()=>{
  const task=new TaskYield();try{await Promise.all([task.wait(),task.wait()]);assert.equal(task.pending.length,0);}finally{task.close();}assert.equal(task.closed,true);
});
test('worker close releases a scheduled task before any further decoder work',async()=>{
  const core=new TileWorkerCore({config:{payloadStride:1024,policy:0},now:()=>0,post:()=>{}}),pending=core.taskYield.wait();await core.close();await pending;assert.equal(core.closed,true);assert.equal(core.taskYield.closed,true);assert.equal(core.taskYield.pending.length,0);
});
