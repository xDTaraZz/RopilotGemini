import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';
import * as asar from '@electron/asar';

const AGY_USAGE_SOURCE = `import { electronAPI } from '../core/platform.js';
import { syncAgyModels } from '../models/model-catalog.js';

const RING_CIRCUMFERENCE = 2 * Math.PI * 6;
const CREDIT_FOOTER_HTML = '<div style="margin-top:8px;padding-top:6px;border-top:1px solid rgba(255,255,255,0.08);font-size:11px;opacity:0.65;text-align:right">RopilotGemini by xDTaraZ (github.com/xDTaraZz)</div>';
let latestUsageState = null;
let listenersBound = false;

function FormatResetCountdown(resetIso) {
  const targetMs = Date.parse(resetIso || '');
  if (!Number.isFinite(targetMs)) return '';
  const diffMin = Math.max(0, Math.round((targetMs - Date.now()) / 60000));
  if (diffMin < 60) return \`Resets in \${diffMin}m\`;
  const totalHours = Math.floor(diffMin / 60);
  const mins = diffMin % 60;
  if (totalHours < 24) return mins > 0 ? \`Resets in \${totalHours}h \${mins}m\` : \`Resets in \${totalHours}h\`;
  const days = Math.floor(totalHours / 24);
  const hours = totalHours % 24;
  return hours > 0 ? \`Resets in \${days}d \${hours}h\` : \`Resets in \${days}d\`;
}

function GetRingColor(pct) {
  if (pct <= 15) return '#ef4444';
  if (pct <= 40) return '#f59e0b';
  return '#4ade80';
}

function BuildRingSvg(pct, size = 14) {
  const clamped = Math.max(0, Math.min(100, pct));
  const offset = RING_CIRCUMFERENCE * (1 - clamped / 100);
  const color = GetRingColor(clamped);
  return \`<svg width="\${size}" height="\${size}" viewBox="0 0 16 16" aria-hidden="true">\`
    + \`<circle cx="8" cy="8" r="6" fill="none" stroke="rgba(255,255,255,0.16)" stroke-width="2"></circle>\`
    + \`<circle cx="8" cy="8" r="6" fill="none" stroke="\${color}" stroke-width="2" stroke-linecap="round" \`
    + \`stroke-dasharray="\${RING_CIRCUMFERENCE.toFixed(2)}" stroke-dashoffset="\${offset.toFixed(2)}" \`
    + \`transform="rotate(-90 8 8)"></circle></svg>\`;
}

function GetBucketLabel(bucket) {
  if (bucket?.window === 'weekly') return 'Weekly limit';
  if (bucket?.window === '5h') return '5h limit';
  return bucket?.name || 'Limit';
}

function RenderUsageError(message) {
  const body = document.getElementById('agy-usage-popover-body');
  const pillPct = document.getElementById('agy-usage-pill-pct');
  const pillRing = document.getElementById('agy-usage-pill-ring');
  if (pillRing) pillRing.innerHTML = BuildRingSvg(100, 14);
  if (pillPct) pillPct.textContent = 'AGY';
  if (body) {
    const safeMsg = String(message || 'Unable to load Antigravity quota.')
      .replace(/[&<>"]/g, (ch) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[ch]));
    body.innerHTML = \`<div class="agy-usage-empty">\${safeMsg}</div>\${CREDIT_FOOTER_HTML}\`;
  }
}

function RenderUsagePopover(data) {
  const body = document.getElementById('agy-usage-popover-body');
  const pillPct = document.getElementById('agy-usage-pill-pct');
  const pillRing = document.getElementById('agy-usage-pill-ring');
  if (!body || !pillPct || !pillRing) return;

  const groups = Array.isArray(data?.groups) ? data.groups : [];
  const geminiGroup = groups.find((g) => String(g?.name || '').toLowerCase().includes('gemini')) || groups[0];
  const fiveHour = geminiGroup?.buckets?.find((b) => b?.window === '5h') || geminiGroup?.buckets?.[0];
  const rawFraction = Number(fiveHour?.remaining_fraction);
  const primaryPct = Number.isFinite(rawFraction) ? Math.round(rawFraction * 100) : null;

  pillRing.innerHTML = BuildRingSvg(primaryPct ?? 100, 14);
  pillPct.textContent = primaryPct !== null ? \`\${primaryPct}%\` : 'AGY';

  if (!groups.length) {
    body.innerHTML = \`<div class="agy-usage-empty">Loading Antigravity quota...</div>\${CREDIT_FOOTER_HTML}\`;
    return;
  }

  body.innerHTML = groups.map((group) => {
    const rows = (group.buckets || []).map((bucket) => {
      const frac = Number(bucket.remaining_fraction);
      const pct = Number.isFinite(frac) ? Math.round(frac * 100) : 0;
      const resetText = FormatResetCountdown(bucket.reset_time);
      return \`<div class="agy-usage-row">\`
        + \`<div class="agy-usage-row-left">\`
        + \`<div class="agy-usage-limit-name">\${GetBucketLabel(bucket)}</div>\`
        + (resetText ? \`<div class="agy-usage-reset">\${resetText}</div>\` : '')
        + \`</div>\`
        + \`<div class="agy-usage-row-right">\`
        + \`<span class="agy-usage-pct">\${pct}% left</span>\`
        + BuildRingSvg(pct, 15)
        + \`</div>\`
        + \`</div>\`;
    }).join('');
    return \`<div class="agy-usage-group">\`
      + \`<div class="agy-usage-group-title">\${group.name || 'Models'}</div>\`
      + rows
      + \`</div>\`;
  }).join('') + CREDIT_FOOTER_HTML;
}

export async function refreshAgyModels(app) {
  try {
    const res = await electronAPI()?.getAgyModels?.();
    if (res?.success && Array.isArray(res.models) && syncAgyModels(res.models)) {
      app?.refreshModelOptions?.();
      app?.composerDropdowns?.syncComposerDropdown?.('composer-model-control');
      return true;
    }
  } catch {}
  return false;
}

export async function refreshAgyUsage(forceRefresh = false) {
  try {
    const res = await electronAPI().getAgyUsage?.(forceRefresh);
    if (res?.success && res.data) {
      latestUsageState = res.data;
      RenderUsagePopover(latestUsageState);
    } else if (res && res.success === false && !latestUsageState) {
      RenderUsageError(res.error);
    }
  } catch (err) {
    if (!latestUsageState) RenderUsageError(err?.message);
  }
}

export function invalidateAndRefreshAgyUsage(app) {
  if (app?.normalizeModelProvider?.(app.modelProvider) !== 'gemini') return;
  if (app?.isUseRopilotCreditsEnabled?.() === true) return;
  void refreshAgyUsage(true);
}

function EnsureUsageListeners() {
  if (listenersBound) return;
  listenersBound = true;
  const wrap = document.getElementById('agy-usage-indicator');
  const btn = document.getElementById('agy-usage-button');
  const refreshBtn = document.getElementById('agy-usage-refresh');
  if (btn && wrap) {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      const willOpen = !wrap.classList.contains('is-open');
      wrap.classList.toggle('is-open', willOpen);
      if (willOpen) refreshAgyUsage(false);
    });
    document.addEventListener('click', (e) => {
      if (!wrap.contains(e.target)) wrap.classList.remove('is-open');
    });
  }
  if (refreshBtn) {
    refreshBtn.addEventListener('click', async (e) => {
      e.stopPropagation();
      refreshBtn.classList.add('is-spinning');
      await refreshAgyUsage(true);
      refreshBtn.classList.remove('is-spinning');
    });
  }
}

export function syncAgyUsageIndicator(app) {
  if (!syncAgyUsageIndicator._modelsFetched) {
    syncAgyUsageIndicator._modelsFetched = true;
    void refreshAgyModels(app).then((ok) => {
      if (!ok) syncAgyUsageIndicator._modelsFetched = false;
    });
  }
  const wrap = document.getElementById('agy-usage-indicator');
  if (!wrap) return;
  EnsureUsageListeners();

  const isGeminiActive = app?.normalizeModelProvider?.(app.modelProvider) === 'gemini'
    && app?.isUseRopilotCreditsEnabled?.() !== true;
  wrap.classList.toggle('hidden', !isGeminiActive);
  if (!isGeminiActive) {
    wrap.classList.remove('is-open');
    return;
  }

  if (latestUsageState) {
    RenderUsagePopover(latestUsageState);
  } else {
    RenderUsagePopover(null);
    refreshAgyUsage(false);
  }
}
`;

