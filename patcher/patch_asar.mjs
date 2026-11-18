import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execFileSync, spawnSync } from 'node:child_process';

/**
 * Resolves the live Ropilot Studio `app.asar` path across Windows, macOS, and Linux.
 * @returns {{ liveAsar: string, workspaceAsar: string }} Candidate bundle paths.
 */
function ResolveAsarPaths() {
  const home = os.homedir();
  const candidates = [];
  if (process.platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA || path.join(home, 'AppData', 'Local');
    candidates.push(path.join(localAppData, 'Programs', 'Ropilot Studio', 'resources', 'app.asar'));
  } else if (process.platform === 'darwin') {
    candidates.push(
      '/Applications/Ropilot Studio.app/Contents/Resources/app.asar',
      path.join(home, 'Applications', 'Ropilot Studio.app', 'Contents', 'Resources', 'app.asar')
    );
  } else {
    candidates.push(
      '/opt/Ropilot Studio/resources/app.asar',
      path.join(home, '.local', 'share', 'Ropilot Studio', 'resources', 'app.asar')
    );
  }
  const liveAsar = candidates.find((p) => fs.existsSync(p)) || candidates[0];
  const workspaceAsar = path.join('D:', 'Skills RE', 'Program', 'Ropilot Studio', 'resources', 'app.asar');
  return { liveAsar, workspaceAsar };
}

function ReadFile(filePath) {
  return fs.readFileSync(filePath, 'utf8').replace(/\r\n/g, '\n');
}

function WriteFile(filePath, content) {
  fs.writeFileSync(filePath, content, 'utf8');
}

function AssertReplace(text, target, replacement, label) {
  if (!text.includes(target)) {
    throw new Error(`Anchor not found for ${label}`);
  }
  return text.replace(target, replacement);
}

function PatchMainJs(rootDir) {
  const filePath = path.join(rootDir, 'main.min.js');
  let raw = ReadFile(filePath);
  if (raw.includes('get-agy-models')) {
    return;
  }

  const anchor = 'h.handle("get-agy-status",';
  const handler = `let _agyModelsCache=null;h.handle("get-agy-models",async()=>{const now=Date.now();if(_agyModelsCache&&now-_agyModelsCache.ts<60000)return _agyModelsCache;const os=require("os"),fs=require("fs"),path=require("path"),{execFile}=require("child_process");const binName=process.platform==="win32"?"agy.exe":"agy";let bin=[path.join(os.homedir(),".gemini","bin",binName),"/opt/homebrew/bin/"+binName,"/usr/local/bin/"+binName].find(p=>fs.existsSync(p))||binName;return await new Promise(resolve=>{execFile(bin,["models"],{timeout:12000,windowsHide:!0},(err,stdout)=>{if(err||!stdout){resolve({success:!1,models:[],error:err?err.message:"empty"});return;}const out=[];for(const line of String(stdout).split(/\\r?\\n/)){const parts=line.split("\\t");if(parts.length<2)continue;const id=parts[0].trim(),label=parts[1].trim();if(!id||id.startsWith("Fetching"))continue;out.push({id,label});}_agyModelsCache={success:!0,models:out,ts:Date.now()};resolve(_agyModelsCache);});});});`;

  raw = AssertReplace(raw, anchor, handler + anchor, 'main.min.js get-agy-models');

  WriteFile(filePath, raw);
}

function PatchPreloadJs(rootDir) {
  const filePath = path.join(rootDir, 'preload.js');
  let raw = ReadFile(filePath);
  if (raw.includes('getAgyModels')) {
    return;
  }
  const anchor = "getAgyStatus: () => ipcRenderer.invoke('get-agy-status'),";
  const replacement = `${anchor}\n  getAgyModels: () => ipcRenderer.invoke('get-agy-models'),`;
  raw = AssertReplace(raw, anchor, replacement, 'preload.js getAgyModels');

  WriteFile(filePath, raw);
}

