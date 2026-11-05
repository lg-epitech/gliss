import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {resolve} from 'node:path';
import {launchBrowser} from '../tests/helpers.mjs';

export async function withDecoder(run,sourcePath='web/decoder.js') {
  const source=await readFile(resolve(sourcePath),'utf8');
  const server=createServer((request,response)=>{
    response.setHeader('Content-Type',request.url==='/decoder.js'?'text/javascript':'text/html');
    response.end(request.url==='/decoder.js'?source:'<!doctype html><script type="module">import {Decoder} from "/decoder.js";window.Decoder=Decoder;</script>');
  });
  let browser;
  try {
    await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
    browser=await launchBrowser();
    const page=await browser.newPage(),errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.waitForFunction(()=>window.Decoder);
    const result=await run(page,browser.version());
    assert.deepEqual(errors,[]);
    return result;
  } finally {
    await browser?.close();
    await new Promise(resolve=>server.close(resolve));
  }
}