/**
 * Resolves the live Ropilot Studio `app.asar` path across Windows, macOS, and Linux.
 * @returns {string} Path to the installed `app.asar`.
 */
function ResolveAsarPath() {
  const idx = process.argv.indexOf('--target');
  if (idx !== -1 && process.argv[idx + 1]) {
    return path.resolve(process.argv[idx + 1]);
  }
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
  return candidates.find((p) => fs.existsSync(p)) || candidates[0];
}

function ReadFile(filePath) {
  return fs.readFileSync(filePath, 'utf8').replace(/\r\n/g, '\n');
}

function WriteFile(filePath, content, crlf = true) {
  const out = crlf ? content.replace(/\n/g, '\r\n') : content;
  fs.writeFileSync(filePath, out, 'utf8');
}

function AssertReplace(text, target, replacement, label) {
  if (!text.includes(target)) {
    throw new Error(`Anchor not found for ${label}`);
  }
  return text.replace(target, replacement);
}

function Sha256File(filePath) {
  return crypto.createHash('sha256').update(fs.readFileSync(filePath)).digest('hex');
}

function PatchClaudeManager(rootDir) {
  const filePath = path.join(rootDir, 'lib', 'claude-manager.js');
  let raw = ReadFile(filePath);
  if (raw.includes('getRealClaudeBinaryPath')) return;

  raw = AssertReplace(
    raw,
    `  return path.join(getClaudeDir(), binaryName);\n}`,
    `  return path.join(getClaudeDir(), binaryName);\n}\n\nfunction getRealClaudeBinaryPath() {\n  const binaryName = process.platform === 'win32' ? 'claude-real.exe' : 'claude-real';\n  return path.join(getClaudeDir(), binaryName);\n}`,
    'claude-manager.js getRealClaudeBinaryPath'
  );
  raw = AssertReplace(
    raw,
    `function isClaudeInstalled() {\n  return fs.existsSync(getClaudeBinaryPath());\n}`,
    `function isClaudeInstalled() {\n  return fs.existsSync(getRealClaudeBinaryPath());\n}`,
    'claude-manager.js isClaudeInstalled'
  );
  raw = AssertReplace(
    raw,
    `  const binaryUrl = \`\${gcsBase}/\${version}/\${platformKey}/\${binaryName}\`;\n  const destPath = getClaudeBinaryPath();`,
    `  const binaryUrl = \`\${gcsBase}/\${version}/\${platformKey}/\${binaryName}\`;\n  const destPath = getRealClaudeBinaryPath();`,
    'claude-manager.js downloadClaudeBinary'
  );
  raw = AssertReplace(
    raw,
    `  if (!authenticated && installed) {\n    const binaryPath = getClaudeBinaryPath();`,
    `  if (!authenticated && installed) {\n    const binaryPath = getRealClaudeBinaryPath();`,
    'claude-manager.js auth fallback'
  );
  WriteFile(filePath, raw);
}

