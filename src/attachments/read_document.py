import json,sys,subprocess,zipfile,xml.etree.ElementTree as ET
from pathlib import Path
path=Path(sys.argv[1]);name=sys.argv[2];ext=Path(name).suffix.lower();warnings=[]
try:
    if ext=='.pdf':
        result=subprocess.run(['pdftotext','-layout','-enc','UTF-8',str(path),'-'],capture_output=True,timeout=30,check=True)
        text=result.stdout.decode('utf-8')
        if not text.strip(): warnings.append('PDF 未提取到文字；可能为扫描件，需要将相关页渲染为图片后进行视觉分析。')
    elif ext=='.docx':
        with zipfile.ZipFile(path) as z:
            entry=z.getinfo('word/document.xml')
            if entry.file_size>32*1024*1024: raise ValueError('DOCX 正文解压后超过限制')
            root=ET.fromstring(z.read(entry));ns={'w':'http://schemas.openxmlformats.org/wordprocessingml/2006/main'}
            text='\n'.join(''.join(p.itertext()) for p in root.findall('.//w:p',ns))
            # itertext includes inline text and preserves paragraph/table traversal order.
            if z.namelist().count('word/document.xml')!=1:raise ValueError('DOCX 正文结构无效')
            warnings.append('已提取正文和表格文字；图片和复杂版式不包含在文字提取结果中。')
    elif ext in ('.txt','.md','.csv','.json','.xml','.html','.css','.js','.ts','.py','.rs','.go','.yaml','.yml','.svg'):
        raw=path.read_bytes()
        try:text=raw.decode('utf-8-sig')
        except UnicodeDecodeError:text=raw.decode('gb18030');warnings.append('文件按 GB18030 解码。')
    else: raise ValueError('该格式已保存，但尚无对应的内容解析器。')
    if len(text)>8_000_000:raise ValueError('提取文本超过限制')
    print(json.dumps({'text':text,'warnings':warnings},ensure_ascii=False))
except Exception as e:
    print(str(e),file=sys.stderr);sys.exit(1)
