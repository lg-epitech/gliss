import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFile} from 'node:fs/promises';
import {cpus,release} from 'node:os';
import {withDecoder} from './browser.mjs';
import {kinds,makeCase,verifierSource} from './corpus.mjs';
import {CPU_BROWSER_ARGS} from '../tests/helpers.mjs';

const startedAt=new Date().toISOString();
const result=await withDecoder(async(page,chromium)=>{
  await page.addScriptTag({content:verifierSource()});
  const rows=[];
  for(const [width,height] of [[1280,800],[1920,1080]])for(const kind of kinds) {
    const input=makeCase(width,height,kind);
    const packetHashes=[input.key,...input.deltas].map(s=>createHash('sha256').update(Buffer.from(s,'base64')).digest('hex'));
    const measured=await page.evaluate(async input=>{
      const decode=s=>Uint8Array.from(atob(s),c=>c.charCodeAt(0)).buffer;
      const decoder=new window.Decoder(),packets=input.deltas.map(decode);
      await decoder.decode(decode(input.key));
      let sequence=0,frame;
      const deliver=async index=>{
        new DataView(packets[index%2]).setUint32(4,++sequence,true);
        frame=await decoder.decode(packets[index%2]);
        if(frame.sequence!==sequence)throw new Error('Sequence differs');
      };
      for(let i=0;i<4;i++)await deliver(i);
      window.verifyPixels(frame,input,4);
      let count=0;
      const start=performance.now();
      do {await deliver(count++);}while(count<8||(count<65536&&performance.now()-start<120));
      const elapsed=performance.now()-start;
      window.verifyPixels(frame,input,count+4);
      return {count,elapsed,ms:elapsed/count,sequence};
    },input);
    assert.ok(Number.isFinite(measured.ms)&&measured.ms>0&&measured.count>=8);
    rows.push({width,height,kind,...measured,packetHashes});
  }
  return {chromium,scope:'Lossless decoder elapsed time in CPU Chromium; packet construction, warmup and scalar whole-frame checks excluded. Equal-weight geometric mean across 24 fixed synthetic cases. No network, sender, Canvas or physical-display claim.',rows};
});
result.sourceSHA256=createHash('sha256').update(await readFile('web/decoder.js')).digest('hex');
result.environment={startedAt,finishedAt:new Date().toISOString(),node:process.version,linux:release(),cpu:cpus()[0].model,
  affinity:(await readFile('/proc/self/status','utf8')).match(/^Cpus_allowed_list:[ \t]+(.+)$/m)?.[1],browserArgs:CPU_BROWSER_ARGS};
result.geometricMeanMs=Math.exp(result.rows.reduce((sum,r)=>sum+Math.log(r.ms),0)/result.rows.length);
console.log(JSON.stringify(result));
console.log(`METRIC decoder_log_ms=${Math.log(result.geometricMeanMs)}`);