function PatchMainJs(rootDir) {
  const filePath = path.join(rootDir, 'main.min.js');
  let raw = ReadFile(filePath);

  if (raw.includes('he()&&t.protocol==="file:"')) {
    raw = AssertReplace(
      raw,
      `return he()&&t.protocol==="file:"?!0:t.origin===O.appUrl||t.origin===O.siteUrl`,
      `return t.protocol==="file:"||t.origin===O.appUrl||t.origin===O.siteUrl`,
      'main.min.js isAllowedUrl'
    );
  }
  const stockLoader = `function vr(e){let t=B.join(__dirname,"src","index.html");return!e||e.isDestroyed()?Promise.resolve():he()?(console.log("Development mode: Loading local UI"),e.loadFile(t)):e.loadURL(js()).catch(n=>(console.log("Failed to load remote UI, falling back to local:",n.message),e.loadFile(t)))}`;
  if (raw.includes(stockLoader)) {
    raw = AssertReplace(
      raw,
      stockLoader,
      `function vr(e){let t=B.join(__dirname,"src","index.html");return!e||e.isDestroyed()?Promise.resolve():e.loadFile(t)}`,
      'main.min.js loadUI'
    );
  }
  const autoUpd = `setTimeout(()=>{let o=pr();Cr.checkAppUpdate(o.version)},2e3),y.on("activate",`;
  if (raw.includes(autoUpd)) {
    raw = AssertReplace(raw, autoUpd, `void 0,y.on("activate",`, 'main.min.js disable auto-update');
  }

  const agyHandlers = `let _agyModelsCache=null;h.handle("get-agy-models",async()=>{const now=Date.now();if(_agyModelsCache&&now-_agyModelsCache.ts<60000)return _agyModelsCache;const os=require("os"),fs=require("fs"),path=require("path"),{execFile}=require("child_process");const binName=process.platform==="win32"?"agy.exe":"agy";let bin=[path.join(os.homedir(),".gemini","bin",binName),"/opt/homebrew/bin/"+binName,"/usr/local/bin/"+binName].find(p=>fs.existsSync(p))||binName;return await new Promise(resolve=>{execFile(bin,["models"],{timeout:12000,windowsHide:!0},(err,stdout)=>{if(err||!stdout){resolve({success:!1,models:[],error:err?err.message:"empty"});return;}const out=[];for(const line of String(stdout).split(/\\r?\\n/)){const parts=line.split("\\t");if(parts.length<2)continue;const id=parts[0].trim(),label=parts[1].trim();if(!id||id.startsWith("Fetching"))continue;out.push({id,label});}_agyModelsCache={success:!0,models:out,ts:Date.now()};resolve(_agyModelsCache);});});});h.handle("get-agy-status",()=>{const e=require("fs"),t=require("path"),n=require("os"),binName=process.platform==="win32"?"agy.exe":"agy";const cands=[t.join(n.homedir(),".gemini","bin",binName),"/opt/homebrew/bin/"+binName,"/usr/local/bin/"+binName];for(const p of cands){if(e.existsSync(p))return{installed:!0,path:p}}try{const cmd=process.platform==="win32"?"where.exe":"which";let o=require("child_process").execFileSync(cmd,[binName],{encoding:"utf8",timeout:3e3,windowsHide:!0}).split(/\\r?\\n/).find(Boolean);if(o&&e.existsSync(o.trim()))return{installed:!0,path:o.trim()}}catch{}return{installed:!1,path:cands[0]}});let _agyUsageCache=null,_agyUsagePending=null;h.on("set-agy-route",(e,t)=>{try{let n=require("path"),r=require("fs"),o=n.join(y.getPath("userData"),"agy-routes");r.existsSync(o)||r.mkdirSync(o,{recursive:!0});let s=String(t?.chatId||"default").replace(/[^a-zA-Z0-9._-]/g,"_"),i=JSON.stringify({provider:String(t?.provider||""),model:String(t?.model||""),effort:String(t?.effort||""),ts:Date.now()});r.writeFileSync(n.join(o,s+".json"),i,"utf8"),r.writeFileSync(n.join(y.getPath("userData"),"agy-route-latest.json"),i,"utf8"),e.returnValue={success:!0}}catch(n){e.returnValue={success:!1,error:n.message}}});h.handle("get-agy-usage",async(e,t)=>{let n=Date.now();if(!t&&_agyUsageCache&&n-_agyUsageCache.fetchedAt<45e3)return _agyUsageCache;if(_agyUsagePending)return _agyUsagePending;_agyUsagePending=new Promise(r=>{let o=require("path"),s=require("os"),i=require("fs"),a=require("child_process").execFile,binName=process.platform==="win32"?"agy.exe":"agy",c=[o.join(s.homedir(),".gemini","bin",binName),"/opt/homebrew/bin/"+binName,"/usr/local/bin/"+binName].find(p=>i.existsSync(p))||binName;a(c,["-p","/usage","--output-format","json"],{timeout:15e3,windowsHide:!0},(l,u)=>{if(l){r({success:!1,error:l.message});return}try{let d=JSON.parse(u.trim());_agyUsageCache={success:!0,data:d?.command?.data||null,fetchedAt:Date.now()};r(_agyUsageCache)}catch(d){r({success:!1,error:d.message})}})}).finally(()=>{_agyUsagePending=null});return _agyUsagePending});`;

  if (!raw.includes('get-agy-status')) {
    raw = AssertReplace(
      raw,
      `h.handle("get-claude-cli-status",()=>N.getStatus());`,
      `h.handle("get-claude-cli-status",()=>N.getStatus());${agyHandlers}`,
      'main.min.js agy handlers'
    );
  } else if (!raw.includes('get-agy-models')) {
    const modelsOnly = agyHandlers.slice(0, agyHandlers.indexOf('h.handle("get-agy-status"'));
    raw = AssertReplace(raw, `h.handle("get-agy-status",`, `${modelsOnly}h.handle("get-agy-status",`, 'main.min.js get-agy-models');
  }
  WriteFile(filePath, raw, false);
}

