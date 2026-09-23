import docx, re
from docx.oxml.ns import qn
p=r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v6.docx"
d=docx.Document(p)
body=d.element.body
lines=[];pi=0;ti=0
for child in body.iterchildren():
    tag=child.tag.split('}')[1]
    if tag=='p':
        txt=''.join(n.text or '' for n in child.iter(qn('w:t')))
        if txt.strip():
            lines.append(f'P{pi} :: {txt}')
        pi+=1
    elif tag=='tbl':
        lines.append(f'@@@ TABLE {ti}')
        ti+=1
open(r"T:\创新创业\OwO-master\docs\.qa_v5\v6_paras.txt","w",encoding="utf-8").write("\n".join(lines))
# 表格
tl=[]
for i,t in enumerate(d.tables):
    tl.append(f'=== TABLE {i} rows={len(t.rows)} cols={len(t.columns)} ===')
    for r in t.rows:
        tl.append(' | '.join(c.text.strip().replace("\n","/") for c in r.cells))
open(r"T:\创新创业\OwO-master\docs\.qa_v5\v6_tables.txt","w",encoding="utf-8").write("\n".join(tl))
print('ok')
