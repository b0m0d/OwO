# -*- coding: utf-8 -*-
"""Build v19 = v18 + two wording fixes that the new table makes necessary.

  * 10.2 结尾补一句两年累计口径，与 10.3 的“两年累计亏损约 113 万元”对齐
  * 10.3 明确“约 113 万元”对应基准情景
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v18.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v19.docx"

TEXT_EDITS = [
    ('表9 的营业收入、经营成本与经营结果三组数字互为加减关系，可逐行核对。',
     '表9 的营业收入、经营成本与经营结果三组数字互为加减关系，可逐行核对：'
     '第一年基准情景收入 1.4 万元、成本 50 万元，净损益负 48.6 万元；'
     '前两年累计净损益保守情景负 72.5 万元、基准情景负 113.1 万元、进取情景负 160.4 万元。'),
]

# 10.3 末句跨 run，按段落整段重写
PARA_REWRITE = [
    ('基准情景下前两年累计亏损约 113 万元',
     '项目已有资源包括学校实验室与实验工位、团队成员个人设备、指导教师的方法与安全指导、'
     '开源模型与本地推理能力，这些不计入现金需求。项目未来两年预计新增资金需求 160 万元，'
     '其中第一年 50 万元、第二年 110 万元。资金来源以学校及竞赛创新基金、团队自筹与外部产业合作为主，'
     '具体比例随阶段融资落实情况调整。第一年资金用于把原型推进到可验证的试点闭环，'
     '并完成对照、消融与红队测试；第二年资金用于场景复制、团队版交付、兼容矩阵扩展与支持运维。'
     '资金按决策门分两批释放，任一批未达标即暂停后续投入。'
     '基准情景下前两年累计净损益为负 113.1 万元（保守情景负 72.5 万元、进取情景负 160.4 万元），'
     '160 万元额度可覆盖基准情景所需并保留安全边际。'),
]

PARA_RE = re.compile(r'<w:p(?: [^>]*)?>.*?</w:p>', re.S)
T_RE = re.compile(r'(<w:t(?: [^>]*)?>)([^<]*)(</w:t>)')


def set_para_text(para_xml, new_text):
    ppr = re.search(r'<w:pPr>.*?</w:pPr>', para_xml, re.S)
    rpr = re.search(r'<w:rPr>.*?</w:rPr>', para_xml, re.S)
    open_tag = re.match(r'<w:p(?: [^>]*)?>', para_xml).group(0)
    head = open_tag + (ppr.group(0) if ppr else '')
    run = '<w:r>%s<w:t xml:space="preserve">%s</w:t></w:r>' % (
        rpr.group(0) if rpr else '', new_text)
    return head + run + '</w:p>'


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')
    for old, new in TEXT_EDITS:
        n = xml.count(old)
        if n != 1:
            raise SystemExit('ABORT: 锚点命中 %d 次: %s' % (n, old[:40]))
        xml = xml.replace(old, new)
        print('  ✔ %s…' % old[:32])

    print('\n=== 段落级重写 ===')
    for anchor, new_text in PARA_REWRITE:
        # 锚点按“去空白”匹配：文档中的数字与中文之间由独立 run 提供空格
        flat_anchor = re.sub(r'\s+', '', anchor)
        hit = None
        for pm in PARA_RE.finditer(xml):
            ptext = ''.join(m.group(2) for m in T_RE.finditer(pm.group(0)))
            if re.sub(r'\s+', '', ptext).count(flat_anchor) == 1:
                hit = pm
                break
        if hit is None:
            raise SystemExit('ABORT: 段落锚点未找到: %s' % anchor[:30])
        xml = xml[:hit.start()] + set_para_text(hit.group(0), new_text) + xml[hit.end():]
        print('  ✔ %s…' % anchor[:30])

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