function PatchPreloadJs(rootDir) {
  const filePath = path.join(rootDir, 'preload.js');
  let raw = ReadFile(filePath);
  if (!raw.includes('getAgyStatus')) {
    raw = AssertReplace(
      raw,
      `  getClaudeCliStatus: () => ipcRenderer.invoke('get-claude-cli-status'),`,
      `  getClaudeCliStatus: () => ipcRenderer.invoke('get-claude-cli-status'),\n  getAgyStatus: () => ipcRenderer.invoke('get-agy-status'),\n  getAgyModels: () => ipcRenderer.invoke('get-agy-models'),\n  getAgyUsage: (forceRefresh) => ipcRenderer.invoke('get-agy-usage', forceRefresh),\n  setAgyRoute: (payload) => ipcRenderer.sendSync('set-agy-route', payload),`,
      'preload.js agy api'
    );
  } else if (!raw.includes('getAgyModels')) {
    raw = AssertReplace(
      raw,
      `  getAgyStatus: () => ipcRenderer.invoke('get-agy-status'),`,
      `  getAgyStatus: () => ipcRenderer.invoke('get-agy-status'),\n  getAgyModels: () => ipcRenderer.invoke('get-agy-models'),`,
      'preload.js getAgyModels'
    );
  }
  WriteFile(filePath, raw);
}

function PatchAppJs(rootDir) {
  const filePath = path.join(rootDir, 'src', 'app.js');
  let raw = ReadFile(filePath);
  if (raw.includes('if (creditsLevels[normalizedModel] === normalizedLevel) return;')) {
    raw = AssertReplace(
      raw,
      `if (creditsLevels[normalizedModel] === normalizedLevel) return;`,
      `if ((creditsLevels[normalizedModel] ?? '') === normalizedLevel && normalizedModel in creditsLevels) return;`,
      'app.js creditsLevels guard'
    );
  }
  if (raw.includes('if (levels[normalizedModel] === normalizedLevel) return;')) {
    raw = AssertReplace(
      raw,
      `if (levels[normalizedModel] === normalizedLevel) return;`,
      `if ((levels[normalizedModel] ?? '') === normalizedLevel && normalizedModel in levels) return;`,
      'app.js levels guard'
    );
  }
  if (!raw.includes('wirePresenceProvider') && raw.includes('provider: this.modelProvider,')) {
    raw = AssertReplace(
      raw,
      `provider: this.modelProvider,`,
      `provider: this.normalizeModelProvider?.(this.modelProvider) === 'gemini' ? 'claude' : this.modelProvider,`,
      'app.js discordPresence wireProvider'
    );
  }
  WriteFile(filePath, raw);
}

function PatchIndexHtml(rootDir) {
  const filePath = path.join(rootDir, 'src', 'index.html');
  let raw = ReadFile(filePath);
  if (raw.includes('id="agy-usage-indicator"')) return;
  raw = AssertReplace(
    raw,
    `<div class="input-right-actions">\n                        <div\n                          class="composer-context-indicator hidden"`,
    `<div class="input-right-actions">\n                        <div class="agy-usage-indicator hidden" id="agy-usage-indicator">\n                          <button type="button" class="agy-usage-button" id="agy-usage-button" aria-label="Antigravity Usage">\n                            <span id="agy-usage-pill-ring"></span>\n                            <span id="agy-usage-pill-pct">AGY</span>\n                          </button>\n                          <div class="agy-usage-popover" id="agy-usage-popover" role="dialog" aria-label="Antigravity Usage">\n                            <div class="agy-usage-header">\n                              <span class="agy-usage-title">Usage</span>\n                              <button type="button" class="agy-usage-refresh" id="agy-usage-refresh" title="Refresh quota" aria-label="Refresh quota">↻</button>\n                            </div>\n                            <div id="agy-usage-popover-body"></div>\n                          </div>\n                        </div>\n                        <div\n                          class="composer-context-indicator hidden"`,
    'index.html agy-usage-indicator'
  );
  WriteFile(filePath, raw);
}

function PatchStylesCss(rootDir) {
  const filePath = path.join(rootDir, 'src', 'styles.css');
  let raw = ReadFile(filePath);
  if (raw.includes('.agy-usage-indicator')) return;
  raw += `\n.agy-usage-indicator{position:relative;display:inline-flex;align-items:center}\n.agy-usage-indicator.hidden{display:none}\n.agy-usage-button{display:inline-flex;align-items:center;gap:5px;height:26px;padding:0 8px;border-radius:999px;border:1px solid rgba(255,255,255,.12);background:rgba(255,255,255,.04);color:var(--text-secondary);font-size:11px;font-weight:600;cursor:pointer}\n.agy-usage-popover{position:absolute;right:0;bottom:calc(100% + 8px);width:250px;padding:10px 12px;border-radius:10px;background:rgba(24,24,28,.96);border:1px solid rgba(255,255,255,.12);box-shadow:0 12px 32px rgba(0,0,0,.45);display:none;z-index:200}\n.agy-usage-indicator.is-open .agy-usage-popover{display:block}\n.agy-usage-header{display:flex;align-items:center;justify-content:space-between;margin-bottom:8px;font-size:12px;font-weight:600}\n.agy-usage-refresh{background:none;border:none;color:var(--text-secondary);cursor:pointer;font-size:13px}\n.agy-usage-refresh.is-spinning{animation:spin .8s linear infinite}\n.agy-usage-group{margin-top:6px}\n.agy-usage-group-title{font-size:11px;font-weight:600;opacity:.75;margin-bottom:4px}\n.agy-usage-row{display:flex;align-items:center;justify-content:space-between;font-size:11px;padding:3px 0}\n.agy-usage-row-right{display:inline-flex;align-items:center;gap:5px}\n.agy-usage-reset{font-size:10px;opacity:.6}\n.agy-usage-empty{font-size:11px;opacity:.75}\n`;
  WriteFile(filePath, raw);
}

