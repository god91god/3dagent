// ============================================================
// 记忆系统 Prompt 模板 —— 借鉴自 Project N.E.K.O. (Apache-2.0)
// 源文件: N.E.K.O-main/config/prompts/prompts_memory.py
// 原作者: Project N.E.K.O. Team (Copyright 2025-2026)
// 许可: Apache License 2.0 —— 本文件保留上游版权声明
// 说明: 只取 zh 版模板，占位符适配本项目的 LANLAN_NAME=优香 / MASTER_NAME=主人
// L1 只用 FACT_EXTRACTION_PROMPT；其余 L2 用，先放着避免重复搬运
#![allow(dead_code)]
// ============================================================

/// 事实提取 prompt（N.E.K.O. get_fact_extraction_prompt）
/// 占位符: {LANLAN_NAME} {MASTER_NAME} {CONVERSATION}
pub const FACT_EXTRACTION_PROMPT: &str = r#"从以下对话中提取关于 {LANLAN_NAME} 和 {MASTER_NAME} 的重要事实信息。

要求：
- 只提取重要且明确的事实（偏好、习惯、身份、关系动态等）
- 忽略闲聊、寒暄、模糊的内容
- 忽略AI幻觉、胡言乱语(gibberish)、无意义的编造内容，只提取对话中有真实依据的事实
- 每条事实必须是一个独立的原子陈述
- entity 标注为 "master"(关于{MASTER_NAME})、"neko"(关于{LANLAN_NAME})或 "relationship"(关于两人关系)

importance 评分 1-10，评分指引（请按此打分，不要泛泛都打 7）：
- **10**：关键长期信息——姓名、昵称、生日、身份、核心关系节点；用户明确表示"请{LANLAN_NAME}记住 X" / "这个你一定要记得"；或者 {LANLAN_NAME} 自己特别希望记住的重要相处细节。这些会被快速沉淀为长期记忆。
- **8-9**：长期稳定的核心偏好 / 固定习惯（不是一时兴起）
- **6-7**：普通偏好、日常习惯、近期动态
- **5**：次要但有记录价值的观察
- **1-4**：弱相关或不确定的线索（仍请返回，下游按场景过滤；不要在此处预先丢弃）

event_when（可选 — 事件发生时间，一律用相对时间，绝不写绝对日期）：
- 如果事实里提到具体时间线索（"昨天"、"上周一"、"三月份"、"今早"），用 event_when 标注
- 格式 {"start": {"offset": <整数>, "unit": "<单位>"}, "end": {"offset": <整数>, "unit": "<单位>"}}
- offset 负值=过去、0=当下、正值=未来；unit ∈ minute | hour | day | week | month | year
- **粒度可以粗，不要求精确**——"几天前"→ day、"上周"→ week、"几个月前"→ month 即可，不必精确到 minute/hour（没有具体数字的话，可以根据上下文猜测一个数字）
- 没有时间线索就直接省略 event_when 字段，或写 null
- 例 1：用户说"昨天晚上没睡好" → event_when = {"start": {"offset": -1, "unit": "day"}, "end": null}
- 例 2：用户说"喜欢喝咖啡"（长期偏好，无时间） → 不写 event_when

======以下为对话======
{CONVERSATION}
======以上为对话======

请以 JSON 数组格式返回（如果没有值得提取的事实，返回空数组 []）：
[
  {"text": "事实描述", "importance": 7, "entity": "master", "event_when": null},
  ...
]"#;

/// 信号检测 prompt（N.E.K.O. get_signal_detection_prompt）—— L2 使用
/// 占位符: {NEW_FACTS} {EXISTING_OBSERVATIONS}
pub const SIGNAL_DETECTION_PROMPT: &str = r#"你是一个记忆关系判定专家。给你一组新提取的事实，和一组系统已经记录过的观察，请判断每条新事实对已有观察的关系。

======以下为新提取的事实======
{NEW_FACTS}
======以上为新事实======

======以下为已有观察（按 type.entity.id 索引）======
{EXISTING_OBSERVATIONS}
======以上为已有观察======

请对每条新事实判断：
- reinforces：是否加强了某条已有观察？返回 target_id 和理由
- negates：是否反驳了某条已有观察？返回 target_id 和理由
- 若都没有，对应新事实没有 signal —— 不写进 signals 数组即可

target_id 必须来自上面"已有观察"区，不要凭空生成；若某条新事实与多条已有观察相关，可返回多条 signal。

输出 JSON（如果没有匹配任何已有观察，返回 {"signals": []}）：
{
  "signals": [
    {"source_fact_id": "fact_xxx",
     "target_type": "reflection",
     "target_id": "r_xxx",
     "signal": "reinforces",
     "reason": "简短理由"},
    ...
  ]
}"#;

