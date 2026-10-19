# -*- coding: utf-8 -*-
"""Step 2: unify reference list format, add one conclusion box per chapter, group 表 2 rows."""
import copy, re
from docx import Document
from docx.text.paragraph import Paragraph
from docx.oxml.ns import qn

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, text):
    runs = p.runs
    if not runs:
        p.add_run(text); return
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

def set_cell(cell, text):
    ps = cell.paragraphs
    set_text(ps[0], text)
    for extra in ps[1:]:
        extra._element.getparent().remove(extra._element)

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

REFS = [
 "[1] 教育部. 关于举办中国国际大学生创新大赛（2026）的通知[Z]. 2026.",
 "[2] 中国国际大学生创新大赛组织委员会. 2025 年高教主赛道创意组评审规则[Z]. 2025.",
 "[3] 中国互联网络信息中心. 第 57 次中国互联网络发展状况统计报告[R]. 北京: 中国互联网络信息中心, 2026.",
 "[4] 教育部. 2025 年全国教育事业发展统计公报[R]. 北京: 教育部, 2026.",
 "[5] 工业和信息化部. 2025 年软件业运行情况[R]. 北京: 工业和信息化部, 2026.",
 "[6] 搜狗输入法. 版本升级日志[EB/OL]. https://pinyin.sogou.com/changelog.php.",
 "[7] Wispr Flow. Context Awareness[EB/OL]. https://docs.wisprflow.ai/articles/4678293671-Context-Awareness.",
 "[8] OpenAI. Introducing the Codex app[EB/OL]. https://openai.com/index/introducing-the-codex-app/.",
 "[9] 全国人民代表大会常务委员会. 中华人民共和国个人信息保护法[Z]. 2021.",
 "[10] 全国人民代表大会常务委员会. 中华人民共和国数据安全法[Z]. 2021.",
 "[11] 国家互联网信息办公室等. 生成式人工智能服务管理暂行办法[Z]. 2023.",
 "[12] Xiaomi. MiMo Desktop 产品文档[EB/OL]. https://mimo.mi.com/docs/en-US/news/latest/mimo-desktop.",
 "[13] YAO S, ZHAO J, YU D, et al. ReAct: Synergizing Reasoning and Acting in Language Models[C]. ICLR, 2023.",
 "[14] SCHICK T, DWIVEDI-YU J, DESSI R, et al. Toolformer: Language Models Can Teach Themselves to Use Tools[C]. NeurIPS, 2023.",
 "[15] SHINN N, CASSANO F, GOPINATH A, et al. Reflexion: Language Agents with Verbal Reinforcement Learning[C]. NeurIPS, 2023.",
 "[16] WU Q, BANSAL G, ZHANG J, et al. AutoGen: Enabling Next-Gen LLM Applications via Multi-Agent Conversation[R]. arXiv:2308.08155, 2023.",
 "[17] HONG S, ZHUGE M, CHEN J, et al. MetaGPT: Meta Programming for a Multi-Agent Collaborative Framework[C]. ICLR, 2024.",
 "[18] JIMENEZ C E, YANG J, WETTIG A, et al. SWE-bench: Can Language Models Resolve Real-World GitHub Issues?[C]. ICLR, 2024.",
 "[19] ZHOU S, XU F F, ZHU H, et al. WebArena: A Realistic Web Environment for Building Autonomous Agents[C]. ICLR, 2024.",
 "[20] XIE T, ZHANG D, CHEN J, et al. OSWorld: Benchmarking Multimodal Agents for Open-Ended Tasks in Real Computer Environments[C]. NeurIPS, 2024.",
 "[21] DOURISH P. What We Talk About When We Talk About Context[J]. Personal and Ubiquitous Computing, 2004, 8(1): 19-30.",
 "[22] DEY A K. Understanding and Using Context[J]. Personal and Ubiquitous Computing, 2001, 5(1): 4-7.",
 "[23] HORVITZ E. Principles of Mixed-Initiative User Interfaces[C]. Proceedings of CHI, 1999: 159-166.",
 "[24] AMERSHI S, WELD D, VORVOREANU M, et al. Guidelines for Human-AI Interaction[C]. Proceedings of CHI, 2019: 1-13.",
 "[25] LEE J D, SEE K A. Trust in Automation: Designing for Appropriate Reliance[J]. Human Factors, 2004, 46(1): 50-80.",
 "[26] PARASURAMAN R, RILEY V. Humans and Automation: Use, Misuse, Disuse, Abuse[J]. Human Factors, 1997, 39(2): 230-253.",
 "[27] Microsoft. Text Services Framework 与 UI Automation 架构文档[EB/OL]. Microsoft Learn, 2025.",
 "[28] NIST. Artificial Intelligence Risk Management Framework (AI RMF 1.0)[R]. NIST AI 100-1, 2023.",
 "[29] ISO/IEC. 25010:2011 Systems and Software Quality Requirements and Evaluation (SQuaRE)[S]. Geneva: ISO, 2011.",
 "[30] SHNEIDERMAN B. Human-Centered AI[M]. Oxford: Oxford University Press, 2022.",
]
ref_idx = 0
for p in doc.paragraphs:
    if re.match(r"^\[\d+\]\s", p.text.strip()) and ref_idx < len(REFS):
        set_text(p, REFS[ref_idx])
        ref_idx += 1
