# PPT 引擎更新与前端集成结果

执行时间：2026-10-23 晚至 2026-10-24 凌晨（北京时间）。

## 完成结果

- 用户指定的 `D:/enterpriseProject/pptx-editor-engine` 已从 `0be575a` 快进到远端 main 的 `76fdb64c6d9b4d0830a4fe80a711d996d4d6d41f`。
- 按原锁文件执行 npm ci，安装 459 个依赖；未修改 package.json 或锁文件。原有两张未跟踪 PNG 保留，仓库没有新增跟踪文件修改。
- 浏览器 WASM、Node WASM、引擎独立生产包均构建成功。
- AI PPT 前端的类型检查、生产构建通过，构建 hash 为 `8961751e1fce351a`。存在 CSS 顺序与包体积警告。
- 前端及 Node 渲染服务已切换到上述指定仓库并重启。前端 16011、渲染服务 8787；后端 18897、Agent 13001 保持运行。

前端延续项目现有的源码别名集成方式，通过 `PPTX_ENGINE_DIR` 使用引擎源码及生成的 WASM；没有将引擎演示用 dist 冒充 npm 库安装。生产构建模块清单已确认包含指定仓库的代码及 WASM。生产包位于 `target/ppt-local-backend-20261023/frontend-build`，引擎独立包位于 `D:/enterpriseProject/pptx-editor-engine/dist`。

## 构建方式与限制

构建使用工作区内的便携 Node v22.23.4，未修改系统 Node。路径保存在 `target/ppt-local-backend-20261023/build-runtime.json`。

首次 wasm-pack 的可选 wasm-opt 优化阶段崩溃（Precompute.cpp，退出码 0xc0000409）。随后仅跳过该额外阶段，保留 Rust release 构建：

```text
wasm-pack build rust-engine --target bundler --out-dir pkg --no-opt
wasm-pack build rust-engine --target nodejs --out-dir pkg-node --no-opt
node node_modules/@rspack/cli/bin/rspack build
```

前端构建脚本为 `target/ppt-local-backend-20261023/build-ai-ppt.cjs`。临时类型配置使用 ES2022 lib 与两个仓库的类型目录；未修改前端业务源码或其原始 tsconfig。

## 当前故障判断

**更新引擎没有消除字体 503，真实生成仍保持停止。**

引擎最新提交修复 Rspack 对 wasm-bindgen 导入的保留问题；而当前错误来自 Go 后端的字体目录接口：

```text
GET http://127.0.0.1:18897/prod-api/aiPpt/fonts/api/fonts
HTTP 503
fontconfig is unavailable; install fontconfig and fonts-noto-cjk,
fonts-crosextra-carlito, fonts-crosextra-caladea, fonts-liberation
```

后端 `service/ppt_runtime/fonts.go` 调用外部 `fc-list` / `fc-match`。本地进程环境未提供可用命令。最新引擎的 Node 路径 `src/headless/fonts.ts:62` 同样依赖 `fc-match`。

选取 page-tool-link、page-autosave、design-service、design-diff、api-schema 五个测试文件运行：**19 通过、10 失败**。后两类中的 10 个失败均出现 `spawnSync fc-match ENOENT`。发现后未重复运行这些测试，也未继续生成。渲染服务健康检查返回 200，仅证明服务可访问，不代表实际字体渲染通过。

下一步需要为后端与 Node 渲染进程配置可用的 fontconfig、字体文件及 PATH，再验证字体目录和实际渲染。本次未自动安装或修复该环境。

## 保留的业务状态与证据

项目 808、会话 `task_8f4abfe4d6a870baca6c3c3b62f1d8e3` 保留；5 页《Agent 设计要点》、中国联通 UniAI 模板的大纲版本 112 仍未确认。本次没有恢复模型执行，没有制作新正文页或导出。

- `target/ppt-local-backend-20261023/evidence/engine-integration-build.json`：前端构建结果及实际引擎模块路径。
- `target/ppt-local-backend-20261023/evidence/engine-update-verification.json`：WASM 哈希、字体 503 原始响应和渲染服务健康状态。
- `target/ppt-local-backend-20261023/evidence/engine-update-font-503.png`：切换更新后的浏览器页面。
- `target/ppt-local-backend-20261023/runtime.json`：进程及引擎路径记录。
- `target/ppt-local-backend-20261023/frontend-engine-update.stdout.log`、`render-engine-update.stdout.log`：更新后的服务日志。

原始真实测试过程见 [UniAI 五页真实测试记录](ppt-uniai-five-page-real-test-2026-10-23.md)。

## 后续修复（2026-10-24 00:25）

Windows 字体兼容已补齐，字体接口与编辑器加载恢复。实际所需字体已安装，先前缺的是原生发现和匹配实现；无需在 Windows 上补装 fontconfig。详见 [Windows 字体兼容修复](ppt-windows-font-compatibility-2026-10-24.md)。