/// 反思五步法 prompt（N.E.K.O. get_reflection_prompt）—— L2 使用
/// 占位符: {LANLAN_NAME} {MASTER_NAME} {RELATED_CONTEXT_BLOCK} {FACTS}
pub const REFLECTION_PROMPT: &str = r#"以下是关于 {LANLAN_NAME} 和 {MASTER_NAME} 的一系列已提取事实：

{RELATED_CONTEXT_BLOCK}======以下为事实======
{FACTS}
======以上为事实======

请基于这些事实，提炼一条高层次的反思洞察。请按以下五步思考：

第一步：判断该反思主要关于谁（entity）
- "master": 主要关于 {MASTER_NAME} 的个人特征
- "neko": 主要关于 {LANLAN_NAME} 的自我认知
- "relationship": 关于两人之间的关系动态

第二步：选定语义类别 relation_type（必须与 entity 匹配）
- master 可用: preference(偏好) | trait(性格) | habit(习惯) | identity(身份) | emotional(情感) | boundary(边界)
- neko 可用: self_awareness(自我认知) | learned(习得行为) | role_note(角色备注)
- relationship 可用: dynamic(互动模式) | milestone(里程碑) | tension(摩擦) | shared_memory(共同记忆) | agreement(约定)

第三步：围绕已选定的 entity / relation_type 撰写 reflection 文本
要求：
- 紧扣单一观察或模式，不要罗列事实，也不要把多个无关事实混在一起
- 简洁清晰，不得超过 150 字
- **不要在 reflection 文本里使用"今天/刚刚/最近/这周/近期"等相对时间词** —— 具体时间靠 event_when 字段记录，文本保持中性叙事（例如"某次"、"那段时间"、"当时"）

第四步：判定时间属性 temporal_scope（三档之一，反映"是否会过期"）
- "pattern": 持续模式 / 性格特质 / 长期偏好，永不过期。例：「{MASTER_NAME} 喜欢咖啡」「{LANLAN_NAME} 性格内向」「两人长期互相依赖」。
- "state": 当前持续的情境，几周内自然过期。例：「{MASTER_NAME} 最近工作压力大」「{LANLAN_NAME} 这段时间在适应新角色」。
- "episode": 一次具体事件，几天内过期。例：「{MASTER_NAME} 昨晚通宵改代码」「{LANLAN_NAME} 今天收到一份礼物」。
- 拿不准时请倾向选 pattern（误判 pattern 当 state / episode 会让长期特征过早淡出，比反过来更危险）。

第五步：标注事件时间 event_when（一律使用相对时间，禁止绝对日期）
- 格式：{"start": {"offset": <整数>, "unit": "<单位>"}, "end": {"offset": <整数>, "unit": "<单位>"}}
- offset 负值=过去、0=当下、正值=未来；unit 必须是 minute | hour | day | week | month | year 之一
- start = 事件起点；end = 事件终点（pattern 类通常可省略 end，写 null）
- **粒度可以粗，不要求精确**——"前几天"用 `{"offset": -3, "unit": "day"}`、"上周"用 `{"offset": -1, "unit": "week"}`、"几个月前"用 month 即可；不要追求精确到小时分钟（没有具体数字的话，可以根据上下文猜测一个数字）
- 若事实里完全没有时间线索（连"近期"这样的暗示也没有），整段 event_when 写 null（系统会兜底为创建时刻）
- 例 1：事实中"上周一去爬山" → {"start": {"offset": -1, "unit": "week"}, "end": {"offset": -1, "unit": "week"}}
- 例 2：事实中"今天感冒了" → {"start": {"offset": 0, "unit": "day"}, "end": null}
- 例 3：长期"喜欢咖啡"（pattern） → null

请以 JSON 格式返回，字段顺序保持如下：
{"entity": "master/neko/relationship", "relation_type": "preference", "reflection": "你的反思洞察", "temporal_scope": "pattern", "event_when": null}"#;

/// 事实去重仲裁 prompt（N.E.K.O. get_fact_dedup_prompt）—— L2 使用
/// 占位符: {COUNT} {PAIRS}
pub const FACT_DEDUP_PROMPT: &str = r#"以下是 {COUNT} 组由相似度筛选出的候选事实对，请逐组判断是否真的指向同一件事，并选择处理方式。

======以下为候选事实对======
{PAIRS}
======以上为候选事实对======

对于每组，从下列动作中选一个：
- merge: 两条记录的确指向同一事件/偏好/状态，保留 existing，丢弃 candidate（existing 的 importance 会自动+1，candidate id 会被记入 merged_from_ids）
- replace: 同样指向同一件事，但 candidate 措辞更准确/更新，应保留 candidate、丢弃 existing
- keep_both: 看似相似但其实是两件不同的事（如"喜欢"与"讨厌"，或同一对象在不同情境下的不同状态），都保留