function PatchModelCatalog(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'models', 'model-catalog.js');
  let raw = ReadFile(filePath);

  if (!raw.includes("provider: 'gemini'")) {
    raw = AssertReplace(
      raw,
      `    value: 'gpt-5.3-codex-spark',\n    label: 'GPT 5.3 Spark',\n    provider: 'codex',\n    supportsFastMode: false\n  }\n];`,
      `    value: 'gpt-5.3-codex-spark',\n    label: 'GPT 5.3 Spark',\n    provider: 'codex',\n    supportsFastMode: false\n  },\n  { value: 'gemini-4-carbon', label: 'Gemini 4 Carbon', provider: 'gemini', supportsFastMode: false },\n  { value: 'gemini-4-argon', label: 'Gemini 4 Argon', provider: 'gemini', supportsFastMode: false },\n  { value: 'gemini-4-hydrogen', label: 'Gemini 4 Hydrogen', provider: 'gemini', supportsFastMode: false },\n  { value: 'gemini-3.8-flash', label: 'Gemini 3.8 Flash', provider: 'gemini', supportsFastMode: false },\n  { value: 'gemini-pro-orchestrator', label: 'Orchestrator (Carbon)', provider: 'gemini', supportsFastMode: false },\n  { value: 'gemini-pro-orchestrator-argon', label: 'Orchestrator (Argon)', provider: 'gemini', supportsFastMode: false }\n];`,
      'model-catalog.js MODEL_CATALOG gemini'
    );
    raw = AssertReplace(
      raw,
      `  codex: 'gpt-6.1-sol',\n  ropilot: 'ropilot-blaze'\n};`,
      `  codex: 'gpt-6.1-sol',\n  ropilot: 'ropilot-blaze',\n  gemini: 'gemini-4-carbon'\n};`,
      'model-catalog.js DEFAULT_MODEL_BY_PROVIDER'
    );
    raw = AssertReplace(
      raw,
      `export const COMPOSER_MODEL_PROVIDER_ORDER = ['ropilot', 'claude', 'codex'];`,
      `export const COMPOSER_MODEL_PROVIDER_ORDER = ['ropilot', 'claude', 'codex', 'gemini'];`,
      'model-catalog.js COMPOSER_MODEL_PROVIDER_ORDER'
    );
    raw = AssertReplace(
      raw,
      `    return 'ropilot-blaze';\n  }\n  return DEFAULT_MODEL_BY_PROVIDER.claude;`,
      `    return 'ropilot-blaze';\n  }\n  if (MODEL_CATALOG.some((entry) => entry.provider === 'gemini' && entry.value === value)) return value;\n  const legacyGeminiBase = value.replace(/-(low|medium|high|xhigh)$/, '');\n  if (MODEL_CATALOG.some((entry) => entry.provider === 'gemini' && entry.value === legacyGeminiBase)) return legacyGeminiBase;\n  return DEFAULT_MODEL_BY_PROVIDER.claude;`,
      'model-catalog.js normalizeSelectedModel'
    );
    raw = AssertReplace(
      raw,
      `  if (value === 'ropilot' || value === 'google' || value === 'gemini') return 'ropilot';`,
      `  if (value === 'ropilot') return 'ropilot';\n  if (value === 'gemini' || value === 'agy' || value === 'antigravity' || value === 'google') return 'gemini';`,
      'model-catalog.js normalizeModelProvider'
    );
    raw = AssertReplace(
      raw,
      `  if (normalizedProvider === 'codex') return 'Codex';\n  return 'Claude Code';`,
      `  if (normalizedProvider === 'codex') return 'Codex';\n  if (normalizedProvider === 'gemini') return 'Gemini (AGY)';\n  return 'Claude Code';`,
      'model-catalog.js getProviderDisplayName'
    );
    raw = AssertReplace(
      raw,
      `export function supportsXHighReasoningForModel(model) {\n  const normalizedModel = normalizeSelectedModel(model);\n  const provider = getProviderForModel(normalizedModel);`,
      `export const NO_REASONING_EFFORT_MODEL_VALUES = new Set(['gemini-pro-orchestrator', 'gemini-pro-orchestrator-argon']);\nexport function supportsReasoningEffortForModel(model) {\n  const normalizedModel = normalizeSelectedModel(model);\n  const dyn = typeof getDynamicGeminiEfforts === 'function' ? getDynamicGeminiEfforts(normalizedModel) : null;\n  if (dyn) return dyn.length > 0;\n  return !NO_REASONING_EFFORT_MODEL_VALUES.has(normalizedModel);\n}\nexport function supportsXHighReasoningForModel(model) {\n  const normalizedModel = normalizeSelectedModel(model);\n  const provider = getProviderForModel(normalizedModel);\n  if (!supportsReasoningEffortForModel(normalizedModel)) return false;`,
      'model-catalog.js NO_REASONING_EFFORT_MODEL_VALUES'
    );
    raw = AssertReplace(
      raw,
      `      normalizedModel === 'claude-fable-5-1[1m]' ||\n      normalizedModel === 'claude-sonnet-5-5';\n  }\n  if (provider !== 'codex') {`,
      `      normalizedModel === 'claude-fable-5-1[1m]' ||\n      normalizedModel === 'claude-sonnet-5-5';\n  }\n  if (provider === 'gemini') {\n    const dyn = getDynamicGeminiEfforts(normalizedModel);\n    if (dyn) return dyn.includes('xhigh');\n    return normalizedModel.startsWith('gemini-4-hydrogen');\n  }\n  if (provider !== 'codex') {`,
      'model-catalog.js supportsXHighReasoningForModel'
    );
    raw = AssertReplace(
      raw,
      `  const normalizedModel = normalizeSelectedModel(model);\n  if (normalizedModel === 'ropilot-blaze') {`,
      `  const normalizedModel = normalizeSelectedModel(model);\n  const dyn = typeof getDynamicGeminiEfforts === 'function' ? getDynamicGeminiEfforts(normalizedModel) : null;\n  if (dyn) return dyn;\n  if (!supportsReasoningEffortForModel(normalizedModel)) return [];\n  if (normalizedModel === 'ropilot-blaze') {`,
      'model-catalog.js getSupportedReasoningLevelsForModel'
    );
    raw = AssertReplace(
      raw,
      `  const normalizedModel = normalizeSelectedModel(model);\n  if (normalizedModel === 'gpt-6-astra'`,
      `  const normalizedModel = normalizeSelectedModel(model);\n  if (!supportsReasoningEffortForModel(normalizedModel)) return '';\n  if (normalizedModel === 'gpt-6-astra'`,
      'model-catalog.js getDefaultReasoningLevelForModel'
    );
  }

  if (!raw.includes('export function syncAgyModels')) {
    const dynamicBlock = `
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
    MODEL_CATALOG.push({ value: entry.value, label: entry.label, provider: 'gemini', supportsFastMode: false });
  }
  DEFAULT_MODEL_BY_PROVIDER.gemini = parsed[0].value;
  return true;
}

export function getDynamicGeminiEfforts(modelValue) {
  return dynamicGeminiEfforts.get(modelValue) || null;
}
`;
    raw = AssertReplace(raw, `export const COMPOSER_MODEL_PROVIDER_ORDER =`, `${dynamicBlock}\nexport const COMPOSER_MODEL_PROVIDER_ORDER =`, 'model-catalog.js syncAgyModels');
  }

  WriteFile(filePath, raw);
}

