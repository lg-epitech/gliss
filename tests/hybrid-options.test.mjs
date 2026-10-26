import test from 'node:test';
import assert from 'node:assert/strict';
import {hybridServerArgs} from './helpers.mjs';

test('native helper preserves old default arguments and admits only explicit Hybrid options', () => {
  assert.deepEqual(hybridServerArgs(), []);
  for (const codec of ['deflate','global','sparse','h264','hybrid']) assert.deepEqual(hybridServerArgs({codec}), []);
  assert.deepEqual(hybridServerArgs({codec:'hybrid',hybridBase:'global-cached',hybridVideoProfile:'camera-scene-off'}),
    ['--hybrid-base','global-cached','--hybrid-video-profile','camera-scene-off']);
  assert.deepEqual(hybridServerArgs({codec:'hybrid',hybridVideoProfile:'camera-scene-on'}), ['--hybrid-video-profile','camera-scene-on']);
  assert.deepEqual(hybridServerArgs({codec:'deflate',hybridBase:'global',hybridVideoProfile:'screen'}),
    ['--hybrid-base','global','--hybrid-video-profile','screen']);
});

test('native helper rejects nondefault Hybrid settings for other codecs and unsupported profiles', () => {
  for (const codec of ['deflate','h264','global-cached']) {
    assert.throws(() => hybridServerArgs({codec,hybridBase:'global-cached'}));
    assert.throws(() => hybridServerArgs({codec,hybridVideoProfile:'camera-scene-off'}));
  }
  for (const hybridBase of ['adaptive','GLOBAL',null,0]) assert.throws(() => hybridServerArgs({codec:'hybrid',hybridBase}));
  for (const hybridVideoProfile of ['screen-scene-off','camera','gpu',null,0]) assert.throws(() => hybridServerArgs({codec:'hybrid',hybridVideoProfile}));
});

test('fine grid flags are explicit and cannot affect prior codecs', () => {
  assert.deepEqual(hybridServerArgs({codec:'hybrid-grid'}), []);
  assert.deepEqual(hybridServerArgs({codec:'hybrid-grid',hybridMaskGrid:16,hybridBase:'global-cached'}),
    ['--hybrid-base','global-cached','--hybrid-mask-grid','16']);
  for(const codec of ['hybrid','h264','deflate'])assert.throws(()=>hybridServerArgs({codec,hybridMaskGrid:32}));
  for(const hybridMaskGrid of [0,17,64,'16',null])assert.throws(()=>hybridServerArgs({codec:'hybrid-grid',hybridMaskGrid}));
});
