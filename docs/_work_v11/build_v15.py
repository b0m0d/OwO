# -*- coding: utf-8 -*-
"""Build v15 = v14 (cover image already updated) + retuned 目录 typography.

TOC retune
  * 章节行: 微软雅黑 9pt 加粗 / 页码 9pt 金色，行距 1.25，段前 4pt —— 章节之间有呼吸感
  * 小节行: 微软雅黑 8.5pt / 页码 8.5pt 灰色，行距 1.15
  * 字体统一为微软雅黑（原来章节用微软雅黑、小节用宋体，混用造成观感不齐）
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v14.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v15.docx"

# --- typography targets -------------------------------------------------
CH_TXT_SZ = '18'   # 9pt   chapter entries
CH_NUM_SZ = '18'   # 9pt   chapter page numbers
SEC_TXT_SZ = '17'  # 8.5pt section entries
SEC_NUM_SZ = '17'  # 8.5pt section page numbers
CH_LINE = '316'    # 15.8pt exact line box
SEC_LINE = '300'   # 15pt   exact line box
CH_BEFORE = '100'  # 5pt before chapter rows
SEC_BEFORE = '0'
LINE_RULE = 'exact'

EA = '微软雅黑'
LATIN = 'Aptos'

CH_COLOR = '242C33'
CH_NUM_COLOR = 'B98A2F'
SEC_COLOR = '666D73'

CHAPTER_RE = re.compile(r'^(第[一二三四五六七八九十]+章|项目摘要|参考资料)')


def classify(cell_text):
    t = cell_text.replace('\u3000', ' ').strip().lstrip()
    return 'chapter' if CHAPTER_RE.match(t) else 'section'


def retune_paragraph(p, level):
    """Rewrite pPr spacing and every run's rPr for one TOC cell paragraph."""
    xml = p
    if level == 'chapter':
        line, before = CH_LINE, CH_BEFORE
        txt_sz, num_sz = CH_TXT_SZ, CH_NUM_SZ
    else:
        line, before = SEC_LINE, SEC_BEFORE
        txt_sz, num_sz = SEC_TXT_SZ, SEC_NUM_SZ

    # spacing
    def fix_spacing(m):
        s = m.group(0)
        s = re.sub(r'w:line="\d+"', 'w:line="%s"' % line, s)
        s = re.sub(r'w:lineRule="[^"]*"', 'w:lineRule="%s"' % LINE_RULE, s)
        s = re.sub(r'w:before="\d+"', 'w:before="%s"' % before, s)
        s = re.sub(r'w:after="\d+"', 'w:after="0"', s)
        return s
    xml = re.sub(r'<w:spacing [^>]*/>', fix_spacing, xml)

    # runs: the page-number run is a direct child of a right-aligned paragraph
    is_right = 'w:jc w:val="end"' in xml
    def fix_run(m):
        block = m.group(0)
        sz = num_sz if is_right else txt_sz
        color = (CH_NUM_COLOR if is_right else CH_COLOR) if level == 'chapter' \
            else SEC_COLOR
        bold = '<w:b/>' if level == 'chapter' else '<w:b w:val="false"/>'
        block = re.sub(r'<w:rFonts[^>]*/>',
                       '<w:rFonts w:ascii="%s" w:hAnsi="%s" w:eastAsia="%s"/>' % (LATIN, LATIN, EA),
                       block)
        block = re.sub(r'<w:b[^>]*/>', bold, block)
        block = re.sub(r'<w:sz w:val="\d+"/>', '<w:sz w:val="%s"/>' % sz, block)
        if '<w:sz ' not in block and '<w:szCs' not in block:
            block = block.replace('<w:rPr>', '<w:rPr><w:sz w:val="%s"/>' % sz, 1)
        block = re.sub(r'<w:color w:val="[^"]*"/>', '<w:color w:val="%s"/>' % color, block)
        return block
    xml = re.sub(r'<w:rPr>.*?</w:rPr>', fix_run, xml, flags=re.S)
    return xml


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')

    # --- isolate the TOC table (2nd tbl) by string scanning -----------------
    tbl_starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
    tbl_ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
    if len(tbl_starts) != len(tbl_ends):
        raise SystemExit('ABORT: tbl open/close mismatch %d/%d' % (len(tbl_starts), len(tbl_ends)))
    spans = list(zip(tbl_starts, tbl_ends))
    s, e = spans[1]
    toc = xml[s:e]

    # --- walk rows ---------------------------------------------------------
    out = []
    pos = 0
    counts = {'chapter': 0, 'section': 0}
    row_spans = [(m.start(), m.end()) for m in re.finditer(r'<w:tr>.*?</w:tr>', toc, re.S)]
    if not row_spans:
        raise SystemExit('ABORT: no rows found in TOC table')

    for rs, re_ in row_spans:
        out.append(toc[pos:rs])
        row = toc[rs:re_]
        cells = [(m.start(), m.end()) for m in re.finditer(r'<w:tc>.*?</w:tc>', row, re.S)]

        # pass 1: identify label cells and remember the level that follows each
        pending = []   # (cell_xml, level) in document order
        for a, b in cells:
            cell = row[a:b]
            cell_text = re.sub(r'<[^>]+>', '', cell).replace('\u3000', ' ').strip()
            cell_text = re.sub(r'\s+', ' ', cell_text)
            if not cell_text or cell_text.isdigit():
                lvl = pending[-1][1] if pending else 'section'
                pending.append((cell, lvl))
            else:
                lvl = classify(cell_text)
                counts[lvl] += 1
                pending.append((cell, lvl))

        new_row = []
        p = 0
        for (cell, lvl), (a, b) in zip(pending, cells):
            new_row.append(row[p:a])
            new_row.append(retune_paragraph(cell, lvl))
            p = b
        new_row.append(row[p:])
        out.append(''.join(new_row))
        pos = re_
    out.append(toc[pos:])

    new_toc = ''.join(out)

    # widen the entry columns (keep the 4-column table width = full text column)
    new_toc2, n_grid = re.subn(
        r'(<w:tblGrid>)<w:gridCol w:w="\d+"/><w:gridCol w:w="\d+"/>'
        r'<w:gridCol w:w="\d+"/><w:gridCol w:w="\d+"/>(</w:tblGrid>)',
        r'\g<1><w:gridCol w:w="2660"/><w:gridCol w:w="1065"/>'
        r'<w:gridCol w:w="2660"/><w:gridCol w:w="1065"/>\g<2>',
        new_toc, count=1)
    if n_grid != 1:
        raise SystemExit('ABORT: TOC tblGrid pattern matched %d times' % n_grid)
    new_toc = new_toc2
    # cell widths: label cells 2395 -> 2660 ; page-number cells 2395/2396 -> 1065
    new_toc, n_w = re.subn(r'<w:tcW w:w="2395" w:type="dxa"/>',
                           '<w:tcW w:w="2660" w:type="dxa"/>', new_toc)
    new_toc, n_w2 = re.subn(r'<w:tcW w:w="2396" w:type="dxa"/>',
                            '<w:tcW w:w="1065" w:type="dxa"/>', new_toc)
    # rows were pinned to 261 twips (13.05 pt) with hRule="exact", which clips any
    # font larger than that -> switch to atLeast so the row can grow with the text
    new_toc, n_th = re.subn(r'<w:trHeight w:val="\d+" w:hRule="exact"/>',
                            '<w:trHeight w:val="400" w:hRule="atLeast"/>', new_toc)
    print('行高改写 (exact -> atLeast): %d 行' % n_th)

    print('列宽改写: 标签列 %d 个, 页码列 %d 个' % (n_w, n_w2))

    xml = xml[:s] + new_toc + xml[e:]

    print('章节行 %d，小节行 %d' % (counts['chapter'], counts['section']))
    print('章节: 微软雅黑 %s 加粗, 行距 %s, 段前 %s ; 小节: 微软雅黑 %s, 行距 %s' % (
        CH_TXT_SZ, CH_LINE, CH_BEFORE, SEC_TXT_SZ, SEC_LINE))

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
