"""Read an uploaded .pptx with the standard library only.

  read_pptx.py <file> text                 per-slide text, tables, chart data, notes, picture ids
  read_pptx.py <file> pictures <slide|id>  pictures of one slide (or one picture id) with their bytes

Pictures are the images placed on slides. Pictures that belong to layouts and
masters are the template's decoration, so they are not listed.
"""
import base64, json, posixpath, re, sys, zipfile
import xml.etree.ElementTree as ET

NS = {
    'a': 'http://schemas.openxmlformats.org/drawingml/2006/main',
    'p': 'http://schemas.openxmlformats.org/presentationml/2006/main',
    'r': 'http://schemas.openxmlformats.org/officeDocument/2006/relationships',
    'c': 'http://schemas.openxmlformats.org/drawingml/2006/chart',
    'rel': 'http://schemas.openxmlformats.org/package/2006/relationships',
}
R_EMBED = '{%s}embed' % NS['r']
R_ID = '{%s}id' % NS['r']
EMU_CM = 360000
MAX_ENTRY = 64 * 1024 * 1024
IMAGE_TYPES = {'png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'tif', 'tiff', 'svg', 'emf', 'wmf'}


def q(tag):
    prefix, name = tag.split(':')
    return '{%s}%s' % (NS[prefix], name)


class Deck:
    def __init__(self, path):
        self.z = zipfile.ZipFile(path)
        self.names = set(self.z.namelist())
        if 'ppt/presentation.xml' not in self.names:
            raise ValueError('不是有效的 PPTX：缺少 ppt/presentation.xml')

    def xml(self, name):
        info = self.z.getinfo(name)
        if info.file_size > MAX_ENTRY:
            raise ValueError('%s 解压后超过限制' % name)
        return ET.fromstring(self.z.read(name))

    def rels(self, part):
        folder, base = posixpath.split(part)
        name = posixpath.join(folder, '_rels', base + '.rels')
        out = {}
        if name in self.names:
            for rel in self.xml(name):
                target = rel.get('Target', '')
                if rel.get('TargetMode') == 'External':
                    out[rel.get('Id')] = (rel.get('Type', ''), None)
                    continue
                path = posixpath.normpath(target.lstrip('/')) if target.startswith('/') else posixpath.normpath(posixpath.join(folder, target))
                out[rel.get('Id')] = (rel.get('Type', ''), path)
        return out

    def slides(self):
        pres = self.xml('ppt/presentation.xml')
        rels = self.rels('ppt/presentation.xml')
        order = []
        for sld in pres.iter(q('p:sldId')):
            target = rels.get(sld.get(R_ID))
            if target and target[1] in self.names:
                order.append(target[1])
        size = pres.find('p:sldSz', NS)
        dims = (int(size.get('cx')) / EMU_CM, int(size.get('cy')) / EMU_CM) if size is not None else None
        return order, dims


def paragraphs(shape):
    out = []
    for para in shape.iter(q('a:p')):
        text = ''.join(t.text or '' for t in para.iter() if t.tag in (q('a:t'),) ) 
        if text.strip():
            level = int((para.find('a:pPr', NS).get('lvl') if para.find('a:pPr', NS) is not None else 0) or 0)
            bullet = para.find('a:pPr/a:buChar', NS) is not None or para.find('a:pPr/a:buAutoNum', NS) is not None
            out.append(('  ' * level + ('- ' if bullet else '') + text.strip()))
    return out


def rect_cm(shape):
    off = shape.find('.//a:xfrm/a:off', NS)
    ext = shape.find('.//a:xfrm/a:ext', NS)
    if off is None or ext is None:
        return None
    return [round(int(off.get('x')) / EMU_CM, 2), round(int(off.get('y')) / EMU_CM, 2),
            round(int(ext.get('cx')) / EMU_CM, 2), round(int(ext.get('cy')) / EMU_CM, 2)]


def table_markdown(tbl):
    rows = []
    for tr in tbl.findall('a:tr', NS):
        cells = [' '.join(paragraphs(tc)).replace('|', '/').replace('\n', ' ') for tc in tr.findall('a:tc', NS)]
        rows.append(cells)
    if not rows:
        return ''
    width = max(len(r) for r in rows)
    rows = [r + [''] * (width - len(r)) for r in rows]
    lines = ['| ' + ' | '.join(rows[0]) + ' |', '|' + '---|' * width]
    lines += ['| ' + ' | '.join(r) + ' |' for r in rows[1:]]
    return '\n'.join(lines)


def chart_text(deck, path):
    if path not in deck.names:
        return '（图表数据缺失）'
    root = deck.xml(path)
    plot = root.find('.//c:plotArea', NS)
    if plot is None:
        return '（图表数据缺失）'
    parts = []
    for kind in plot:
        name = kind.tag.split('}')[1]
        if not name.endswith('Chart'):
            continue
        sers = kind.findall('c:ser', NS)
        cats = []
        for ser in sers:
            cat = ser.find('c:cat', NS)
            if cat is not None:
                cats = [v.text or '' for v in cat.iter(q('c:v'))]
                break
        parts.append('%s；类别：%s' % (name[:-5], '、'.join(cats) if cats else '无'))
        for ser in sers:
            title = ser.find('c:tx', NS)
            label = ''.join(v.text or '' for v in title.iter(q('c:v'))) if title is not None else ''
            values = [v.text or '' for v in (ser.find('c:val', NS).iter(q('c:v')) if ser.find('c:val', NS) is not None else [])]
            parts.append('  系列「%s」：%s' % (label or '未命名', ', '.join(values)))
    return '\n'.join(parts) or '（图表数据缺失）'


def walk(tree):
    """Shapes in document order, groups flattened."""
    for child in tree:
        tag = child.tag.split('}')[1] if '}' in child.tag else child.tag
        if tag == 'grpSp':
            yield from walk(child)
        elif tag in ('sp', 'pic', 'graphicFrame', 'cxnSp'):
            yield tag, child


def describe_slide(deck, number, part, keep_data=False):
    root = deck.xml(part)
    rels = deck.rels(part)
    tree = root.find('p:cSld/p:spTree', NS)
    lines, pictures, charts = [], [], 0
    layout = ''
    for rid, (kind, target) in rels.items():
        if kind.endswith('/slideLayout') and target in deck.names:
            cs = deck.xml(target).find('p:cSld', NS)
            layout = cs.get('name', '') if cs is not None else ''
    notes = ''
    for rid, (kind, target) in rels.items():
        if kind.endswith('/notesSlide') and target in deck.names:
            notes = '\n'.join(paragraphs(deck.xml(target).find('p:cSld/p:spTree', NS)))
    k = 0
    for tag, shape in walk(tree):
        nv = shape.find('.//p:cNvPr', NS)
        name = nv.get('name', '') if nv is not None else ''
        if tag == 'sp':
            ph = shape.find('.//p:nvPr/p:ph', NS)
            role = (ph.get('type') or 'body') if ph is not None else ''
            paras = paragraphs(shape.find('p:txBody', NS)) if shape.find('p:txBody', NS) is not None else []
            if paras:
                lines.append('[%s%s]' % (('占位符 ' + role + '：' if role else ''), name))
                lines.extend(paras)
        elif tag == 'pic':
            blip = shape.find('.//a:blip', NS)
            target = deck.rels(part).get(blip.get(R_EMBED), (None, None))[1] if blip is not None else None
            if not target or target not in deck.names:
                continue
            k += 1
            ext = target.rsplit('.', 1)[-1].lower()
            item = {'id': 's%dp%d' % (number, k), 'name': name, 'alt': (nv.get('descr') or '') if nv is not None else '',
                    'format': ext, 'rect_cm': rect_cm(shape), 'bytes': deck.z.getinfo(target).file_size, 'part': target}
            if keep_data:
                item['data'] = base64.b64encode(deck.z.read(target)).decode()
            pictures.append(item)
        elif tag == 'graphicFrame':
            tbl = shape.find('.//a:tbl', NS)
            chart = shape.find('.//c:chart', NS)
            if tbl is not None:
                lines.append('[表格：%s]' % name)
                lines.append(table_markdown(tbl))
            elif chart is not None:
                charts += 1
                target = rels.get(chart.get(R_ID), (None, None))[1]
                lines.append('[图表：%s]' % name)
                lines.append(chart_text(deck, target) if target else '（图表数据缺失）')
    return {'layout': layout, 'lines': lines, 'pictures': pictures, 'notes': notes}


def main():
    path, mode = sys.argv[1], sys.argv[2]
    deck = Deck(path)
    order, dims = deck.slides()
    warnings = []
    if mode == 'text':
        out = ['# 演示文稿：共 %d 页%s' % (len(order), '，页面 %.1f × %.1f cm' % dims if dims else '')]
        index = []
        for number, part in enumerate(order, 1):
            info = describe_slide(deck, number, part)
            out.append('\n## 第 %d 页%s' % (number, '（版式：%s）' % info['layout'] if info['layout'] else ''))
            out.extend(info['lines'] or ['（没有文字内容）'])
            for pic in info['pictures']:
                unsupported = '' if pic['format'] in ('png', 'jpg', 'jpeg', 'webp', 'gif') else '，该格式暂不能搬运'
                alt = '，说明：%s' % pic['alt'] if pic['alt'] else ''
                out.append('[图片 %s：%s，%s，%s × %s cm，%d 字节%s%s]' % (
                    pic['id'], pic['name'] or '未命名', pic['format'], *( (pic['rect_cm'][2], pic['rect_cm'][3]) if pic['rect_cm'] else ('?', '?') ), pic['bytes'], alt, unsupported))
            if info['notes']:
                out.append('[演讲者备注]\n' + info['notes'])
            index.append({'slide': number, 'pictures': [{k: v for k, v in p.items() if k != 'part'} for p in info['pictures']]})
        if any(p['pictures'] for p in index):
            out.append('\n（要复用图片：read_attachment 带 slide=页码 或 picture=图片编号，会把图片放进项目并返回可用于 addImage 的路径。）')
        print(json.dumps({'text': '\n'.join(out), 'warnings': warnings, 'slides': index}, ensure_ascii=False))
    elif mode == 'pictures':
        target = sys.argv[3]
        found = []
        wanted = re.fullmatch(r's(\d+)p(\d+)', target)
        slide_no = int(wanted.group(1)) if wanted else int(target)
        if not 1 <= slide_no <= len(order):
            raise ValueError('没有第 %d 页' % slide_no)
        for pic in describe_slide(deck, slide_no, order[slide_no - 1], keep_data=True)['pictures']:
            if not wanted or pic['id'] == target:
                found.append(pic)
        if wanted and not found:
            raise ValueError('没有图片 %s' % target)
        print(json.dumps({'pictures': found}, ensure_ascii=False))
    else:
        raise ValueError('未知模式')


try:
    main()
except Exception as e:
    print(str(e), file=sys.stderr)
    sys.exit(1)
