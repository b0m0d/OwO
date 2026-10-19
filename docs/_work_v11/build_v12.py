# -*- coding: utf-8 -*-
"""Build v12 from v11: rename the mechanism term 原位回填 -> 原位交付."""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v11.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v12.docx"

REPL = '原位交付'

# (old, expected_count, new)
EDITS = [
    ('并以原位回填（Return to Origin）把成果归位到需求产生的应用与位置。',
     1,
     '并以原位交付（Return to Origin）把成果归位到需求产生的应用与位置。'),

    ('版本化成果契约、原位回填机制与策略、验证、恢复体系',
     1,
     '版本化成果契约、原位交付机制与策略、验证、恢复体系'),

    ('Cuttle 从输入到成果原位回填的完整闭环',
     2,
     'Cuttle 从输入到成果原位交付的完整闭环'),

    ('并以受控执行把成果原位回填。',
     1,
     '并以受控执行完成成果原位交付。'),

    ('3.4 原位回填机制',
     2,   # TOC row + section heading
     '3.4 原位交付机制'),

    ('4.4 关键工程体系 原位回填与策略验证恢复',
     2,   # TOC row + section heading
     '4.4 关键工程体系 原位交付与策略验证恢复'),

    ('原位回填、策略控制、结果验证与失败恢复共同构成两项核心创新的工程支撑体系。',
     1,
     '原位交付、策略控制、结果验证与失败恢复共同构成两项核心创新的工程支撑体系。'),

    ('原位回填机制按原始应用、任务契约与动作风险',
     1,
     '原位交付机制按原始应用、任务契约与动作风险'),

    ('结果验证、原位回填、治理审计',
     1,
     '结果验证、原位交付、治理审计'),

    ('结果验证和原位回填作为统一产品链路',
     1,
     '结果验证和原位交付作为统一产品链路'),

    ('意图胶囊、原位回填、自治升级阈值与真实用户验证五个方向',
     1,
     '意图胶囊、原位交付、自治升级阈值与真实用户验证五个方向'),

    ('原位回填完成率是首要指标',
     1,
     '原位交付完成率是首要指标'),
]


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')
    report = []
    for old, expect, new in EDITS:
        n = xml.count(old)
        if n != expect:
            raise SystemExit('ABORT: expected %d, found %d for:\n  %s' % (expect, n, old))
        xml = xml.replace(old, new)
        report.append((old, new, n))

    # safety: every remaining 原位回填 must be gone, and nothing else changed
    left = xml.count('原位回填')
    if left:
        raise SystemExit('ABORT: %d residual 原位回填 occurrence(s)' % left)

    data['word/document.xml'] = xml.encode('utf-8')

    if os.path.exists(DST):
        os.remove(DST)
    with zipfile.ZipFile(DST, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])

    print('=== 原位回填 -> 原位交付 (%d edits) ===' % len(report))
    for old, new, n in report:
        print('  [x%d] %s' % (n, old[:50]))
        print('        -> %s' % new[:50])
    print('\nresidual 原位回填: %d' % xml.count('原位回填'))
    print('total 原位交付 occurrences: %d' % xml.count('原位交付'))
    print('\nOUTPUT: %s (%.2f MB)' % (DST, os.path.getsize(DST) / 1024.0 / 1024.0))


if __name__ == '__main__':
    main()
