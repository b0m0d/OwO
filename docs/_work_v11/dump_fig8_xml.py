# -*- coding: utf-8 -*-
"""Dump raw XML around the 图8 caption paragraph."""
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v12.docx"
with zipfile.ZipFile(SRC) as z:
    xml = z.read('word/document.xml').decode('utf-8')

i = xml.find('图8')
print('first 图8 at', i)
print(repr(xml[i - 700:i + 400]))
