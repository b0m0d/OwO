# -*- coding: utf-8 -*-
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v10.docx"
with zipfile.ZipFile(SRC) as z:
    xml = z.read('word/document.xml').decode('utf-8')

print('wp:extent literal count:', xml.count('<wp:extent'))
for m in re.finditer(r'<wp:extent[^>]*>', xml):
    print(repr(m.group(0)))
print()
print('a:ext literal count:', xml.count('<a:ext '))
for m in re.finditer(r'<a:ext [^>]*>', xml):
    print(repr(m.group(0)))
print()
print('drawing count:', xml.count('<w:drawing>'))
print('blip count:', xml.count('<a:blip'))
