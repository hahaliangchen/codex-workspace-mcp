// Rebuild the existing PPT demo into this repository's ignored artifact directory.
// Its source and generated WASM package are read only.
const path = require('node:path');
const fs = require('node:fs');
const root = path.resolve(__dirname, '..');
const project = path.resolve(process.env.PPT_PROJECT_ROOT || path.join(root, '../pptx-editor-engine'));
const output = path.resolve(root, '.codex-workspace-mcp', 'visual-input-ppt-dist');
if (!output.startsWith(root + path.sep)) throw new Error('PPT output must remain in this workspace');
const { rspack } = require(path.join(project, 'node_modules/@rspack/core'));
const config = require(path.join(project, 'rspack.config.js'));
const entry = path.join(root, '.codex-workspace-mcp', 'visual-input-ppt-entry.mjs');
fs.mkdirSync(path.dirname(entry), { recursive: true });
fs.writeFileSync(entry, `import * as glue from ${JSON.stringify(path.join(project, 'rust-engine/pkg/ppt_engine_bg.js').replaceAll('\\', '/'))};\nglobalThis.__pptAcceptanceWasmGlue = glue;\nimport ${JSON.stringify(path.join(project, 'src/demo-entry.ts').replaceAll('\\', '/'))};\n`);
const compiler = rspack({ ...config, context: project, mode: 'production',
  entry,
  output: { ...config.output, path: output },
  // wasm-bindgen's JS glue imports its async WASM module in a cycle. Keep all
  // glue exports available to WASM rather than pruning imports in this fixture.
  optimization: { ...config.optimization, usedExports: false, sideEffects: false, concatenateModules: false, minimize: false },
});
compiler.run((error, stats) => {
  compiler.close(closeError => {
    if (error || closeError || stats.hasErrors()) {
      console.error(error || closeError || stats.toString({ all: false, errors: true }));
      process.exitCode = 1;
    } else console.log(`PPT demo rebuilt: ${output}`);
  });
});
