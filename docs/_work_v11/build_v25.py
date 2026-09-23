# -*- coding: utf-8 -*-
"""v25（定稿）= v24 + 目录页码同步（分页后 12.2 / 12.3 / 参考资料 由 25 变 26）。"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v24.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.docx"

FIXES = [
    ('12.2 可信人工智能与数字包容', 25, 26),
    ('12.3 发展愿景', 25, 26),
    ('参考资料', 25, 26),
]
TBL_RE = re.compile(r'<w:tbl>.*?</w:tbl>', re.S)


def label_cell_span(toc, label):
    """找到目录表内含该标签的 <w:tc> 的绝对区间（目录表内相对偏移）。"""
    hits = []
    for m in re.finditer(r'<w:tc>.*?</w:tc>', toc, re.S):
        txt = re.sub(r'\s+', '', ''.join(
            x.group(1) for x in re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', m.group(0))))
        if txt and not txt.isdigit() and txt == re.sub(r'\s+', '', label):
            hits.append((m.start(), m.end()))
    return hits


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}
    xml = data['word/document.xml'].decode('utf-8')

    toc_m = list(TBL_RE.finditer(xml))[1]
    toc = toc_m.group(0)

    edits = []
    for label, old, new in FIXES:
        spans = label_cell_span(toc, label)
        if len(spans) != 1:
            raise SystemExit('ABORT: %r 在目录里匹配到 %d 个标签格' % (label, len(spans)))
        _, ce = spans[0]
        # 标签格之后的第一段文本即页码
        m = re.search(r'<w:t(?: [^>]*)?>(\d{1,2})</w:t>', toc[ce:ce + 1500])
        if not m:
            raise SystemExit('ABORT: %r 后未找到页码' % label)
        if int(m.group(1)) != old:
            raise SystemExit('ABORT: %r 页码 %s ≠ %d' % (label, m.group(1), old))
        s = ce + m.start(1)
        e = ce + m.end(1)
        edits.append((s, e, str(new)))
        print('  %-30s %d -> %d' % (label, old, new))

    for s, e, rep in sorted(edits, key=lambda x: -x[0]):
        toc = toc[:s] + rep + toc[e:]
        print('  已改：偏移 %d 处写入 %s' % (s, rep))

    xml = xml[:toc_m.start()] + toc + xml[toc_m.end():]
    data['word/document.xml'] = xml.encode('utf-8')

    if os.path.exists(DST):
        os.remove(DST)
    with zipfile.ZipFile(DST, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])
    print('OUTPUT: %s (%.2f MB)' % (DST, os.path.getsize(DST) / 1024.0 / 1024.0))


if __name__ == '__main__':
    main()