function PatchModelCatalogJs(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'models', 'model-catalog.js');
  let raw = ReadFile(filePath);

  if (!raw.includes('export function syncAgyModels')) {
    const helperBlock = `
const AGY_EFFORT_ORDER = ['low', 'medium', 'high', 'xhigh'];
const dynamicGeminiEfforts = new Map();

function ParseAgyCatalogRows(rows) {
  const grouped = new Map();
  for (const row of Array.isArray(rows) ? rows : []) {
    const rawId = String(row?.id || '').trim();
    const rawLabel = String(row?.label || '').trim();
    if (!rawId || !rawId.startsWith('gemini-')) continue;
    const match = rawId.match(/^(.*)-(low|medium|high|xhigh)$/);
    const baseId = match ? match[1] : rawId;
    const effort = match ? match[2] : null;
    const cleanLabel = rawLabel.replace(/\\s*\\((Low|Medium|High|XHigh)\\)$/i, '').trim() || baseId;
    if (!grouped.has(baseId)) {
      grouped.set(baseId, { value: baseId, label: cleanLabel, provider: 'gemini', supportsFastMode: false, efforts: new Set() });
    }
    if (effort) grouped.get(baseId).efforts.add(effort);
  }
  return [...grouped.values()];
}

export function syncAgyModels(rows) {
  const parsed = ParseAgyCatalogRows(rows);
  if (parsed.length === 0) return false;
  for (let i = MODEL_CATALOG.length - 1; i >= 0; i--) {
    if (MODEL_CATALOG[i].provider === 'gemini') MODEL_CATALOG.splice(i, 1);
  }
  dynamicGeminiEfforts.clear();
  for (const entry of parsed) {
    const levels = AGY_EFFORT_ORDER.filter((lvl) => entry.efforts.has(lvl));
    dynamicGeminiEfforts.set(entry.value, levels);
    MODEL_CATALOG.push({
      value: entry.value,
      label: entry.label,
      provider: 'gemini',
      supportsFastMode: false,
    });
  }
  DEFAULT_MODEL_BY_PROVIDER.gemini = parsed[0].value;
  return true;
}

export function getDynamicGeminiEfforts(modelValue) {
  return dynamicGeminiEfforts.get(modelValue) || null;
}
`;
    const anchor = "export const COMPOSER_MODEL_PROVIDER_ORDER =";
    raw = AssertReplace(raw, anchor, `${helperBlock}\n${anchor}`, 'model-catalog.js syncAgyModels');
  }

  if (!raw.includes('getDynamicGeminiEfforts(normalizedModel)')) {
    raw = AssertReplace(
      raw,
      "  if (provider === 'gemini') {\n    return normalizedModel.startsWith('gemini-4-hydrogen');\n  }",
      "  if (provider === 'gemini') {\n    const dyn = getDynamicGeminiEfforts(normalizedModel);\n    if (dyn) return dyn.includes('xhigh');\n    return normalizedModel.startsWith('gemini-4-hydrogen');\n  }",
      'model-catalog.js dynamic xhigh'
    );
  }

  WriteFile(filePath, raw);
}

