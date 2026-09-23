import docx, re
from docx.oxml.ns import qn
p=r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v6.docx"
d=docx.Document(p)
body=d.element.body
lines=[];pi=0;ti=0
seq=[]
for child in body.iterchildren():
    tag=child.tag.split('}')[1]
    if tag=='p':
        txt=''.join(n.text or '' for n in child.iter(qn('w:t')))
        if txt.strip():
            lines.append(f'P{pi} :: {txt}')
            m=re.match(r'^(表|图) (\d+)', txt.strip())
            if m: seq.append((m.group(1), int(m.group(2)), txt.strip()[:30]))
        pi+=1
    elif tag=='tbl':
        lines.append(f'@@@ TABLE {ti}')
        ti+=1
open(r"T:\创新创业\OwO-master\docs\.qa_v5\v6_paras2.txt","w",encoding="utf-8").write("\n".join(lines))
print("== 图表编号顺序 ==")
for s in seq: print(s)
