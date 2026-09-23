# -*- coding: utf-8 -*-
"""Build v16 = v15 + shortened over-long TOC labels (body headings untouched).

Long labels wrap onto a second line, which made the TOC spill onto page 3 and look
cramped. The document's own section headings stay exactly as they are; only the
TOC entry text is condensed to its key terms.
"""
import os
import re
import zipfile

from PIL import ImageFont

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v15.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v16.docx"

# TOC label -> condensed label (keeps the leading number and the key nouns)
SHORTEN = {
    '1.1 一个发生在输入时刻的真实矛盾': '1.1 输入时刻的真实矛盾',
    '1.3 输入时刻作为原生入口的技术依据': '1.3 输入时刻作为原生入口的依据',
    '4.1 核心创新一 意图胶囊与最小情境机制': '4.1 意图胶囊与最小情境机制',
    '4.2 核心创新二 三维任务决策与受控自治升级': '4.2 三维决策与受控自治升级',
    '4.3 协作机制 版本化成果契约与自适应组队': '4.3 版本化成果契约与自适应组队',
    '4.4 关键工程体系 原位交付与策略验证恢复': '4.4 原位交付与策略验证恢复',
    '4.5 核心机制的扩展 跨设备能力节点': '4.5 跨设备能力节点',
}

CELL_W_PT = 2660 / 20.0
INDENT_PT = 125 / 20.0


def main():
    font = ImageFont.truetype(r'C:\Windows\Fonts\msyh.ttc', 19)  # 9.5pt

    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')

    # only touch the TOC table
    starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
    ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
    s, e = starts[1], ends[1]
    toc = xml[s:e]

    for old, new in SHORTEN.items():
        # the label may be split across runs (number run + text run); handle the
        # common case where the whole label sits in one <w:t>
        if toc.count(old) == 1:
            toc = toc.replace(old, new)
            print('  [整体] %-40s -> %s' % (old, new))
            continue
        # split case: "4.1 " in one run and the rest in the next
        num, rest = old.split(' ', 1)
        if toc.count('>%s <' % num) >= 1 and toc.count('>%s<' % rest) == 1:
            toc = toc.replace('>%s<' % rest, '>%s<' % new.split(' ', 1)[1])
            print('  [拆分] %-40s -> %s' % (old, new))
        else:
            raise SystemExit('ABORT: could not locate TOC label %r (split=%d)'
                             % (old, toc.count('>%s<' % old.split(' ', 1)[1])))

    # verify: no label should still exceed the column width
    flat = re.sub(r'<[^>]+>', '\n', toc)
    over = []
    for lab in set(x.strip() for x in flat.split('\n')):
        if len(lab) < 6 or lab.isdigit():
            continue
        w = font.getlength(lab) / 2.0 + INDENT_PT
        if w > CELL_W_PT:
            over.append((w, lab))
    print('\n仍超列宽的条目: %s' % (sorted(over, reverse=True) or '无'))

    xml = xml[:s] + toc + xml[e:]
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
