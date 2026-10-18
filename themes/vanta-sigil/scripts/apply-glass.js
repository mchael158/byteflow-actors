const fs = require('fs');
const path = require('path');

const home = process.env.USERPROFILE;
const vantaExt = path.join(home, '.cursor/extensions/computador.vanta-sigil-1.0.0');
const repo = path.join(home, 'Desktop/Bytecode/themes/vanta-sigil');
const lynxExt = path.join(home, '.cursor/extensions/bastndev.lynx-theme-5.3.6-universal');
const appOut = path.join(process.env.LOCALAPPDATA, 'Programs/cursor/resources/app/out');

const oldV = 'async function V(t){t.globalState.get("lynxLiquidInstalled",!1)||await E(t)}';
const newV = 'async function V(t){let p=false;try{const {ElectronJSFile:s}=(0,o.resolveVSCodePaths)(C);p=(await f.promises.readFile(s,"utf-8")).includes("frame:false,transparent:true")}catch(e){}if(t.globalState.get("lynxLiquidInstalled",!1)&&p)return;await E(t)}';

function patchHandleActivation(file) {
  let s = fs.readFileSync(file, 'utf8');
  if (s.includes(oldV)) {
    fs.writeFileSync(file, s.replace(oldV, newV));
    console.log('patched V()', file);
  } else if (s.includes('frame:false,transparent:true')) {
    console.log('already patched', file);
  } else {
    console.log('V() pattern missing', file);
  }
}

for (const file of [
  path.join(repo, 'dist/themes/liquid-theme/platforms/windows.js'),
  path.join(vantaExt, 'dist/themes/liquid-theme/platforms/windows.js'),
]) {
  patchHandleActivation(file);
}

const lynxTheme = path.join(lynxExt, 'dist/themes/liquid-theme/liquid-theme.js');
let lynx = fs.readFileSync(lynxTheme, 'utf8');
const lynxOld = 'a()?i.handleActivation(e):i.handleDeactivation(e)';
const lynxNew = 'a()?i.handleActivation(e):void 0';
if (lynx.includes(lynxOld)) {
  fs.writeFileSync(lynxTheme, lynx.replace(lynxOld, lynxNew));
  console.log('Lynx deactivation disabled');
} else if (lynx.includes(lynxNew)) {
  console.log('Lynx already no-op');
} else {
  console.log('Lynx pattern missing');
}

const obsoletePath = path.join(home, '.cursor/extensions/.obsolete');
let obsolete = {};
if (fs.existsSync(obsoletePath)) {
  try { obsolete = JSON.parse(fs.readFileSync(obsoletePath, 'utf8')); } catch {}
}
obsolete['bastndev.lynx-theme-5.3.6-universal'] = true;
fs.writeFileSync(obsoletePath, JSON.stringify(obsolete));
console.log('wrote .obsolete');

const { generateNewJS, injectElectronOptions, patchCSP } = require(
  path.join(vantaExt, 'dist/themes/liquid-theme/utils/file-transforms')
);

const JSFile = path.join(appOut, 'main.js');
const HTMLFile = path.join(appOut, 'vs/code/electron-sandbox/workbench/workbench.html');
const runtimeDir = path.join(appOut, 'lynx-liquid-runtime-v2');
const runtimeSrc = path.join(vantaExt, 'dist/themes/liquid-theme/runtime');
const runtimeEntry = path.join(runtimeDir, 'inject.mjs');
const windowsJs = path.join(vantaExt, 'dist/themes/liquid-theme/platforms/windows.js');

function copyDir(src, dest) {
  fs.mkdirSync(dest, { recursive: true });
  for (const ent of fs.readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, ent.name);
    const d = path.join(dest, ent.name);
    if (ent.isDirectory()) copyDir(s, d);
    else fs.copyFileSync(s, d);
  }
}

if (fs.existsSync(runtimeDir)) fs.rmSync(runtimeDir, { recursive: true, force: true });
copyDir(runtimeSrc, runtimeDir);

const electronAfter = injectElectronOptions(fs.readFileSync(JSFile, 'utf8'));
fs.writeFileSync(JSFile, electronAfter);

const cssDir = path.join(vantaExt, 'dist/themes/liquid-theme/css');
const themeCSS = [
  fs.readFileSync(path.join(cssDir, 'global.css'), 'utf8'),
  fs.readFileSync(path.join(cssDir, 'liquid-glass/liquid-glass.css'), 'utf8'),
  fs.readFileSync(path.join(cssDir, 'animation/text.css'), 'utf8'),
].join('\n');

const mainJS = generateNewJS(
  fs.readFileSync(JSFile, 'utf8'),
  windowsJs,
  { os: 'windows', themeCSS, config: { refreshInterval: 1000, preventFlash: true } },
  runtimeEntry
);
fs.writeFileSync(JSFile, mainJS);

const html = fs.readFileSync(HTMLFile, 'utf8');
const { result: patchedHTML, noMetaTag } = patchCSP(html);
if (!noMetaTag) fs.writeFileSync(HTMLFile, patchedHTML);

const check = fs.readFileSync(JSFile, 'utf8');
console.log('transparent patched:', check.includes('frame:false,transparent:true'));
console.log('liquid marker:', check.includes('LYNX-LIQUID-START'));
console.log('runtime exists:', fs.existsSync(runtimeEntry));
console.log('html csp:', fs.readFileSync(HTMLFile, 'utf8').includes('LynxLiquidTheme'));
