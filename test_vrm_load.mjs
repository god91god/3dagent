// 用项目里的 three + three-vrm 加载 VRM，检查表达式/绑定/lookAt 是否正常
globalThis.self = globalThis;
import * as THREE from 'three';
import { GLTFLoader } from 'three/examples/jsm/loaders/GLTFLoader.js';
import { VRMLoaderPlugin } from '@pixiv/three-vrm';
import fs from 'node:fs';

const path = process.argv[2] || 'D:/3Dagent/model/优香_new.vrm';
const buf = fs.readFileSync(path);
const ab = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);

const loader = new GLTFLoader();
loader.register((parser) => new VRMLoaderPlugin(parser));
loader.parse(ab, '', (gltf) => {
  const vrm = gltf.userData.vrm;
  console.log('=== 模型加载 ===');
  console.log('meta name:', vrm.meta?.name);
  console.log('spec: VRM 1.0 =', !!vrm.meta, '| expressionManager:', !!vrm.expressionManager);
  console.log('lookAt:', vrm.lookAt ? '存在' : 'MISSING');
  console.log('humanoid:', vrm.humanoid ? '存在' : 'MISSING');

  if (vrm.expressionManager) {
    const exps = vrm.expressionManager.expressions;
    console.log('\n=== 表情列表 (' + exps.length + ') ===');
    for (const e of exps) {
      const type = e.expressionName;
      const binds = e.binds ?? [];
      const desc = binds.map((b) => {
        return 'mesh#' + (b.meshIndex ?? b.primitives?.[0]?.id ?? '?') + '[' + b.index + ']x' + b.weight;
      }).join(', ') || '(无绑定)';
      console.log(' -', type, '=>', desc);
    }
    const withBinds = exps.filter((e) => (e.binds ?? []).length > 0);
    console.log('\n有绑定的表情数:', withBinds.length, '/', exps.length);
  }

  vrm.update(0.016);
  console.log('\nvrm.update(0.016) OK');
}, (err) => {
  console.error('加载失败:', err);
  process.exit(1);
});
