# -*- coding: utf-8 -*-
"""Editorial pass B1 (robust): delete redundant tables, convert dimmed sections to prose."""
import copy
from docx import Document
from docx.oxml.ns import qn
from docx.text.paragraph import Paragraph

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, text):
    runs = p.runs
    if not runs:
        p.add_run(text); return
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

def has(prefix):
    return any(p.text.strip().startswith(prefix) for p in doc.paragraphs)

def E(prefix, text):
    set_text(find(prefix), text)

def insert_after(anchor, texts):
    prev = anchor
    for t in texts:
        el = copy.deepcopy(anchor._element)
        prev._element.addnext(el)
        np = Paragraph(el, prev._parent)
        set_text(np, t)
        prev = np

def first_cells(t):
    return [c.text.strip() for c in t.rows[0].cells]

def drop_table_by_header(sig, ncols=None):
    for t in list(doc.tables):
        fc = first_cells(t)
        if fc[:len(sig)] == sig and (ncols is None or len(fc) == ncols):
            t._element.getparent().remove(t._element)
            print("  dropped table:", fc[:2])
            return True
    return False

def drop_para(prefix):
    if has(prefix):
        p = find(prefix); p._element.getparent().remove(p._element)
        print("  dropped caption:", prefix[:26])

# ---- B1-1 删除 5 张冗余表 ----
drop_table_by_header(["场景", "用户输入", "系统处理", "最终交付"])          # 表 5 典型场景
drop_table_by_header(["任务示例", "自治程度"], 4)                          # 表 7 三维决策示例
drop_table_by_header(["风险层级", "示例", "默认策略", "恢复与证据"])        # 表 9 风险分级
drop_table_by_header(["产品", "目标用户", "核心权益", "规划定价"])          # 表 13 产品版本
drop_table_by_header(["阶段", "产品目标", "研究目标", "市场目标"])          # 表 15 分阶段决策门
drop_table_by_header(["阶段", "学习任务", "实践任务", "可核验成长成果"])    # 表 18 人才培养路径
drop_para("表 5 典型场景的输入到交付闭环")
drop_para("表 7 三维决策示例")
drop_para("表 9 风险分级与控制策略")
drop_para("表 13 产品版本、定价与验证状态")
drop_para("表 15 分阶段研发与市场决策门")
drop_para("表 18 项目驱动的能力成长路径")
drop_para("表 3 目标用户与基准任务链")

# ---- B1-2 3.2 典型工作场景 -> 四个短案例 ----
if not has("项目汇报。用户在 Word 中"):
    h = find("3.2 典型体验场景")
    set_text(h, "3.2 典型工作场景")
    insert_after(h, [
      "项目汇报。用户在 Word 中写好材料标题后输入“按这些资料更新汇报口径”，系统识别文档结构与格式要求，"
      "调用检索、核验与写作流程，在目标文档中生成带来源标注与修改痕迹的内容，关键数字保留可追溯的证据链。",
      "资料核验。用户在文献或资料页面选中条目后输入“比较这些方法”，系统读取选中资料，建立证据矩阵，"
      "在向外读取更多内容前请求确认，最终输出论点、证据、来源与局限表。",
      "代码修复。用户在 IDE 中输入“修复这个报错并测试”，系统读取错误信息与相关文件，生成计划并执行测试，"
      "返回可审阅差异、测试结果与回滚点。",
      "沟通表达。用户在聊天框中输入“礼貌拒绝并给替代时间”，系统结合关系与语气生成候选回复，发送权仍由用户掌握。",
    ])

# ---- B1-3 5.3 风险分级改为正文 ----
if not has("项目按动作影响范围划分四档风险"):
    E("项目把两个与输入法入口强相关的风险作为专有指标管理",
      "项目按动作影响范围划分四档风险，并对应不同的控制策略。低风险包括补全、翻译与语气调整，由本地候选生成，"
      "用户可忽略，系统保留修改量记录；中风险包括读取选区与生成文档副本，系统显示读取范围并要求先预览，"
      "原件不被覆盖且操作可撤销；高风险包括写文件、执行命令与访问网络，必须经过独立审批，工具与路径受限定，"
      "并保留差异、日志与回滚点；极高风险包括发送消息、删除、支付与敏感数据外发，默认拒绝或要求二次确认，"
      "保留完整审计信息并具备人工处置通道。\n"
      "在此基础上，项目把两个与输入法入口强相关的风险作为专有指标管理。“禁止访问”与“读取过多”属于不同性质的指标："
      "禁止字段访问率针对明确标识的密码框、支付确认与金融敏感字段，属于设计红线，取值为 0；"
      "情境过度读取率针对普通工作情境中读取了非必需数据，属于统计指标，目标低于 5%。"
      "两者依据不同的判定规则分别报告。")

# ---- B1-4 8.3 路线改为五个阶段决策门 ----
if not has("路线分五个阶段推进"):
    h = find("8.3 二十四个月路线")
    set_text(h, "8.3 二十四个月路线与阶段决策门")
    insert_after(h, [
      "路线分五个阶段推进。前三个月完成问题验证，交付交互原型与情境边界定义，进入下一阶段的条件是需求重复出现且结果可验收。",
      "第四至六个月建成输入入口、意图胶囊与单执行器返回闭环，完成可用性与安全测试并校准验证阈值，"
      "进入下一阶段的条件是任务成功率达到 80% 且安全红线全部通过。",
      "第七至十二个月完成兼容矩阵与 Origin、Return 适配器标准化，完成三组对照实验与消融实验并标定路由阈值，"
      "在三个场景试点中验证连续四周留存与安全门槛。",
      "第十三至十八个月交付团队版与版本化成果契约接口，验证协同净收益与团队交付成本，形成首批付费团队。",
      "第十九至二十四个月进入规模化复制与工程加固，验证跨设备最小数据原则，形成机构合作、私有部署首单与场景复制能力。",
    ])

# ---- B1-5 9.2 人才培养改为一段正文 ----
if not has("项目按需求发现、产品试制、实验论证"):
    h = find("9.2 成员能力成长路径")
    set_text(h, "9.2 成员能力成长")
    insert_after(h, [
      "项目按需求发现、产品试制、实验论证、市场验证与成果传播五个环节推进，每个环节对应明确的学习任务、"
      "实践任务与可核验成果。需求发现阶段完成用户研究、创新方法与访谈观察，产出问题定义与研究材料；"
      "产品试制阶段完成系统设计与交互安全设计，产出原型、兼容性方案与权限设计记录；"
      "实验论证阶段完成实验设计、数据分析、对照消融与红队故障注入，产出评测报告与可复现实验；"
      "市场验证阶段完成商业模式、财务与合规训练，通过场景试点、报价与合作复盘产出意向与经营数据；"
      "成果传播阶段完成知识产权与科学表达训练，通过软件著作权、专利交底、论文与路演形成公开成果。"
      "全部成长成果以任务、代码、文档、测试与运营证据归档，成果归属与数据授权同步明确。",
    ])

# ---- B1-6 7.4 标题 ----
if has("7.4 单位经济模型"):
    E("7.4 单位经济模型", "7.4 产品版本与单位经济")

doc.save(SRC)
print("pass B1 done. tables:", len(doc.tables), "paragraphs:", len(doc.paragraphs))