function PatchAgyUsageJs(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'ui', 'agy-usage.js');
  let raw = ReadFile(filePath);

  if (!raw.includes('syncAgyModels')) {
    raw = AssertReplace(
      raw,
      "import { electronAPI } from '../core/platform.js';",
      "import { electronAPI } from '../core/platform.js';\nimport { syncAgyModels } from '../models/model-catalog.js';",
      'agy-usage.js import syncAgyModels'
    );
  }

  if (!raw.includes('RopilotGemini by xDTaraZ')) {
    raw = AssertReplace(
      raw,
      "  }).join('');\n}",
      "  }).join('') + '<div style=\"margin-top:8px;padding-top:6px;border-top:1px solid rgba(255,255,255,0.08);font-size:11px;opacity:0.65;text-align:right\">RopilotGemini by xDTaraZ (github.com/xDTaraZz)</div>';\n}",
      'agy-usage.js xDTaraZ credit'
    );
  }

  if (!raw.includes('getAgyModels')) {
    raw = AssertReplace(
      raw,
      'export async function refreshAgyUsage(forceRefresh = false) {',
      `export async function refreshAgyModels(app) {
  try {
    const res = await electronAPI()?.getAgyModels?.();
    if (res?.success && Array.isArray(res.models) && syncAgyModels(res.models)) {
      app?.composerDropdowns?.renderComposerModelOptions?.();
    }
  } catch {}
}

export async function refreshAgyUsage(forceRefresh = false) {`,
      'agy-usage.js refreshAgyModels'
    );
  }

  if (!raw.includes('_modelsFetched')) {
    raw = AssertReplace(
      raw,
      'export function syncAgyUsageIndicator(app) {\n  const wrap =',
      'export function syncAgyUsageIndicator(app) {\n  if (!syncAgyUsageIndicator._modelsFetched) {\n    syncAgyUsageIndicator._modelsFetched = true;\n    void refreshAgyModels(app);\n  }\n  const wrap =',
      'agy-usage.js syncAgyUsageIndicator models hook'
    );
  }

  WriteFile(filePath, raw);
}

function PatchMessageHandlerJs(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'message-handler.js');
  let raw = ReadFile(filePath);
  const deadBlock = /\r?\n\s*if \(command\.startsWith\('__ROPILOT_THINKING__:'\)\) \{[\s\S]*?\r?\n\s*return;\r?\n\s*\}\r?\n/;
  if (deadBlock.test(raw)) {
    raw = raw.replace(deadBlock, '\n');
    WriteFile(filePath, raw);
  }
}

function VerifySyntax(rootDir) {
  const files = [
    'main.min.js',
    'preload.js',
    path.join('src', 'app.js'),
    path.join('src', 'js', 'message-handler.js'),
    path.join('src', 'js', 'models', 'model-catalog.js'),
    path.join('src', 'js', 'ui', 'agy-usage.js'),
  ];
  for (const rel of files) {
    const full = path.join(rootDir, rel);
    if (!fs.existsSync(full)) continue;
    const res = spawnSync(process.execPath, ['--check', full], { stdio: 'inherit' });
    if (res.status !== 0) {
      throw new Error(`Syntax check failed: ${rel}`);
    }
  }
}

function Run() {
  const { liveAsar, workspaceAsar } = ResolveAsarPaths();
  if (!fs.existsSync(liveAsar)) {
    throw new Error(`Ropilot Studio app.asar not found at: ${liveAsar}`);
  }

  const backupAsar = `${liveAsar}.bak`;
  if (!fs.existsSync(backupAsar)) {
    fs.copyFileSync(liveAsar, backupAsar);
  }

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ropilot-gemini-asar-'));
  try {
    const inAsar = path.join(tmpDir, 'in.asar');
    fs.copyFileSync(liveAsar, inAsar);
    const extractDir = path.join(tmpDir, 'app');
    const asarCmd = 'npx';
    const execOpts = { stdio: 'inherit', shell: process.platform === 'win32' };
    execFileSync(asarCmd, ['--yes', '@electron/asar', 'extract', inAsar, extractDir], execOpts);

    PatchMainJs(extractDir);
    PatchPreloadJs(extractDir);
    PatchModelCatalogJs(extractDir);
    PatchAgyUsageJs(extractDir);
    PatchMessageHandlerJs(extractDir);
    VerifySyntax(extractDir);

    const outAsar = path.join(tmpDir, 'app.asar');
    execFileSync(asarCmd, ['--yes', '@electron/asar', 'pack', extractDir, outAsar], execOpts);
    fs.copyFileSync(outAsar, liveAsar);
    if (fs.existsSync(path.dirname(workspaceAsar))) {
      fs.copyFileSync(outAsar, workspaceAsar);
    }
    console.log('RopilotGemini app.asar patched cleanly.');
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

Run();