function PatchMessageSender(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'message-sender.js');
  let raw = ReadFile(filePath);
  if (raw.includes('setAgyRoute')) return;

  raw = AssertReplace(
    raw,
    `    // Check for API key (either global or folder-specific)\n    const effectiveKey = this.app.getEffectiveApiKey();`,
    `    const isGeminiDirect = this.app.normalizeModelProvider(this.app.modelProvider) === 'gemini' && this.app.isUseRopilotCreditsEnabled?.() !== true;\n    if (!isGeminiDirect) {\n    // Check for API key (either global or folder-specific)\n    const effectiveKey = this.app.getEffectiveApiKey();`,
    'message-sender.js isGeminiDirect open'
  );
  raw = AssertReplace(
    raw,
    `    if (!isApiKeyValidated()) {\n      this.app.showApiKeyReminder('Your API key is invalid. Please double check.');\n      return;\n    }`,
    `    if (!isApiKeyValidated()) {\n      this.app.showApiKeyReminder('Your API key is invalid. Please double check.');\n      return;\n    }\n    }`,
    'message-sender.js isGeminiDirect close'
  );
  raw = AssertReplace(
    raw,
    `        provider: options.executionProvider,\n        model: options.executionModel,`,
    `        provider: options.executionProvider === 'gemini' ? 'claude' : options.executionProvider,\n        model: options.executionProvider === 'gemini' ? 'claude-sonnet-5-5' : options.executionModel,`,
    'message-sender.js steer wire model'
  );
  raw = AssertReplace(
    raw,
    `      this.app.ws.send(JSON.stringify(msg));\n      this.warmRopilotHarnessForManagedTurn(useRopilotCredits);`,
    `      if (options.executionProvider === 'gemini') electronAPI().setAgyRoute?.({ chatId: targetChat.id, provider: options.executionProvider, model: options.executionModel, effort: msg.reasoning_level || '' });\n      this.app.ws.send(JSON.stringify(msg));\n      this.warmRopilotHarnessForManagedTurn(useRopilotCredits);`,
    'message-sender.js steer setAgyRoute'
  );
  raw = AssertReplace(
    raw,
    `      this.app.messageHandler?.cancelClaudeMissingSessionRetry?.(targetChat.id);\n      this.app.ws.send(JSON.stringify(msg));`,
    `      this.app.messageHandler?.cancelClaudeMissingSessionRetry?.(targetChat.id);\n      if (safeProvider === 'gemini') electronAPI().setAgyRoute?.({ chatId: targetChat.id, provider: safeProvider, model: safeModel, effort: msg.reasoning_level || '' });\n      this.app.ws.send(JSON.stringify(msg));`,
    'message-sender.js main setAgyRoute'
  );
  raw = AssertReplace(
    raw,
    `    const claudeSessionId = safeProvider === 'claude' && providerMatchesHistory\n      ? (targetChat.claudeSessionId || undefined)\n      : undefined;`,
    `    const usesClaudeWire = safeProvider === 'claude' || safeProvider === 'gemini';\n    const wireProvider = safeProvider === 'gemini' ? 'claude' : safeProvider;\n    const wireModel = safeProvider === 'gemini' ? 'claude-sonnet-5-5' : safeModel;\n    const claudeSessionId = usesClaudeWire && providerMatchesHistory\n      ? (targetChat.claudeSessionId || undefined)\n      : undefined;`,
    'message-sender.js usesClaudeWire'
  );
  raw = AssertReplace(
    raw,
    `    const hasHistory = safeProvider === 'claude'\n      ? (!!claudeSessionId && !!targetChat.hasHistory)`,
    `    const hasHistory = usesClaudeWire\n      ? (!!claudeSessionId && !!targetChat.hasHistory)`,
    'message-sender.js hasHistory'
  );
  raw = AssertReplace(
    raw,
    `      provider: safeProvider,\n      api_key: apiKey,\n      project_dir: projectDir || undefined,\n      model: safeModel,`,
    `      provider: wireProvider,\n      api_key: apiKey,\n      project_dir: projectDir || undefined,\n      model: wireModel,`,
    'message-sender.js wire payload'
  );
  raw = AssertReplace(
    raw,
    `      turn_checkpoints: safeProvider === 'claude' && providerMatchesHistory ? (targetChat.turnCheckpoints || undefined) : undefined,`,
    `      turn_checkpoints: usesClaudeWire && providerMatchesHistory ? (targetChat.turnCheckpoints || undefined) : undefined,`,
    'message-sender.js turn_checkpoints'
  );
  raw = AssertReplace(
    raw,
    `      if (safeProvider === 'claude' || safeProvider === 'codex') {`,
    `      if (safeProvider === 'claude' || safeProvider === 'codex' || safeProvider === 'gemini') {`,
    'message-sender.js pendingRewind'
  );
  WriteFile(filePath, raw);
}

