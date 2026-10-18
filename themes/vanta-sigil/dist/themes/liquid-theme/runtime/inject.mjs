import a from"electron";import d from"./methods/index.mjs";const n=global.lynx_liquid_plugin;n?a.app.on("browser-window-created",(t,e)=>{const o=d(e),l=n.config?.preventFlash?"overwrite":"interval",s=o[l];e.on("closed",()=>s.uninstall()),e.webContents.on("dom-ready",()=>{const i=e.webContents.getURL();if(i.includes("workbench.html")||i.includes("workbench.esm.html"))if(n.os==="macos"&&n.vibrancyType){e.setBackgroundColor("#00000000"),e.setVibrancy(n.vibrancyType);const r=e.getBounds();e.setBounds({width:r.width+1}),e.setBounds({width:r.width}),s.install(),c(e)}else e.setBackgroundColor("#0c1118"),c(e).then(()=>{e.setBackgroundColor("#00000000"),s.install()})})}):console.error("[Lynx Liquid Runtime] Extension config not found.");function u(){return`<style id="lynx-liquid-theme-css">${n?.themeCSS??""}</style>`}function c(t){const e=u();return t.webContents.executeJavaScript(`
    (function() {
      try {
        const ttp = window.trustedTypes
          ? window.trustedTypes.createPolicy("LynxLiquidTheme", { createHTML: (v) => v })
          : null;
        const existing = document.getElementById('lynx-liquid-style-root');
        if (existing) existing.remove();
        const container = document.createElement('div');
        container.id = 'lynx-liquid-style-root';
        container.innerHTML = ttp ? ttp.createHTML(${JSON.stringify(e)}) : ${JSON.stringify(e)};
        document.body.appendChild(container);
      } catch (e) { console.error(e); }
    })();
  `).then(()=>{}).catch(o=>console.error(o))}
