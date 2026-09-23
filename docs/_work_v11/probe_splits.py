# -*- coding: utf-8 -*-
"""Show the run split for each anchor we need to edit."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

PROBES = [
    '第一年经营成本',
    '收入行口径为',
    '策略、验证与恢复体系负责审批',
    '输入法产品也在进入大模型时代',
    '训练的是把模糊需求',
    '不代表项目已经取得收入',
    '全部单价与转化率均属',
    '在证据取得之前',
    '0 / 0 / 0',
    '输入法信任风险',
    '输入法场景的安全原则',
]

for p in PROBES:
    i = xml.find(p)
    if i < 0:
        print('%-22s 未找到' % p[:20])
        continue
    # expand to the enclosing paragraph
    s = xml.rfind('<w:p ', 0, i)
    if s < 0:
        s = xml.rfind('<w:p>', 0, i)
    e = xml.find('</w:p>', i) + 6
    seg = xml[s:e]
    runs = re.findall(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', seg)
    print('=== %s ===' % p[:24])
    print('   run 文本: %s' % [r[:40] for r in runs])
    print()