function PatchMessageHandler(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'message-handler.js');
  let raw = ReadFile(filePath);
  const deadBlock = /\n\s*if \(command\.startsWith\('__ROPILOT_THINKING__:'\)\) \{[\s\S]*?\n\s*return;\n\s*\}\n/;
  if (deadBlock.test(raw)) {
    raw = raw.replace(deadBlock, '\n');
  }
  if (!raw.includes('invalidateAndRefreshAgyUsage')) {
    raw = AssertReplace(
      raw,
      `import { rememberBlenderImportProgress } from './blender-import-progress.js';`,
      `import { rememberBlenderImportProgress } from './blender-import-progress.js';\nimport { invalidateAndRefreshAgyUsage } from './ui/agy-usage.js';`,
      'message-handler.js import agy-usage'
    );
    raw = AssertReplace(
      raw,
      `    void this.app.settings.refreshActions().catch((err) => {\n      console.warn('Post-turn balance refresh failed:', err?.message || err);\n    });\n    return;`,
      `    void this.app.settings.refreshActions().catch((err) => {\n      console.warn('Post-turn balance refresh failed:', err?.message || err);\n    });\n    invalidateAndRefreshAgyUsage(this.app);\n    return;`,
      'message-handler.js refresh 1'
    );
  }
  WriteFile(filePath, raw);
}

function PatchComposerDropdowns(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'composer', 'composer-dropdowns.js');
  let raw = ReadFile(filePath);
  if (raw.includes("gemini: []")) return;

  raw = AssertReplace(
    raw,
    `    const providerOptions = { ropilot: [], codex: [], claude: [] };\n    const providerAccess = {\n      ropilot: { locked: false },\n      codex: { locked: false },\n      claude: {\n        locked: this.shouldLockAnthropicManagedCreditProvider(),\n        reason: 'Upgrade to Plus to unlock Anthropic models.'\n      }\n    };\n    const useRopilotCredits = this.app.isUseRopilotCreditsEnabled?.() === true;`,
    `    const useRopilotCredits = this.app.isUseRopilotCreditsEnabled?.() === true;\n    const providerOptions = { ropilot: [], codex: [], claude: [], gemini: [] };\n    const providerAccess = {\n      ropilot: { locked: false },\n      codex: { locked: false },\n      gemini: { locked: useRopilotCredits },\n      claude: {\n        locked: this.shouldLockAnthropicManagedCreditProvider(),\n        reason: 'Upgrade to Plus to unlock Anthropic models.'\n      }\n    };`,
    'composer-dropdowns.js providerOptions'
  );
  raw = AssertReplace(
    raw,
    `      if (provider === 'ropilot' && !useRopilotCredits) continue;`,
    `      if (provider === 'ropilot' && !useRopilotCredits) continue;\n      if (provider === 'gemini' && useRopilotCredits) continue;`,
    'composer-dropdowns.js filter gemini'
  );
  raw = AssertReplace(
    raw,
    `    if (normalizedProvider === 'codex') return 'OpenAI';\n    return 'Anthropic';`,
    `    if (normalizedProvider === 'codex') return 'OpenAI';\n    if (normalizedProvider === 'gemini') return 'Gemini (AGY)';\n    return 'Anthropic';`,
    'composer-dropdowns.js getProviderDisplayName'
  );
  WriteFile(filePath, raw);
}

