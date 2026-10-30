# Windows 字体兼容修复

## 结果

已修复并启动本地后端和 Node 渲染服务。字体目录接口从 503 恢复为 200，浏览器编辑器和中国联通 UniAI 模板的四张初始页面缩略图正常加载。未恢复真实 PPT 生成，未修改项目 809 的页面内容。

此前把问题描述为“缺字体”不够准确：本机 `C:/Windows/Fonts` 已安装 Noto Sans SC、Noto Serif SC、Carlito、Caladea、Liberation 等所需开源字体。实际问题是 Go 和 Node 代码只调用 fontconfig，未实现 Windows 字体目录发现。

## 修改

- 后端 `target/ppt-real-e2e-20261022/ai-ppt-server/service/ppt_runtime/fonts_native.go`：Windows 直接解析系统及用户字体目录中的 TTF/OTF/TTC 元数据，以字体内部家族名识别字体，匹配字重和斜体。继续限定原有开源字体白名单，不向浏览器提供 Office 字体。
- 后端 `fonts.go`：Windows 自动使用原生实现；Linux 未配置额外目录时保留 fontconfig 路径。
- 引擎 `D:/enterpriseProject/pptx-editor-engine/src/headless/native-fonts.ts`、`fonts.ts`：使用相同的开源字体替代规则，从 Windows 原生字体目录选择文件，不调用 `fc-match`。
- 两边均支持 `PPT_FONT_DIRS` 指定共享字体目录（Windows 用分号分隔）。Windows 默认扫描 `%WINDIR%/Fonts` 和 `%LOCALAPPDATA%/Microsoft/Windows/Fonts`，当前机器无需额外配置。
- 新增 Go/TypeScript 字体解析与匹配测试、两个项目内的 Windows 字体部署说明。没有更改系统 PATH 或安装新字体。

## 验证

- Go：3 个新增字体用例和原有开源字体服务用例通过；本地后端编译成功。
- 引擎：TypeScript 检查通过；4 个新增字体用例通过，之前因 `fc-match ENOENT` 失败的 design-service / design-diff 共 10 个用例全部通过。
- 字体目录接口 HTTP 200，识别 Caladea、Carlito、DejaVu Sans、Liberation Mono、Liberation Sans、Liberation Serif、Noto Sans、Noto Sans SC、Noto Serif SC 共 9 个家族。
- Noto Sans SC、Carlito 字体文件接口 HTTP 200；渲染服务健康状态 200。
- 重新打开项目 809 后编辑区和四张模板缩略图正常显示，无字体 503。大纲版本 102 保留；任务此前已取消，本次未恢复它。

这些验证限于字体兼容、已有相关单元用例和编辑器加载，不是新一轮真实模型生成测试。字体加载失败向 Agent 的错误传播和 Agent 后续处理策略不在本次修改范围内。

## 启动与证据

- 后端使用新编译的 `target/ppt-real-e2e-20261022/ai-ppt-server-windows-fonts.exe`，端口 18897；Node 渲染服务 8787。现有前端 16011、Agent 13001 保持运行。
- 后端 PID 35320，渲染 PID 35904；最新记录见 `target/ppt-local-backend-20261023/runtime.json`。
- 可复用启动脚本：`target/ppt-local-backend-20261023/start-font-compatible-services.ps1`。端口已占用时拒绝重复启动，不自动终止未知进程。
- 接口证据：`target/ppt-local-backend-20261023/evidence/windows-native-fonts.json`。
- 页面证据：`target/ppt-local-backend-20261023/evidence/windows-fonts-editor-loaded.png`。
- 服务日志：`backend-fonts-20261024-002343.*.log`、`render-fonts-20261024-002343.*.log`。

修改尚未提交或推送。引擎原有的两张未跟踪 PNG 保留。
