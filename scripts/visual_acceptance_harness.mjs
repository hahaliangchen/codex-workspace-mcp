// An isolated harness importing the existing PPT editor component, not a replacement editor.
// Generated output stays in the workspace artifact directory; external sources are read only.
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {build} from '../frontend/node_modules/esbuild/lib/main.js';
const root=path.resolve(path.dirname(fileURLToPath(import.meta.url)),'..');
const editor=path.resolve(process.env.PPT_EDITOR_ROOT||path.join(root,'../ai-ppt-view'));
const output=path.join(root,'.codex-workspace-mcp','visual-acceptance-editor');
fs.mkdirSync(output,{recursive:true});
const component=path.join(editor,'src/components/SlideEditor/SlideEditor.tsx').replaceAll('\\','/');
const svg='<svg xmlns="http://www.w3.org/2000/svg" width="960" height="540" viewBox="0 0 960 540"><rect x="0" y="0" width="960" height="540" fill="#eef3fa"/><text x="70" y="100" font-size="40" fill="#102b50">Quarterly product review</text><text x="70" y="170" font-size="25" fill="#43546e">Keep the heading and the blue circle</text><circle cx="220" cy="340" r="80" fill="#2368d7"/><rect id="delete-target" x="590" y="250" width="220" height="170" rx="18" fill="#e0416f"/><text x="635" y="460" font-size="24" fill="#43546e">Remove the pink rectangle</text></svg>';
const entry=path.join(output,'entry.tsx');
fs.writeFileSync(entry,`import React from 'react';import {createRoot} from 'react-dom/client';import SlideEditor from ${JSON.stringify(component)};
function App(){return <><h1>PPT SVG editor acceptance</h1><p id="state">target_count=1; edits=0</p><SlideEditor svgContent={${JSON.stringify(svg)}} onChange={s=>{const d=new DOMParser().parseFromString(s,'image/svg+xml');const p=document.querySelector('#state');p.textContent='target_count='+d.querySelectorAll('#delete-target').length+'; edits=1';}}/></>};createRoot(document.querySelector('#root')).render(<App/>);`);
await build({entryPoints:[entry],outfile:path.join(output,'bundle.js'),bundle:true,format:'iife',platform:'browser',jsx:'automatic',alias:{react:path.join(editor,'node_modules/react'),'react-dom':path.join(editor,'node_modules/react-dom')},logLevel:'warning'});
fs.writeFileSync(path.join(output,'index.html'),'<!doctype html><meta charset="utf-8"><title>PPT node deletion acceptance</title><style>body{margin:24px;background:#172131;color:white;font:16px Arial}h1{font-size:22px}svg{display:block}</style><div id="root"></div><script src="bundle.js"></script>');
console.log(`Harness built: ${output}`);
