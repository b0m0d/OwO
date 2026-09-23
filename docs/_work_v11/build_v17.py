# -*- coding: utf-8 -*-
"""Build v17 = v16 + (1) white backgrounds, (2) cover table moved to lower-left and tightened.

Changes
  1. every cell fill FCFBF8 (目录底色) / F1EFEA (首页表格底色) -> FFFFFF
  2. cover identity table: jc center -> left, 段前 260pt push down, tighter rows
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v16.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"

COVER_PUSH_TWIPS = '4300'   # 215 pt before the identity table -> sits low on the page
COVER_LINE = '240'          # 12 pt paragraph line box inside the table
COVER_ROW = '350'           # row floor 17.5 pt (was 476 = 23.8 pt)


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')

    # ---------------------------------------------------- 1. white backgrounds
    for fill in ('FCFBF8', 'F1EFEA'):
        n = len(re.findall(r'w:fill="%s"' % fill, xml))
        xml = re.sub(r'(w:fill=")%s(")' % fill, r'\g<1>FFFFFF\g<2>', xml)
        print('底纹 %s -> FFFFFF : %d 处' % (fill, n))
    left = re.findall(r'w:fill="(FCFBF8|F1EFEA)"', xml)
    if left:
        raise SystemExit('ABORT: residual cream fills %s' % set(left))

    # ------------------------------------------- 2. cover table: locate it
    starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
    ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
    s, e = starts[0], ends[0]
    cover = xml[s:e]
    if '参赛组别' not in cover:
        raise SystemExit('ABORT: first table is not the cover identity table')

    # center -> left
    cover, n_jc = re.subn(r'(<w:tblPr><w:tblW [^>]*/>)<w:jc w:val="center"/>',
                          r'\g<1><w:jc w:val="left"/>', cover, count=1)
    print('封面表格对齐 center -> left : %d' % n_jc)

    # tighter cell margins (80 -> 30 twips)
    cover, n_mar = re.subn(r'<w:start w:w="80" w:type="dxa"/>', '<w:start w:w="30" w:type="dxa"/>', cover)
    cover, n_mar2 = re.subn(r'<w:end w:w="80" w:type="dxa"/>', '<w:end w:w="30" w:type="dxa"/>', cover)
    cover, n_mar3 = re.subn(r'<w:top w:w="50" w:type="dxa"/>', '<w:top w:w="20" w:type="dxa"/>', cover)
    cover, n_mar4 = re.subn(r'<w:bottom w:w="50" w:type="dxa"/>', '<w:bottom w:w="20" w:type="dxa"/>', cover)
    print('单元格边距收紧: 左右 %d/%d, 上下 %d/%d' % (n_mar, n_mar2, n_mar3, n_mar4))

    # rows: atLeast 476 -> atLeast 300 twips (15pt) => tighter, still safe for 8.5pt text
    cover, n_th = re.subn(r'<w:trHeight w:val="\d+" w:hRule="atLeast"/>',
                          '<w:trHeight w:val="%s" w:hRule="atLeast"/>' % COVER_ROW, cover)
    print('封面行高收紧 atLeast 476 -> %s : %d 行' % (COVER_ROW, n_th))

    # paragraph spacing inside the cover cells: collapse to zero
    cover, n_sp = re.subn(r'<w:spacing w:before="0" w:after="0"/>',
                          '<w:spacing w:lineRule="exact" w:line="%s" w:before="0" w:after="0"/>' % COVER_LINE,
                          cover)
    print('封面段落间距收紧: %d 处' % n_sp)

    xml = xml[:s] + cover + xml[e:]

    # push the table down: spacing before the last paragraph preceding it
    # (the hero paragraph carries after=560; raise it so the table drops)
    hero = re.search(r'(<w:spacing w:lineRule="auto" w:line="348" w:before="0" w:after=")(\d+)("/>)', xml)
    if not hero:
        raise SystemExit('ABORT: hero paragraph spacing not found')
    old_after = hero.group(2)
    xml = xml[:hero.start(2)] + COVER_PUSH_TWIPS + xml[hero.end(2):]
    print('首页表格下移: 段落 after %s -> %s twips' % (old_after, COVER_PUSH_TWIPS))

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