function PatchComposerFooter(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'composer', 'composer-footer.js');
  let raw = ReadFile(filePath);
  if (raw.includes('syncAgyUsageIndicator')) return;

  raw = AssertReplace(
    raw,
    `import { getProviderMarkSvg } from '../ui/provider-marks.js';`,
    `import { getProviderMarkSvg } from '../ui/provider-marks.js';\nimport { syncAgyUsageIndicator } from '../ui/agy-usage.js';`,
    'composer-footer.js import syncAgyUsageIndicator'
  );
  raw = AssertReplace(
    raw,
    `    let codexAuthenticated = false;\n    let hasCodexAccountPlan = false;`,
    `    let codexAuthenticated = false;\n    let hasCodexAccountPlan = false;\n    let agyAuthenticated = false;\n    try {\n      const st = await electronAPI().getAgyStatus?.();\n      agyAuthenticated = st ? !!st.installed : true;\n    } catch {}`,
    'composer-footer.js getAgyStatus'
  );
  raw = AssertReplace(
    raw,
    `    return {\n      claude: claudeAuthenticated,\n      codex: codexAuthenticated\n    };`,
    `    return {\n      claude: claudeAuthenticated,\n      codex: codexAuthenticated,\n      gemini: agyAuthenticated\n    };`,
    'composer-footer.js return auth map'
  );
  raw = AssertReplace(
    raw,
    `    const level = this.app.getReasoningLevelForModel(selectedModel, { useRopilotCredits });`,
    `    const level = supportedReasoningLevels.length > 0 ? this.app.getReasoningLevelForModel(selectedModel, { useRopilotCredits }) : '';`,
    'composer-footer.js reasoning level guard'
  );
  raw = AssertReplace(
    raw,
    `    } else if (provider === 'ropilot') {\n      reasoningSelect.title = \`Controls \${modelConfig?.label || 'Ropilot'} reasoning effort.\`;\n    } else {`,
    `    } else if (provider === 'ropilot') {\n      reasoningSelect.title = \`Controls \${modelConfig?.label || 'Ropilot'} reasoning effort.\`;\n    } else if (provider === 'gemini') {\n      reasoningSelect.title = \`Controls \${modelConfig?.label || 'Gemini (AGY)'} reasoning effort.\`;\n    } else {`,
    'composer-footer.js reasoningSelect title'
  );
  raw = AssertReplace(
    raw,
    `    const key = normalizedProvider === 'ropilot' || normalizedProvider === 'codex' ? normalizedProvider : 'claude';`,
    `    const key = normalizedProvider === 'ropilot' || normalizedProvider === 'codex' || normalizedProvider === 'gemini' ? normalizedProvider : 'claude';`,
    'composer-footer.js provider icon'
  );
  raw = AssertReplace(
    raw,
    `    return displayLabels[normalizedModel] || modelValue || 'Model';`,
    `    return displayLabels[normalizedModel] || modelCatalog.getModelConfig(normalizedModel)?.label || modelValue || 'Model';`,
    'composer-footer.js model display label'
  );
  raw = AssertReplace(
    raw,
    `  syncComposerFooterControls() {\n    const modelSelect = document.getElementById('model-select');`,
    `  syncComposerFooterControls() {\n    syncAgyUsageIndicator(this.app);\n    const modelSelect = document.getElementById('model-select');`,
    'composer-footer.js syncAgyUsageIndicator'
  );
  WriteFile(filePath, raw);
}

function PatchProviderMarks(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'ui', 'provider-marks.js');
  let raw = ReadFile(filePath);
  if (raw.includes('GEMINI_MARK')) return;

  const geminiMark = `const GEMINI_MARK = '<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 2L14.8 9.2L22 12L14.8 14.8L12 22L9.2 14.8L2 12L9.2 9.2L12 2Z"/></svg>';\n\n`;
  raw = AssertReplace(
    raw,
    `export function getProviderMarkSvg(provider) {`,
    `${geminiMark}export function getProviderMarkSvg(provider) {`,
    'provider-marks.js GEMINI_MARK'
  );
  raw = AssertReplace(
    raw,
    `  if (p === 'ropilot') return ROPILOT_MARK;`,
    `  if (p === 'ropilot') return ROPILOT_MARK;\n  if (p === 'gemini' || p === 'agy' || p === 'antigravity') return GEMINI_MARK;`,
    'provider-marks.js gemini case'
  );
  WriteFile(filePath, raw);
}

function WriteAgyUsageJs(rootDir) {
  const filePath = path.join(rootDir, 'src', 'js', 'ui', 'agy-usage.js');
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  WriteFile(filePath, AGY_USAGE_SOURCE);
}

function VerifySyntax(rootDir) {
  const files = [
    'main.min.js',
    'preload.js',
    path.join('lib', 'claude-manager.js'),
    path.join('src', 'app.js'),
    path.join('src', 'js', 'models', 'model-catalog.js'),
    path.join('src', 'js', 'message-sender.js'),
    path.join('src', 'js', 'message-handler.js'),
    path.join('src', 'js', 'composer', 'composer-dropdowns.js'),
    path.join('src', 'js', 'composer', 'composer-footer.js'),
    path.join('src', 'js', 'ui', 'provider-marks.js'),
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

async function Run() {
  const liveAsar = ResolveAsarPath();
  if (!fs.existsSync(liveAsar)) {
    throw new Error(`Ropilot Studio app.asar not found at: ${liveAsar}`);
  }

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ropilot-gemini-asar-'));
  try {
    const extractDir = path.join(tmpDir, 'app');
    asar.extractAll(liveAsar, extractDir);

    const isStock = !ReadFile(path.join(extractDir, 'main.min.js')).includes('get-agy-status');
    const backupAsar = `${liveAsar}.bak`;
    const metaPath = `${liveAsar}.bak.meta.json`;
    if (isStock || !fs.existsSync(backupAsar)) {
      fs.copyFileSync(liveAsar, backupAsar);
      fs.writeFileSync(metaPath, JSON.stringify({ sha256: Sha256File(liveAsar), savedAt: new Date().toISOString() }, null, 2));
    }

    PatchClaudeManager(extractDir);
    PatchMainJs(extractDir);
    PatchPreloadJs(extractDir);
    PatchAppJs(extractDir);
    PatchIndexHtml(extractDir);
    PatchStylesCss(extractDir);
    PatchModelCatalog(extractDir);
    PatchMessageSender(extractDir);
    PatchMessageHandler(extractDir);
    PatchComposerDropdowns(extractDir);
    PatchComposerFooter(extractDir);
    PatchProviderMarks(extractDir);
    WriteAgyUsageJs(extractDir);
    VerifySyntax(extractDir);

    const stagedAsar = path.join(path.dirname(liveAsar), `app.asar.tmp.${process.pid}`);
    await asar.createPackage(extractDir, stagedAsar);
    fs.renameSync(stagedAsar, liveAsar);
    console.log('RopilotGemini app.asar patched cleanly.');
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

Run().catch((err) => {
  console.error(err?.stack || String(err));
  process.exit(1);
});