注意：
- 分数高只说明表层相似，不代表语义相同，特别要警惕褒贬相反、肯定/否定相反的情况
- 优先选 keep_both 而非误合并；记忆系统对错误合并的容忍度低于对冗余的容忍度

仅输出 JSON 数组，每项包含 index、action：
[{"index": 0, "action": "merge"}, {"index": 1, "action": "keep_both"}]"#;

/// 记忆召回重排 prompt（N.E.K.O. get_memory_recall_rerank_prompt）—— L2 使用
/// 占位符: {QUERY} {CANDIDATES} {BUDGET}
pub const MEMORY_RECALL_RERANK_PROMPT: &str = r#"以下是用户最近提到的话题。请从候选记忆中挑选最相关的 {BUDGET} 条用于注入对话上下文。

======以下为用户当前话题======
{QUERY}
======以上为用户当前话题======

======以下为候选记忆======
{CANDIDATES}
======以上为候选记忆======

每条候选前的 score 是用户对该记忆的累计确认度（高 = 反复确认，低 = 较少证据）。可作为辅助信号——同等相关度时优先选 score 高的；但不要让 score 完全压倒相关性，无关的高 score 记忆不该入选。

仅输出 JSON 数组，按重要程度从高到低排列，每项包含 id 字段：
[{"id": "persona.master.xxx"}, {"id": "reflection.ref_yyy"}]

最多 {BUDGET} 条；若候选不足 {BUDGET} 条相关，可返回更少。"#;

/// 未收尾话题检测 prompt（N.E.K.O. OPEN_THREADS_PROMPTS["zh"]）—— 语义版追问系统
/// 源文件: N.E.K.O-main/config/prompts/prompts_activity.py
/// 占位符: {CONVERSATION}
/// 用途: LLM 回顾最近对话，识别"被提起但还没收尾"的话题（AI 答应没做的事 /
///       用户话说一半被打断 / 双重需求只接一边），注入主动搭话 prompt 让优香续聊
pub const OPEN_THREADS_PROMPT: &str = r#"你是对话回顾助手。看下面最近的对话，识别"被提起但还没收尾"的话题——比如 AI 答应过但还没做的事、用户说一半被打断没说完的事、用户讲到一半的故事或心情没说到结局。

======以下为最近对话（按时间顺序）======
{CONVERSATION}
======以上为最近对话（按时间顺序）======

输出严格的 JSON（不带 markdown 代码块）：
{"open_threads": ["短句 1"]}

**默认应返回空数组**。绝大多数对话都自然收尾、没有悬而未决——这种情况下严格返回 {"open_threads": []}。只有当你能明确指出"谁挂了什么、对方还在等"时才报告，至多 3 条；正常情况预期是 0 条，偶尔 1 条，2-3 条很罕见。宁可漏报也不要凑数。

算 hanging（应报告）：
- 用户说"那个 bug 啊……"被打断，之后没回到这个话题
- 用户讲到一半的故事或心情停在悬念上，没说到结局，AI 也没追问后续
- 用户同时表达了两个并列的需求 / 矛盾的心情，AI 只接住其中一边，另一边没人回应

不算 hanging（应忽略）：
- 自然的话题切换、对方主动结束某个话题
- 闲聊里的随口一提、寒暄性的"下次再说"
- 长期话题（早就在聊，不是这段对话新起的悬念）

示例 A——对话顺利结束、互道晚安 → {"open_threads": []}
示例 B——用户的另一半诉求被晾在一边 → {"open_threads": ["用户说想吃顿好的又想减肥，AI 只顺着减肥那条线接了下去——'吃点好的'被晾在一边没人回应"]}"#;

/// 记忆注入的 header 文案
pub const MEMORY_HEADER: &str = "【长期记忆】";
/// 回忆渲染条目标签映射（entity → 中文标签）
pub fn entity_label(entity: &str) -> &'static str {
    match entity {
        "master" => "关于主人",
        "neko" => "关于优香",
        "relationship" => "关系",
        _ => "记忆",
    }
}

/// 相对时间标签（天/周/月），格式 "N 天前"
pub fn time_since_label(days: f64) -> String {
    if days < 1.0 {
        return "今天".to_string();
    }
    if days < 7.0 {
        return format!("{} 天前", days.round() as i64);
    }
    if days < 30.0 {
        return format!("{} 周前", (days / 7.0).round() as i64);
    }
    if days < 365.0 {
        return format!("{} 个月前", (days / 30.0).round() as i64);
    }
    format!("{} 年前", (days / 365.0).round() as i64)
}
