# -*- coding: utf-8 -*-
"""Build v23: 正文从第 1 页起算 + 目录中缝加宽。

页码
  * 目录表之后插入分节符，文档变两节
  * 第一节（封面+目录）：页脚换成无页码的 footer3.xml
  * 第二节（正文起）：页脚含 PAGE 域，pgNumType start=1 -> 正文第 1 页
  * 目录静态页码整体减 2

中缝
  * 页码列 1065 -> 1000 twips；竖线留白 90 -> 60；页码右缩进 140 -> 30
  * 右栏条目标签左缩进 90 -> 150 twips
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v22.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v23.docx"

FOOTER_NUM_RID = 'rId17'      # footer2.xml：含 PAGE 域
FOOTER_PLAIN_RID = 'rId18'    # footer3.xml：无页码
GAP = 60                      # 竖线两侧的 space
LABEL_IND = 190               # 右栏条目标签左缩进（浅缩进会把文字拉近竖线）
NUM_INSET = 10                # 左栏页码右缩进
NUMCOL = 1030                  # 页码列宽（原 1065；收窄后页码向条目靠、中缝变宽）

TBL_RE = re.compile(r'<w:tbl>.*?</w:tbl>', re.S)
TR_RE = re.compile(r'<w:tr>.*?</w:tr>', re.S)
TC_RE = re.compile(r'<w:tc>.*?</w:tc>', re.S)
SECT_RE = re.compile(r'<w:sectPr[^>]*>.*?</w:sectPr>', re.S)


def cell_text(tc):
    return ''.join(m.group(1) for m in
                   re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', tc)).strip()


def set_ind(tc, pairs):
    def patched(mm):
        ind = mm.group(0)
        for attr, val in pairs:
            if 'w:%s=' % attr in ind:
                ind = re.sub(r'w:%s="[^"]*"' % attr, 'w:%s="%s"' % (attr, val), ind)
            else:
                ind = ind[:-2].rstrip() + ' w:%s="%s"/>' % (attr, val)
        return ind
    if '<w:ind ' in tc:
        return re.sub(r'<w:ind [^>]*/>', patched, tc)
    return tc


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}
    xml = data['word/document.xml'].decode('utf-8')

    # ------------------------------------------------ 1. 中缝与列宽
    toc_m = list(TBL_RE.finditer(xml))[1]
    toc = toc_m.group(0)
    # 页码列收窄：页码整体向自己的条目靠、中缝随之变宽
    # 关键：行内「页码格」历史 tcW 也是 2660（与标签同宽），必须逐格按位置改写，
    # 否则会出现标签格被当成页码格压窄、条目折行的问题
    toc, g = re.subn(r'<w:tblGrid>.*?</w:tblGrid>',
                     '<w:tblGrid><w:gridCol w:w="2660"/><w:gridCol w:w="%d"/>'
                     '<w:gridCol w:w="2660"/><w:gridCol w:w="%d"/></w:tblGrid>'
                     % (NUMCOL, NUMCOL), toc, flags=re.S)
    grid_sum = 2 * 2660 + 2 * NUMCOL
    # tblW 保持 v22 原值 9581：改小会被 LibreOffice 重新分配列宽，反而挤压条目列
    print('表格: grid 重写 %d 处（tblW 保持原值）' % g)
    out, pos, n_lab, n_num, n_width = [], 0, 0, 0, 0
    for tr in TR_RE.finditer(toc):
        out.append(toc[pos:tr.start()])
        row = tr.group(0)
        cells = list(TC_RE.finditer(row))
        new_row, p = [], 0
        for ci, cm in enumerate(cells):
            new_row.append(row[p:cm.start()])
            cell = cm.group(0)
            txt = cell_text(cell)
            # 按列位置写死 tcW：0/2 标签列 -> 2660，1/3 页码列 -> NUMCOL
            want = NUMCOL if ci in (1, 3) else 2660
            cell, nw = re.subn(r'<w:tcW w:w="\d+" w:type="dxa"/>',
                               '<w:tcW w:w="%d" w:type="dxa"/>' % want, cell, count=1)
            n_width += nw
            if ci == 1 and txt.isdigit():
                cell = set_ind(cell, [('right', str(NUM_INSET)), ('end', str(NUM_INSET))])
                n_num += 1
            elif ci == 2:
                cell = set_ind(cell, [('left', str(LABEL_IND)), ('start', str(LABEL_IND))])
                n_lab += 1
            new_row.append(cell)
            p = cm.end()
        new_row.append(row[p:])
        out.append(''.join(new_row))
        pos = tr.end()
    out.append(toc[pos:])
    toc = ''.join(out)
    toc, s = re.subn(r'(w:space=")90(")', r'\g<1>%d\g<2>' % GAP, toc)
    print('列宽: 逐格改写 tcW %d 格（标签 2660 / 页码 %d）；中缝留白 %d 处；'
          '标签缩进 %d 格；页码缩进 %d 格' % (n_width, NUMCOL, s, n_lab, n_num))
    xml = xml[:toc_m.start()] + toc + xml[toc_m.end():]

    # ------------------------------------------------ 2. 目录页码减 2
    toc_m2 = list(TBL_RE.finditer(xml))[1]
    toc2 = toc_m2.group(0)
    num_run = re.compile(
        r'(<w:rPr><w:rFonts w:ascii="Aptos" w:hAnsi="Aptos" w:eastAsia="[^"]+"/>'
        r'<w:b(?: w:val="false")?/><w:color w:val="(?:666D73|B98A2F)"/>'
        r'<w:sz w:val="1[0-9]"/></w:rPr><w:t>)(\d{1,2})(</w:t>)')
    shifted = []

    def shift(m):
        old = int(m.group(2))
        new = old - 2
        if new < 1:
            raise SystemExit('ABORT: 目录页码 %d 减 2 后 < 1' % old)
        shifted.append((old, new))
        return m.group(1) + str(new) + m.group(3)

    toc2 = num_run.sub(shift, toc2)
    print('目录页码减 2: %d 个，范围 %d..%d -> %d..%d' % (
        len(shifted), min(a for a, _ in shifted), max(a for a, _ in shifted),
        min(b for _, b in shifted), max(b for _, b in shifted)))
    xml = xml[:toc_m2.start()] + toc2 + xml[toc_m2.end():]

    # ------------------------------------------------ 3. 分节与页码起始
    sect_m = SECT_RE.search(xml)
    if not sect_m:
        raise SystemExit('ABORT: 未找到 body 级 sectPr')
    sect = sect_m.group(0)
    body_sect = sect

    # 正文节：三套页脚（even/default/first）统一指向含 PAGE 域的 footer2.xml，
    # 否则首套（even）会落到另一个页脚上，正文首页被吞掉页码
    body_sect = re.sub(r'(<w:footerReference w:type="[^"]+" r:id=")rId\d+(")',
                       r'\g<1>%s\g<2>' % FOOTER_NUM_RID, body_sect)

    # 现有 pgNumType 只有 fmt，需要补 start=1 让正文从第 1 页起算
    if '<w:pgNumType' in body_sect:
        body_sect = re.sub(r'<w:pgNumType[^>]*/>',
                           '<w:pgNumType w:start="1" w:fmt="decimal"/>', body_sect, count=1)
    else:
        body_sect = body_sect.replace('</w:sectPr>', '<w:pgNumType w:start="1"/></w:sectPr>')

    # 第一节（封面+目录）：页脚一律换成无页码版本，并去掉 start=1
    front_inner = body_sect.replace('<w:sectPr>', '').replace('</w:sectPr>', '')
    front_inner = re.sub(r'(<w:footerReference w:type="[^"]+" r:id=")rId\d+(")',
                         r'\g<1>%s\g<2>' % FOOTER_PLAIN_RID, front_inner)
    front_inner = front_inner.replace('<w:pgNumType w:start="1" w:fmt="decimal"/>',
                                      '<w:pgNumType w:fmt="decimal"/>')

    # 目录表之后本来是一个“分页符”段落；把它改成承载第一节 sectPr 的分节段落，
    # 不新增段落 -> 不会多出空白页
    pbreak = ('<w:p><w:pPr><w:pStyle w:val="Normal"/><w:rPr></w:rPr></w:pPr>'
              '<w:r><w:rPr></w:rPr></w:r><w:r><w:br w:type="page"/></w:r></w:p>')
    toc_end = list(TBL_RE.finditer(xml))[1].end()
    seg = xml[toc_end:toc_end + len(pbreak) + 40]
    if pbreak not in seg:
        raise SystemExit('ABORT: 目录后的分页符段落结构不符:\n%s' % seg[:200])
    front_para = ('<w:p><w:pPr><w:sectPr>' + front_inner + '</w:sectPr></w:pPr>'
                  '<w:r><w:br w:type="page"/></w:r></w:p>')
    xml = xml[:toc_end] + front_para + xml[toc_end + len(pbreak):]

    # body 级（最后一个）sectPr 更新为正文节
    sects = list(SECT_RE.finditer(xml))
    last = sects[-1]
    xml = xml[:last.start()] + body_sect + xml[last.end():]

    print('分节: 封面+目录 -> 页脚 %s（无页码）；正文 -> 页脚 %s，start=1' % (
        FOOTER_PLAIN_RID, FOOTER_NUM_RID))
    print('sectPr 数量: %d' % len(re.findall(r'<w:sectPr[ >]', xml)))

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




