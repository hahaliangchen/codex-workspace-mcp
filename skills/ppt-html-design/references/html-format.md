# HTML 设计视图格式

本文件是项目适配规范，不改动另存的 Anthropic 审美原文。以宿主明确提供的能力清单为准；不要把浏览器支持的全部 CSS 当作可导出的能力。

## 画布与正文

- 一页一个静态 HTML 文档，UTF-8，`html, body` 为 `1280px × 720px`，`margin: 0`，全局 `box-sizing: border-box`，body 为定位原点。
- 修改现有页时保留 `body[data-page-id]`、`data-design-revision` 和 `data-source-hash`。新增页先由宿主分配页面身份，不猜测现有页 ID。
- flex/grid 或绝对定位均可。浏览器负责求出位置与换行，程序冻结测量结果；导出后不再自动调整构图。
- 文本节点内使用 p、h1–h6 等块元素，行内用 span/strong/em/br。布局容器不能夹带裸文本。文本字号和 line-height 明确写成 px；段落默认 margin 归零，再显式设定。
- 文字保持可编辑。不得将整页或大段正文截成图片；复杂视觉元素可以独立素材化。
- 不执行脚本、不导入网页框架、在线样式或远端字体。字体从宿主确认的可用字体选择，中英文字体与测量、编辑器保持一致。

## 身份、组合与模板语义

每个可绘制对象标记 `data-kind="text|shape|image|asset|group"`，已有对象原样保留 `data-id`。新节点省略 ID，测量程序分配新身份。不要自行修改现有节点的 kind。

只有 `data-kind="group"` 可以嵌套其他对象；组在 DOM 中嵌套、在 JSON 中记录 parentId。纯布局容器可以不带身份，但不能有未标记的可见背景、边框或文字。组本身不绘制背景/边框，要用组内独立 shape 表达。

图片和复杂素材必须直接使用 `<img data-kind="image|asset" src="宿主返回的资源引用">`，尺寸、位置写在这个 img 上。不要把 `data-kind="image"` 标在外层 div 上再嵌入 img；需要容器时用未标记的纯布局容器，或明确的 group。新增 SVG 图标先由资产工具生成可靠的显示资源，再把工具返回的 src 放到 img，例如：

```html
<img data-kind="image" src="project:images/TOOL_RETURNED_FILENAME.png" alt="交付闭环示意" style="position:absolute;left:100px;top:180px;width:120px;height:120px">
```

这是元素格式示例，文件名必须来自实际工具结果；不能照抄占位文件名。遇到 `Image assets must be img elements`，修正该节点的标签/容器标记后重新提交，不需要重建项目或重新生成已经成功的素材。

`data-role`、`data-bind`、`data-locked`、`data-required`、`data-max-chars` 等信息来自 JSON。它们帮助模型理解语义，不是授权来源。删除 HTML 的锁定属性不能解锁 JSON。已有元素缺失表示请求删除，必填或锁定对象不可删除。 生成模式中逐字保留已有节点的全部 `data-*` 属性，包括 `data-name`、`data-purpose`、`data-note` 以及空值属性；不要重新简写原节点。固定对象应完整复制其原始 HTML，只在允许的内容节点或新增节点上设计。模板制作的已授权语义变更是明确例外。

HTML 表达已支持的可见外观；模板制作模式还可提交 data-purpose、data-note 及 body 的页面用途属性，具体规则见 [模板用途与保护](template-authoring.md)。普通生成模式不能改变这些权限。原始 OOXML、动画、占位符来源和已存在于 PPTX 的模板标注继续保留在内存 JSON/源包中。宿主管理的模板用途及保护规则随 PPTX 保存，其他临时 JSON 业务字段不承诺跨页面重载保留；不为本流程新增设计 JSON 存储。不要为了简化 HTML 而丢弃这些信息。

## 基础转换能力

当前转换器接受以下范围；部署环境以实际接口说明为准：