print("  references reformatted:", ref_idx)

# ---------- 表 2 增加分组列 ----------
for t in doc.tables:
    fc = [c.text.strip() for c in t.rows[0].cells]
    if fc[:2] == ["验证方向", "验证方法"]:
        groups = {
            "入口机制": "用户价值", "意图胶囊": "核心机制", "三维任务决策": "核心机制",
            "可见审批": "安全可靠", "端到端任务": "安全可靠",
            "团队与机构": "商业验证", "纵向留存": "商业验证",
        }
        # 在第一列前插入分组：把分组写进第一列行首
        for row in t.rows[1:]:
            key = row.cells[0].text.strip()
            tag = None
            for k, v in groups.items():
                if key.startswith(k):
                    tag = v; break
            if tag:
                set_cell(row.cells[0], f"【{tag}】{key}")
        # 表头首列加说明
        set_cell(t.rows[0].cells[0], "验证方向（分组）")
        print("  表 2 分组标注完成")
        break

# ---------- 每章一个核心结论框 ----------
CONCLUSIONS = {
 "项目摘要": None,
 "第一章 真实问题与项目机会": "核心结论：被消耗的不是模型能力，而是意图产生位置与任务承接位置之间的距离。输入时刻是同时具备显式意图与原位置锚点的少数切入点。",
 "第二章 用户需求与验证设计": "核心结论：项目只验证两条任务链与四项机制假设，全部门槛在实验前预设，未达标即按预设方案收缩范围。",
 "第三章 产品方案与体验闭环": "核心结论：普通输入与智能体链路架构分离，只有用户主动发起才进入智能体流程；首阶段以 Windows 与三类高频应用为验证范围。",
 "第四章 核心技术与创新机制": "核心创新：不是更多的 Agent，而是让每一次自治升级都有情境边界、权限边界与返回锚点。",
 "第五章 安全伦理与数据治理": "核心结论：安全边界写入产品结构而非依赖约定——敏感字段访问为 0，高风险动作独立审批，资金类操作不纳入可执行范围。",
 "第六章 市场空间与竞争定位": "核心结论：首年规模由服务承载能力决定，不按市场规模推算；长期空间以公开行业数据作参照，随真实转化数据校准。",
 "第七章 商业模式与市场进入": "核心结论：收入来自个人订阅、团队许可与私有部署三种模式，个人版积累的工作流是团队许可的主要入口。",
 "第八章 研发验证与实施路径": "核心结论：每个阶段以决策门结算，达标才进入下一阶段；团队已具备运行时、权限、审计与回滚的工程基础。",
 "第九章 团队协作与人才培养": "核心结论：核心研发、产品决策与成果表达由学生团队承担，指导教师负责方法、安全合规与阶段评审。",
 "第十章 财务规划与资源配置": "核心结论：第一年以验证为主，收入规模受验证容量约束；基准情景第三年进入盈亏平衡上方，两年资金需求 160 万元。",
 "第十一章 风险管理": "核心结论：一级风险由发布门槛硬性控制，二级风险以性能降级与差异化建设应对，风险状态每两周随产品评审更新。",
 "第十二章 社会价值与发展愿景": "核心结论：项目的价值在于让知识工作者减少无意义搬运，同时把权限、证据与撤销能力留在用户手中。",
}
def model_p():
    for p in doc.paragraphs:
        if p.style.name == "Normal" and len(p.text) > 60:
            return p

added = 0
for p in list(doc.paragraphs):
    t = p.text.strip()
    if t in CONCLUSIONS and CONCLUSIONS[t]:
        nxt = p._element.getnext()
        if nxt is not None and nxt.tag.endswith('}p'):
            np = Paragraph(nxt, p._parent)
            if np.text.strip().startswith("核心结论"):
                continue
        el = copy.deepcopy(model_p()._element)
        p._element.addnext(el)
        box = Paragraph(el, p._parent)
        set_text(box, CONCLUSIONS[t])
        added += 1
print("  conclusion boxes added:", added)

doc.save(SRC)
print("step 2 done")