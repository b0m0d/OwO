# -*- coding: utf-8 -*-
"""Build v14 = v13 + corrected TOC page numbers, then repack the docx."""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v14.docx"

# entry text -> correct page (verified against the rendered PDF)
FIXES = [
    ('6.1 行业机会与目标市场', 16, 17),
    ('6.4 核心竞争力', 19, 18),
    ('7.3 获客、转化与留存', 20, 19),
    ('8.3 二十四个月路线与阶段决策门', 22, 21),
    ('8.5 知识产权与成果计划', 23, 22),
    ('9.3 协作与质量机制', 24, 23),
]

NUM_RUN = re.compile(
    r'(<w:rPr><w:rFonts w:ascii="Aptos" w:hAnsi="Aptos" w:eastAsia="Aptos"/>'
    r'<w:b w:val="0"/><w:color w:val="666D73"/><w:sz w:val="15"/></w:rPr><w:t>)(\d{1,2})(</w:t>)')


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')

    for entry, old, new in FIXES:
        # anchor on the TOC cell (ends the paragraph inside a table cell),
        # not on the section heading of the same name
        anchor = '>' + entry + '<'
        hits = [m.start() for m in re.finditer(re.escape(anchor), xml)]
        cand = []
        for h in hits:
            seg = xml[h:h + 90]
            if '</w:p></w:tc>' in seg:
                cand.append(h)
        if len(cand) != 1:
            raise SystemExit('ABORT: TOC anchor %r -> %d cell candidates (raw hits %d)'
                             % (entry, len(cand), len(hits)))
        i = cand[0]
        seg = xml[i:i + 900]
        m = NUM_RUN.search(seg)
        if not m:
            raise SystemExit('ABORT: no page-number run after %r' % entry)
        got = int(m.group(2))
        if got != old:
            raise SystemExit('ABORT: %r page number is %d, expected %d' % (entry, got, old))
        s = i + m.start(2)
        e = i + m.end(2)
        xml = xml[:s] + str(new) + xml[e:]
        print('  %-34s %2d -> %2d' % (entry, old, new))

    data['word/document.xml'] = xml.encode('utf-8')

    if os.path.exists(DST):
        os.remove(DST)
    with zipfile.ZipFile(DST, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])
    print('\nOUTPUT: %s (%.2f MB)' % (DST, os.path.getsize(DST) / 1024.0 / 1024.0))


if __name__ == '__main__':
    main()