- 纯色背景、统一宽度颜色的实线边框、四角一致的圆角。
- 字体、字号、颜色、粗斜体、字距、明确行高与段距、左中右对齐、文本内边距及垂直对齐。
- PNG/JPEG/GIF 图片，明确宽高。模型使用宿主返回的 design-asset: 或 project:images/... 引用，宿主解析为嵌入资源；需要裁剪时先生成正确比例的素材，不拉伸照片。
- 简单组及子节点，DOM 顺序为绘制顺序；不使用 z-index。
- 原始 SVG 可以作为资产保留，但当前新增图片导出使用可靠位图显示资产；不承诺任意 SVG 路径可编辑。

当前原型未支持 CSS 渐变、阴影、transform、filter、backdrop-filter、混合模式、伪元素内容、整体 opacity、复杂裁剪与分边框。需要这些效果时，将相应视觉元素作为独立素材实现，或使用宿主已声明支持的扩展。不能删掉效果却宣称等价还原。

列表标记、下划线、原生图表及自定义组件必须有宿主的明确支持。没有支持时用有身份的文本、图形和素材组合表达，不依赖浏览器自动生成的列表圆点或杜撰组件标签。

## 最小结构示例

下面仅示意身份和 HTML 结构，不是强制版式、配色或字号。page-id 与 revision 必须取自当前宿主，中文字体必须已确认存在。

```html
<!doctype html>
<html><head><meta charset="utf-8"><style>
* { box-sizing: border-box; }
html, body { margin: 0; width: 1280px; height: 720px; }
body { position: relative; background: white; }
p { margin: 0; flex-shrink: 0; }
</style></head>
<body data-page-id="HOST_PAGE_ID" data-design-revision="0" data-source-hash="HOST_SOURCE_SHA256">
  <div data-kind="group" style="position:absolute;left:80px;top:80px;width:1120px;height:560px;display:flex;flex-direction:column;gap:28px">
    <div data-kind="text" style="width:1120px;height:72px">
      <h1 style="margin:0;font-family:'Noto Sans CJK SC';font-size:44px;line-height:60px;color:#183d32">根据内容设计标题</h1>
    </div>
    <div data-kind="text" style="width:760px;height:200px">
      <p style="font-family:'Noto Sans CJK SC';font-size:26px;line-height:40px;color:#334155">正文保持可编辑，示例中的空白区域应由实际内容与视觉构图决定。</p>
    </div>
  </div>
</body></html>
```

## 交付验证

用宿主的测量与预览能力检查真实断行、文本边界、字体、资产加载和不支持样式。未通过时修订 HTML，再次验证。不要通过 overflow:hidden、缩小所有字体或删除内容来掩盖溢出。

检查 HTML、编辑器与导出重开后的效果是否一致；没有运行某一环节时明确注明。先解决缺内容、缺图标、遮挡和文字残缺，再判断层级、节奏、配色与构图。每页都要有实际检查记录，不能只检查首尾页。

## 统一编辑与差异合并

AI 改文字、填模板、调样式、重新设计都提交 HTML。先 get_design 读取当前页，再改对应 data-id 节点并 apply_html；不调用 apply_edits 或 build_deck。小改保持其余节点结构与样式，不能为改一个字重写整页。宿主持有原始 HTML，只把两份 HTML 中实际变化的属性写回 JSON，并检查受影响节点的溢出。flex/grid 联动会测量相关节点；未变节点不以浏览器测量值覆盖。用户手动编辑完成后自动刷新投影，必须使用新版本。apply_html 返回变化节点与字段摘要；无变化的提交不重建 PPTX。manage_design_pages 负责页数、排序与复制参考页；duplicateSlide 继承品牌素材和保护规则。

若提交因只读属性、版本或固定区域校验被拒绝，原文稿没有被本次提交覆盖。先依据具体错误重读当前 HTML，恢复受保护属性或移开新增内容，再继续已授权设计；不得把一次可修正的拒绝当成生成完成，也不得通过重新初始化消除错误。
