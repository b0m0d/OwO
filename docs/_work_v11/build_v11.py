# -*- coding: utf-8 -*-
"""Build v11: replace figures + de-colloquialize "返回原处 / 原处 / 原地" wording."""
import json
import os
import re
import shutil
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v10.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v11.docx"
NEW_MEDIA = r"T:\创新创业\OwO-master\docs\_work_v11\new_media"

with open(os.path.join(NEW_MEDIA, '_extents.json'), encoding='utf-8') as f:
    EXTENTS = {r['slot']: (r['cx'], r['cy']) for r in json.load(f)}

# ---------------------------------------------------------------- text edits
# exact, whole-string replacements; each must hit exactly once
TEXT_EDITS = [
    ("将用户在任意输入场景中的自然语言请求转化为可验证、可撤销的智能体行动，并将结果及证据返回原工作位置",
     1,
     "将用户在任意输入场景中的自然语言请求转化为可验证、可撤销的智能体行动，并将结果及证据回填至需求产生的原工作位置"),

    ("并把可验证成果返回需求产生的原工作位置。",
     1,
     "并把可验证成果回填至需求产生的原工作位置。"),

    ("并以返回原处（Return to Origin）把成果送回需求产生的应用与位置。",
     1,
     "并以原位回填（Return to Origin）把成果归位到需求产生的应用与位置。"),

    ("版本化成果契约、返回原处机制与策略、验证、恢复体系",
     1,
     "版本化成果契约、原位回填机制与策略、验证、恢复体系"),

    ("Cuttle 从输入到成果返回原处的完整闭环",
     2,
     "Cuttle 从输入到成果原位回填的完整闭环"),

    ("并以受控执行把成果送回原处。",
     1,
     "并以受控执行把成果原位回填。"),

    ("用户可在原处直接审阅并继续编辑。",
     1,
     "用户可在原位直接审阅并继续编辑。"),

    # TOC row for 3.4 (page number is fixed positionally below)
    ('<w:t>3.4 返回原处机制</w:t></w:r></w:p></w:tc>',
     1,
     '<w:t>3.4 原位回填机制</w:t></w:r></w:p></w:tc>'),

    # section heading 3.4
    ("3.4 返回原处机制",
     1,
     "3.4 原位回填机制"),

    ("4.4 关键工程体系 返回原处与策略验证恢复",
     2,
     "4.4 关键工程体系 原位回填与策略验证恢复"),

    ("返回原处、策略控制、结果验证与失败恢复共同构成两项核心创新的工程支撑体系。",
     1,
     "原位回填、策略控制、结果验证与失败恢复共同构成两项核心创新的工程支撑体系。"),

    ("返回原处机制按原始应用、任务契约与动作风险",
     1,
     "原位回填机制按原始应用、任务契约与动作风险"),

    ("结果验证、返回原处、治理审计",
     1,
     "结果验证、原位回填、治理审计"),

    ("结果验证和原处返回作为统一产品链路",
     1,
     "结果验证和原位回填作为统一产品链路"),

    ("意图胶囊、返回原处、自治升级阈值与真实用户验证五个方向",
     1,
     "意图胶囊、原位回填、自治升级阈值与真实用户验证五个方向"),

    ("原地闭环完成率是首要指标",
     1,
     "原位回填完成率是首要指标"),

    ("并把成果送回用户原本工作的地方。",
     1,
     "并把成果回填至用户原本工作的位置。"),
]


def apply_text_edits(xml):
    report = []
    for old, expect, new in TEXT_EDITS:
        n = xml.count(old)
        if n != expect:
            raise SystemExit('ABORT: expected %d occurrence(s), found %d for:\n  %s' % (expect, n, old))
        xml = xml.replace(old, new)
        report.append((old, new, n))

    # Positional fix: the 3.4 TOC row page number (10 -> 11). The run format is
    # shared with other rows, so locate it by the preceding TOC entry text.
    anchor = '<w:t>3.4 原位回填机制</w:t></w:r></w:p></w:tc>'
    i = xml.find(anchor)
    if i < 0:
        raise SystemExit('ABORT: 3.4 TOC row not found for page-number fix')
    seg_end = i + 1500
    seg = xml[i:seg_end]
    m = re.search(r'(<w:rPr><w:rFonts w:ascii="Aptos" w:hAnsi="Aptos" w:eastAsia="Aptos"/>'
                  r'<w:b w:val="0"/><w:color w:val="666D73"/><w:sz w:val="15"/></w:rPr>'
                  r'<w:t>)(\d+)(</w:t>)', seg)
    if not m:
        raise SystemExit('ABORT: page-number run after the 3.4 TOC row not found')
    old_num = m.group(2)
    if old_num != '10':
        raise SystemExit('ABORT: 3.4 TOC page number changed unexpectedly: %s' % old_num)
    abs_start = i + m.start(2)
    xml = xml[:abs_start] + '11' + xml[abs_start + len(old_num):]
    report.append(('TOC 3.4 page number', '11', 1))
    return xml, report


