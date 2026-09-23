# -*- coding: utf-8 -*-
"""v24 = v23 + 修复表10（项目风险登记表）被页底吞掉数据行的问题。

原因：表格被排到第 26 页底部，表头以下的空间不足，而行带 cantSplit（不允许行内断页），
LibreOffice 直接把 6 行数据行丢掉了。
修法：在「表10 项目风险登记表」题注前插入一个分页符，让表格从新页开始（表格约 191 pt，够放）。
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v23.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v24.docx"

PAGEBREAK_P = ('<w:p><w:pPr><w:pStyle w:val="Normal"/><w:spacing w:before="0" w:after="0" '
               'w:line="1" w:lineRule="exact"/><w:rPr><w:sz w:val="2"/></w:rPr></w:pPr>'
               '<w:r><w:br w:type="page"/></w:r></w:p>')

T_RE = re.compile(r'<w:t(?: [^>]*)?>([^<]*)</w:t>')


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}
    xml = data['word/document.xml'].decode('utf-8')

    # 定位表10 题注段落（"表10" 被拆成多个 run，用后半句锚定）
    i = xml.find('项目风险登记表')
    if i < 0:
        raise SystemExit('ABORT: 未找到表10 题注')
    p_start = max(xml.rfind('<w:p ', 0, i), xml.rfind('<w:p>', 0, i))
    if p_start < 0:
        raise SystemExit('ABORT: 未找到题注段落起点')
    print('题注段落起点 %d，run=%s' % (
        p_start, re.findall(r'<w:t(?: [^>]*)?>([^<]*)</w:t>',
                            xml[p_start:xml.find('</w:p>', i) + 6])))

    # 检查前面是否已经有分页符，避免重复插入
    prev = xml[max(0, p_start - 400):p_start]
    if 'w:br w:type="page"' in prev:
        print('题注前已有分页符，跳过插入')
    else:
        xml = xml[:p_start] + PAGEBREAK_P + xml[p_start:]
        print('已在「表10  项目风险登记表」前插入分页符')

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
