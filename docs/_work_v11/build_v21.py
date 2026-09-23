# -*- coding: utf-8 -*-
"""Build v21 (final) = v20 + 参考文献段落按编号升序重排。

v20 的 25 条编号已是 1..25，但条目仍停在原物理位置，列表读起来乱序。
这里把真文献区（最后一个“参考资料”标题之后的、带正文内容的 [n] 段落）
按编号升序重排，段落 XML 原样搬动。
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v20.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v21.docx"

PARA = re.compile(r'<w:p(?: [^>]*)?>.*?</w:p>', re.S)
T_TXT = re.compile(r'<w:t(?: [^>]*)?>([^<]*)</w:t>')


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}
    xml = data['word/document.xml'].decode('utf-8')

    # 文献标题：最后一次字面量出现（目录里那处写作“参考资料”，列表标题同样写法，
    # 因此用“其后的第一个带正文的 [n] 段落”来确认是真列表）
    heads = [m.start() for m in re.finditer(r'<w:t[^>]*>参考资料</w:t>', xml)]
    print('“参考资料”出现位置: %s' % heads)

    # 逐个段落判断：形如 [n] 正文 的段落即文献条目
    entries = []
    for pm in PARA.finditer(xml):
        seg = pm.group(0)
        t = ''.join(m.group(1) for m in T_TXT.finditer(seg)).strip()
        m = re.match(r'\[(\d+)\]\s*(\S.*)$', t, re.S)
        if m and not re.fullmatch(r'(?:\[\d+\])+', m.group(2).strip()):
            entries.append((int(m.group(1)), pm.start(), pm.end()))
    print('文档中 [n] 正文 段落数: %d' % len(entries))
    if len(entries) != 25:
        raise SystemExit('ABORT: 期望 25 条文献，实际 %d' % len(entries))

    nums = [n for n, _, _ in entries]
    if sorted(nums) != list(range(1, 26)):
        raise SystemExit('ABORT: 编号异常 %s' % nums)

    # 文献区从「所有条目之前、最后一个标题」开始
    first_entry = min(s for _, s, _ in entries)
    cut_head = [h for h in heads if h < first_entry]
    if not cut_head:
        raise SystemExit('ABORT: 未找到列表标题')
    cut = cut_head[-1]
    print('采用标题位置 %d；首个条目 %d' % (cut, first_entry))

    # 重建：按**物理位置**遍历槽位，槽位 k 填入编号 k+1 的段落（段落 XML 原样搬动）
    region_head = xml[:cut]
    region = xml[cut:]
    rel = {n: (s - cut, e - cut) for n, s, e in entries}
    seq = sorted((s, e, n) for n, (s, e) in rel.items())   # 物理顺序

    out, pos = [], 0
    for k, (s, e, old_n) in enumerate(seq):
        out.append(region[pos:s])
        src_s, src_e = rel[k + 1]                          # 槽位 k 放编号 k+1
        out.append(region[src_s:src_e])
        pos = e
    out.append(region[pos:])
    xml = region_head + ''.join(out)
    print('物理顺序下原编号: %s' % [n for _, _, n in seq])

    data['word/document.xml'] = xml.encode('utf-8')

    if os.path.exists(DST):
        os.remove(DST)
    with zipfile.ZipFile(DST, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])
    print('原编号顺序: %s' % nums)
    print('\nOUTPUT: %s (%.2f MB)' % (DST, os.path.getsize(DST) / 1024.0 / 1024.0))


if __name__ == '__main__':
    main()