def audit(xml):
    """After edits, report any residual colloquial wording."""
    residual = {}
    for bad in ('返回原处', '原处', '原地', '送回原处', '原地闭环'):
        c = xml.count(bad)
        if c:
            residual[bad] = c
    return residual


# ------------------------------------------------------------- extent edits
def apply_extents2(xml):
    """Replace the Nth (wp:extent + a:ext) pair in document order."""
    slot_order = ['image1.png', 'image2.png', 'image3.png', 'image4.png',
                  'image5.png', 'image6.png', 'image7.png', 'image8.png',
                  'image9.png', 'image10.png', 'image11.png', 'image12.png']
    pairs = list(re.finditer(r'<wp:extent cx="(\d+)" cy="(\d+)"\s*/?>', xml))
    inner = list(re.finditer(r'<a:ext cx="(\d+)" cy="(\d+)"\s*/?>', xml))
    assert len(pairs) == len(slot_order), 'unexpected extent count %d' % len(pairs)

    # rewrite from the back so earlier offsets stay valid
    edits = []  # (start, end, replacement)
    for i, (slot, pm) in enumerate(zip(slot_order, pairs)):
        if slot not in EXTENTS:
            continue
        cx, cy = EXTENTS[slot]
        edits.append((pm.start(), pm.end(), '<wp:extent cx="%d" cy="%d"/>' % (cx, cy)))

    # a:ext pairs appear in exactly the same document order as wp:extent
    aext_for_bodies = inner
    assert len(aext_for_bodies) == len(slot_order), 'a:ext count mismatch'
    for slot, am in zip(slot_order, aext_for_bodies):
        if slot not in EXTENTS:
            continue
        cx, cy = EXTENTS[slot]
        edits.append((am.start(), am.end(), '<a:ext cx="%d" cy="%d"/>' % (cx, cy)))

    edits.sort(key=lambda e: e[0], reverse=True)
    buf = xml
    for s, e, rep in edits:
        buf = buf[:s] + rep + buf[e:]
    return buf, len(edits)


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')
    xml, text_report = apply_text_edits(xml)
    residual = audit(xml)
    xml, n_ext = apply_extents2(xml)
    data['word/document.xml'] = xml.encode('utf-8')

    replaced = []
    for slot, (cx, cy) in EXTENTS.items():
        key = 'word/media/' + slot
        if key not in data:
            raise SystemExit('missing media slot ' + key)
        old_len = len(data[key])
        with open(os.path.join(NEW_MEDIA, slot), 'rb') as f:
            data[key] = f.read()
        replaced.append((slot, old_len, len(data[key])))

    if os.path.exists(DST):
        os.remove(DST)
    with zipfile.ZipFile(DST, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])

    print('=== text edits (%d) ===' % len(text_report))
    for old, new, cnt in text_report:
        print('  [x%d] %s' % (cnt, old[:52]))
        print('        -> %s' % new[:52])
    print('\n=== residual colloquial wording ===')
    print('  ' + ('none' if not residual else str(residual)))
    print('\n=== extents rewritten: %d ===' % n_ext)
    for slot, (cx, cy) in EXTENTS.items():
        print('  %-14s cx=%d cy=%d (%.2f x %.2f cm)' % (slot, cx, cy, cx / 360000, cy / 360000))
    print('\n=== media replaced: %d ===' % len(replaced))
    for slot, o, nw in replaced:
        print('  %-14s %8.1f KB -> %8.1f KB' % (slot, o / 1024.0, nw / 1024.0))
    print('\nOUTPUT: %s (%.2f MB)' % (DST, os.path.getsize(DST) / 1024.0 / 1024.0))


if __name__ == '__main__':
    main()
