# -*- coding: utf-8 -*-
"""v26（定稿）= v25 + 修正表7 的题注/表格/正文顺序。

问题：[171] 7.5 节标题 -> [172] 表7 题注 -> [173] 正文段（成本侧规划假设）-> [174] 表格
      题注与表格被一段正文隔开，读者会以为表格属于下一段。
修法：把 [173] 移到表格之后 -> 标题 -> 题注 -> 表格 -> 该正文段 -> 其余说明段
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v26.docx"

ANCHOR = '成本侧的规划假设为：本地模型承担补全'


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}
    xml = data['word/document.xml'].decode('utf-8')

    body_m = re.search(r'<w:body>(.*)</w:body>', xml, re.S)
    body_off = body_m.start(1)

    blocks = []
    for m in re.finditer(r'<w:tbl>.*?</w:tbl>|<w:p(?: [^>]*)?>.*?</w:p>',
                         body_m.group(1), re.S):
        blocks.append((m.start(), m.end(), m.group(0)))

    # 定位：题注段 / 正文段 / 表格
    cap_i = para_i = tbl_i = None
    for k, (s, e, seg) in enumerate(blocks):
        t = ''.join(x.group(1) for x in
                    re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', seg))
        if '产品版本、定价与单位经济' in t and seg.startswith('<w:p'):
            cap_i = k
        if ANCHOR in t:
            para_i = k
        if seg.startswith('<w:tbl') and '产品版本' in t:
            tbl_i = k
    if None in (cap_i, para_i, tbl_i):
        raise SystemExit('ABORT: 定位失败 cap=%s para=%s tbl=%s' % (cap_i, para_i, tbl_i))
    if not (cap_i < para_i < tbl_i):
        raise SystemExit('ABORT: 顺序不符 cap=%d para=%d tbl=%d' % (cap_i, para_i, tbl_i))
    print('定位: 题注块[%d] -> 正文块[%d] -> 表格块[%d]' % (cap_i, para_i, tbl_i))

    para_seg = blocks[para_i][2]
    p_s, p_e = blocks[para_i][0], blocks[para_i][1]
    tbl_e = blocks[tbl_i][1]

    # 删除正文段，再把它插到表格之后
    tmp = xml[:body_off + p_s] + xml[body_off + p_e:]
    tbl_e_new = tbl_e - (p_e - p_s)
    out = tmp[:body_off + tbl_e_new] + para_seg + tmp[body_off + tbl_e_new:]

    # 校验：新顺序应为 标题 -> 题注 -> 表格 -> 正文
    body2 = re.search(r'<w:body>(.*)</w:body>', out, re.S).group(1)
    order = []
    for m in re.finditer(r'<w:tbl>.*?</w:tbl>|<w:p(?: [^>]*)?>.*?</w:p>', body2, re.S):
        seg = m.group(0)
        t = ''.join(x.group(1) for x in
                    re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', seg))
        stripped = t.strip()
        if stripped == '7.5 产品版本与单位经济':
            order.append('节标题')
        elif seg.startswith('<w:p') and ANCHOR in t:
            order.append('正文段')
        elif seg.startswith('<w:tbl') and '产品版本' in t and '目标用户' in t:
            order.append('表格')
        elif seg.startswith('<w:p') and stripped.startswith('表7') and '单位经济' in t:
            order.append('题注')
        if len(order) == 4:
            break
    print('新顺序: %s' % ' -> '.join(order))
    if order != ['节标题', '题注', '表格', '正文段']:
        raise SystemExit('ABORT: 新顺序不正确: %s' % order)

    data['word/document.xml'] = out.encode('utf-8')
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
