// ============================================================
// 话题深池（Topic Hook Pool）—— 借鉴 N.E.K.O. main_logic/topic/
// 源文件:
//   - N.E.K.O-main/main_logic/topic/pipeline.py      (TopicHookPool)
//   - N.E.K.O-main/main_logic/topic/signals.py       (TopicSignalStore)
//   - N.E.K.O-main/main_logic/topic/delivery.py      (投递模板)
//   - N.E.K.O-main/config/prompts/prompts_activity.py (TOPIC_CANDIDATE_PROMPTS)
// 原作者: Project N.E.K.O. Team (Copyright 2025-2026)
// 许可: Apache License 2.0 —— 本文件保留上游版权声明
//
// 单用户简化版（相对上游改动）：
//   - 信号层：用户/AI 轮次入池（60 轮 / 12h 保留），≥8 条有意义用户轮才 ready
//   - 分析层：后台 LLM 从信号里提取 1-2 个深话题（relevance≥70 && risk≤65）
//   - 投递层：每日配额 2（weight 制，未回应 1/3，回应 1.0）+ 4h 最小间隔
//             + 48h 去重（keyword 交集 + bigram 相似度兜底）
//   - 反馈层：投递后 10min 窗口内用户回复 → weight 升级 1.0
//   - 砍掉：联网 enrich / deep search / 多角色（单用户）
// ============================================================

use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

// ── 常量（抄 N.E.K.O. pipeline.py / signals.py）──

/// 话题投递最小间隔（秒）—— _MIN_TOPIC_TRIGGER_GAP_SECONDS = 4h
pub const MIN_TOPIC_TRIGGER_GAP_SECONDS: f64 = 4.0 * 3600.0;
/// 每日话题配额（weight 制）—— _MAX_DAILY_TOPIC_TRIGGERS = 2
pub const MAX_DAILY_TOPIC_TRIGGERS: f64 = 2.0;
/// 已用话题保留窗口（秒）—— _USED_TOPIC_RECENT_SECONDS = 48h
pub const USED_TOPIC_RECENT_SECONDS: f64 = 48.0 * 3600.0;
/// 未回应投递的配额权重 —— _UNANSWERED_TOPIC_WEIGHT = 1/3
pub const UNANSWERED_TOPIC_WEIGHT: f64 = 1.0 / 3.0;
/// 回应窗口（秒）—— _TOPIC_RESPONSE_WINDOW_SECONDS = 10min
pub const TOPIC_RESPONSE_WINDOW_SECONDS: f64 = 10.0 * 60.0;
/// 信号成熟期（秒）：最后一轮对话后至少等这么久才分析 —— _CANDIDATE_MATURE_SECONDS
pub const CANDIDATE_MATURE_SECONDS: f64 = 60.0;
/// 信号窗口轮数上限 —— _MAX_GLOBAL_TURNS = 60
pub const MAX_SIGNAL_TURNS: usize = 60;
/// 信号保留时长 —— _SIGNAL_RETENTION_SECONDS = 12h
pub const SIGNAL_RETENTION_SECONDS: f64 = 12.0 * 3600.0;
/// 分析门槛：有意义用户轮次数 —— min_user_turns_for_topic = 8
pub const MIN_USER_TURNS_FOR_TOPIC: usize = 8;
/// 素材采纳门槛（抄 N.E.K.O. _material_is_ready）
pub const TOPIC_RELEVANCE_MIN: i32 = 70;
pub const TOPIC_RISK_MAX: i32 = 65;
/// 两次后台分析的最小间隔（秒）：分析过的信号窗口不重复烧 LLM
pub const ANALYSIS_COOLDOWN_SECONDS: f64 = 10.0 * 60.0;

/// 语气词/寒暄（不算有意义信号，抄 signals.py _FILLER_TEXTS）
const FILLER_TEXTS: &[&str] = &[
    "你好", "啊", "嗯", "哦", "好", "可以", "对", "對", "行", "行吧", "哈哈", "没事", "沒事", "不知道",
];

// ── 数据结构 ──

/// 一条信号（对话轮次）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TopicTurnSignal {
    pub actor: String, // "user" | "ai"
    pub text: String,
    pub timestamp: f64,
}

/// 一个话题素材（LLM 提取的深话题）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TopicMaterial {
    pub hook_id: String,
    pub interest: String,
    pub keywords: Vec<String>,
    pub relevance: i32,
    pub risk: i32,
    pub status: String, // "pending" | "used"
    pub created_at: f64,
}

/// 已用话题记录（配额 + 去重 + 回应窗口）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsedTopicRecord {
    pub used_at: f64,
    pub weight: f64,
    pub interest: String,
    pub keywords: Vec<String>,
    /// 回应窗口截止时间（投递后 10min 内用户回复 → weight 升 1.0）
    #[serde(default)]
    pub response_deadline: Option<f64>,
}

/// 话题深池（单用户，所有字段 Mutex 供后台任务共享）
pub struct TopicPool {
    dir: PathBuf,
    /// 信号窗口（按时间序）
    signals: Mutex<Vec<TopicTurnSignal>>,
    /// 待投递素材（pending）
    materials: Mutex<Vec<TopicMaterial>>,
    /// 已用话题（配额/去重）
    used_topics: Mutex<Vec<UsedTopicRecord>>,
    /// 有新信号待分析
    dirty: Mutex<bool>,
    /// 上次分析完成时间（冷却用，避免重复烧 LLM）
    last_analysis_at: Mutex<f64>,
    /// 写锁（落盘串行化）
    write_lock: Mutex<()>,
}

impl TopicPool {
    pub fn new(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).ok();
        let pool = TopicPool {
            dir,
            signals: Mutex::new(Vec::new()),
            materials: Mutex::new(Vec::new()),
            used_topics: Mutex::new(Vec::new()),
            dirty: Mutex::new(false),
            last_analysis_at: Mutex::new(0.0),
            write_lock: Mutex::new(()),
        };
        pool.load_persisted();
        pool
    }

    // ---------- 文件路径 ----------

    fn signals_path(&self) -> PathBuf {
        self.dir.join("topic_signals.json")
    }
    fn materials_path(&self) -> PathBuf {
        self.dir.join("topic_materials.json")
    }
    fn used_path(&self) -> PathBuf {
        self.dir.join("topic_used.json")
    }

    fn atomic_write(&self, path: &std::path::Path, content: &str) -> Result<(), String> {
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("创建临时文件失败: {}", e))?;
        use std::io::Write;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("写临时文件失败: {}", e))?;
        f.flush().ok();
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("替换文件失败: {}", e))
    }

    fn load_persisted(&self) {
        // 信号
        if let Ok(content) = std::fs::read_to_string(self.signals_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&content) {
                if let Some(last) = v.get("last_analysis_at").and_then(|x| x.as_f64()) {
                    *self.last_analysis_at.lock().unwrap() = last;
                }
                if let Some(turns) = v.get("turns").and_then(|x| x.as_array()) {
                    let mut signals: Vec<TopicTurnSignal> = turns
                        .iter()
                        .filter_map(|x| serde_json::from_value(x.clone()).ok())
                        .collect();
                    // 保留期修剪
                    let now = now_secs();
                    signals.retain(|t| now - t.timestamp <= SIGNAL_RETENTION_SECONDS);
                    if signals.len() > MAX_SIGNAL_TURNS {
                        signals.drain(0..signals.len() - MAX_SIGNAL_TURNS);
                    }
                    *self.signals.lock().unwrap() = signals;
                }
            }
        }
        // 素材（pending 跨实例持久化：分析任务写入 → 投递任务读取）
        if let Ok(content) = std::fs::read_to_string(self.materials_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&content) {
                if let Some(arr) = v.get("materials").and_then(|x| x.as_array()) {
                    let materials: Vec<TopicMaterial> = arr
                        .iter()
                        .filter_map(|x| serde_json::from_value(x.clone()).ok())
                        .collect();
                    *self.materials.lock().unwrap() = materials;
                }
            }
        }
        // 已用话题
        if let Ok(content) = std::fs::read_to_string(self.used_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&content) {
                if let Some(records) = v.get("records").and_then(|x| x.as_array()) {
                    let used: Vec<UsedTopicRecord> = records
                        .iter()
                        .filter_map(|x| serde_json::from_value(x.clone()).ok())
                        .collect();
                    *self.used_topics.lock().unwrap() = used;
                }
            }
        }
    }

    /// 从磁盘重载已用话题（跨实例：投递任务写的记录，常驻实例升级前要读到）
    fn reload_used_from_disk(&self) {
        if let Ok(content) = std::fs::read_to_string(self.used_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&content) {
                if let Some(records) = v.get("records").and_then(|x| x.as_array()) {
                    let used: Vec<UsedTopicRecord> = records
                        .iter()
                        .filter_map(|x| serde_json::from_value(x.clone()).ok())
                        .collect();
                    *self.used_topics.lock().unwrap() = used;
                }
            }
        }
    }

    fn persist_signals(&self) {
        let _g = self.write_lock.lock().unwrap();
        let signals = self.signals.lock().unwrap().clone();
        let last_analysis_at = *self.last_analysis_at.lock().unwrap();
        let payload = serde_json::json!({
            "version": 1,
            "last_analysis_at": last_analysis_at,
            "turns": signals,
        });
        let _ = self.atomic_write(
            &self.signals_path(),
            &serde_json::to_string_pretty(&payload).unwrap_or_default(),
        );
    }

    fn persist_materials(&self) {
        let _g = self.write_lock.lock().unwrap();
        let materials = self.materials.lock().unwrap().clone();
        let payload = serde_json::json!({ "version": 1, "materials": materials });
        let _ = self.atomic_write(
            &self.materials_path(),
            &serde_json::to_string_pretty(&payload).unwrap_or_default(),
        );
    }

    fn persist_used(&self) {
        let _g = self.write_lock.lock().unwrap();
        let used = self.used_topics.lock().unwrap().clone();
        let payload = serde_json::json!({ "version": 1, "records": used });
        let _ = self.atomic_write(
            &self.used_path(),
            &serde_json::to_string_pretty(&payload).unwrap_or_default(),
        );
    }

    // ---------- 信号记录 ----------

    /// 记录一轮对话（user 或 ai）；user 轮同时检查回应窗口升级
    pub fn note_turn(&self, actor: &str, text: &str) {
        let cleaned = clean_text(text);
        if cleaned.is_empty() {
            return;
        }
        let safe_actor = if actor == "user" { "user" } else { "ai" };
        let now = now_secs();
        {
            let mut signals = self.signals.lock().unwrap();
            signals.push(TopicTurnSignal {
                actor: safe_actor.to_string(),
                text: cleaned,
                timestamp: now,
            });
            // 保留期 + 轮数上限
            signals.retain(|t| now - t.timestamp <= SIGNAL_RETENTION_SECONDS);
            while signals.len() > MAX_SIGNAL_TURNS {
                signals.remove(0);
            }
        }
        *self.dirty.lock().unwrap() = true;
        self.persist_signals();
        if safe_actor == "user" {
            self.maybe_upgrade_topic_response(now);
        }
    }

    // ---------- 信号查询 ----------

    /// 有意义的用户轮次数（≥ MIN_USER_TURNS_FOR_TOPIC 才 ready）
    pub fn meaningful_user_turns(&self) -> usize {
        self.signals
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.actor == "user" && is_meaningful_turn(&t.text))
            .count()
    }

    pub fn is_ready(&self) -> bool {
        self.meaningful_user_turns() >= MIN_USER_TURNS_FOR_TOPIC
    }

    /// 最后一轮信号时间（无信号 → None）
    pub fn last_turn_at(&self) -> Option<f64> {
        self.signals.lock().unwrap().last().map(|t| t.timestamp)
    }

    /// 渲染信号为话题筛选 prompt 的上下文（相对时间标签）
    pub fn format_global_signals(&self) -> String {
        let signals = self.signals.lock().unwrap().clone();
        if signals.is_empty() {
            return String::new();
        }
        let base_ts = signals.last().map(|t| t.timestamp).unwrap_or(0.0);
        let mut lines = Vec::new();
        for turn in &signals {
            let age_s = (base_ts - turn.timestamp).max(0.0);
            let label = if turn.actor == "user" { "用户" } else { "AI" };
            lines.push(format!("- [{}] {}: {}", format_age(age_s), label, turn.text));
        }
        lines.join("\n")
    }

    /// 当前是否有 pending 素材
    pub fn has_pending_material(&self) -> bool {
        self.materials
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.status == "pending")
    }

    // ---------- 后台分析（LLM 提取深话题）----------

    /// 分析入口：信号 ready + 成熟 + 无 pending 时调用
    /// 返回 Ok(true) = 产生了新素材，Ok(false) = 无素材/未就绪
    pub async fn process_now(&self, api_key: &str) -> Result<bool, String> {
        // 已有 pending 素材在投递阶段 → 不重复分析（抄 pipeline.process_now 开头）
        if self.has_pending_material() {
            return Ok(false);
        }
        if !self.is_ready() {
            *self.dirty.lock().unwrap() = false;
            return Ok(false);
        }
        // 成熟期：最后一轮对话至少 60s 前（避免对话进行中反复分析）
        if let Some(last) = self.last_turn_at() {
            if now_secs() - last < CANDIDATE_MATURE_SECONDS {
                return Ok(false);
            }
        }
        // 分析冷却：两次分析间隔 ≥10min（分析过没素材的信号不重复烧 LLM）
        let now = now_secs();
        if now - *self.last_analysis_at.lock().unwrap() < ANALYSIS_COOLDOWN_SECONDS {
            return Ok(false);
        }
        let signals_text = self.format_global_signals();
        if signals_text.is_empty() {
            return Ok(false);
        }

        let raw = collect_topic_candidates(api_key, &signals_text).await?;
        *self.last_analysis_at.lock().unwrap() = now_secs();
        self.persist_signals();
        // 过滤 + 排序 + 去重（抄 pipeline.process_now 的 cleaned 段）
        let mut cleaned: Vec<TopicMaterial> = raw
            .into_iter()
            .filter(|m| m.relevance >= TOPIC_RELEVANCE_MIN && m.risk <= TOPIC_RISK_MAX)
            .collect();
        cleaned.sort_by(|a, b| b.relevance.cmp(&a.relevance));
        cleaned.truncate(2);
        let cleaned = self.filter_available_materials(cleaned);

        let mut materials = self.materials.lock().unwrap();
        if cleaned.is_empty() {
            materials.clear();
        } else {
            *materials = cleaned.clone();
        }
        *self.dirty.lock().unwrap() = false;
        drop(materials);
        self.persist_materials();
        Ok(!cleaned.is_empty())
    }

    /// 去重：48h 内用过的素材丢弃（抄 pipeline._filter_available_materials）
    fn filter_available_materials(&self, materials: Vec<TopicMaterial>) -> Vec<TopicMaterial> {
        materials
            .into_iter()
            .filter(|m| !self.topic_was_recently_used(m))
            .collect()
    }

    // ---------- 投递门 ----------

    /// 取下一个可投递素材：pending 中 relevance 最高 + 配额/间隔/去重全过
    pub fn next_ready_material(&self) -> Option<TopicMaterial> {
        let mut mats: Vec<TopicMaterial> = self
            .materials
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.status == "pending")
            .cloned()
            .collect();
        if mats.is_empty() {
            return None;
        }
        mats.sort_by(|a, b| b.relevance.cmp(&a.relevance));
        for m in mats {
            if self.topic_was_recently_used(&m) {
                continue;
            }
            if self.daily_quota_reached() {
                continue;
            }
            if self.seconds_until_next_trigger() > 0.0 {
                continue;
            }
            return Some(m);
        }
        None
    }

    /// 今日配额是否用尽（weight 制：sum(today.weight) >= 2）
    pub fn daily_quota_reached(&self) -> bool {
        let now = now_secs();
        let today = local_day(now);
        let used = self.used_topics.lock().unwrap();
        let weight: f64 = used
            .iter()
            .filter(|r| local_day(r.used_at) == today)
            .map(|r| record_weight(r))
            .sum();
        weight >= MAX_DAILY_TOPIC_TRIGGERS - 1e-6
    }

    /// 距离下一次可投递的等待秒数（0 = 可以）
    pub fn seconds_until_next_trigger(&self) -> f64 {
        let now = now_secs();
        let used = self.used_topics.lock().unwrap();
        let latest = used
            .iter()
            .filter(|r| now - r.used_at <= USED_TOPIC_RECENT_SECONDS)
            .map(|r| r.used_at)
            .fold(0.0f64, f64::max);
        if latest <= 0.0 {
            return 0.0;
        }
        (MIN_TOPIC_TRIGGER_GAP_SECONDS - (now - latest)).max(0.0)
    }

    /// 48h 内是否用过类似话题（keyword 交集为主，bigram 相似度兜底）
    pub fn topic_was_recently_used(&self, material: &TopicMaterial) -> bool {
        let now = now_secs();
        let used = self.used_topics.lock().unwrap();
        let material_keywords: HashSet<String> = material
            .keywords
            .iter()
            .map(|k| k.trim().to_lowercase())
            .collect();
        let material_bigrams = bigram_set(&material.interest);

        for record in used.iter().filter(|r| now - r.used_at <= USED_TOPIC_RECENT_SECONDS) {
            // 关键词交集 → 同一话题
            let record_keywords: HashSet<String> =
                record.keywords.iter().map(|k| k.trim().to_lowercase()).collect();
            if !material_keywords.is_empty()
                && !record_keywords.is_empty()
                && !material_keywords.is_disjoint(&record_keywords)
            {
                return true;
            }
            // bigram 兜底：相似度 ≥0.6 且共享 ≥2 个二元组
            let record_bigrams = bigram_set(&record.interest);
            if !material_bigrams.is_empty() && !record_bigrams.is_empty() {
                let shared = material_bigrams.intersection(&record_bigrams).count();
                if shared >= 2
                    && topic_similarity(&material_bigrams, &record_bigrams) >= 0.6
                {
                    return true;
                }
            }
        }
        false
    }

    // ---------- 投递记账 ----------

    /// 标记素材已投递：记录 used（weight 1/3）+ 武装回应窗口
    pub fn mark_topic_used(&self, material: &TopicMaterial) {
        let now = now_secs();
        self.prune_used(now);
        let mut used = self.used_topics.lock().unwrap();
        used.push(UsedTopicRecord {
            used_at: now,
            weight: UNANSWERED_TOPIC_WEIGHT,
            interest: material.interest.clone(),
            keywords: material.keywords.clone(),
            response_deadline: Some(now + TOPIC_RESPONSE_WINDOW_SECONDS),
        });
        drop(used);
        // 素材清空（本次投递消耗掉）
        self.materials.lock().unwrap().clear();
        self.persist_used();
        self.persist_materials();
    }

    /// 用户轮触发：扫 used 记录，10min 窗口内投递的 → weight 升级 1.0
    /// （记录带 deadline 持久化，跨实例/跨重启有效；先重载磁盘确保读到投递任务写的记录）
    pub fn maybe_upgrade_topic_response(&self, now: f64) {
        self.reload_used_from_disk();
        let mut upgraded = false;
        {
            let mut used = self.used_topics.lock().unwrap();
            for record in used.iter_mut() {
                if let Some(deadline) = record.response_deadline {
                    if now <= deadline && record.weight < 1.0 {
                        record.weight = 1.0;
                        record.response_deadline = None;
                        upgraded = true;
                    }
                }
            }
        }
        if upgraded {
            self.persist_used();
        }
    }

    fn prune_used(&self, now: f64) {
        let mut used = self.used_topics.lock().unwrap();
        // 保留：今日记录 + 48h 内记录
        used.retain(|r| local_day(r.used_at) == local_day(now) || now - r.used_at <= USED_TOPIC_RECENT_SECONDS);
    }
}

/// 记录权重（非法值按 1.0 算，抄 pipeline._record_weight）
fn record_weight(record: &UsedTopicRecord) -> f64 {
    let w = record.weight;
    if w.is_nan() {
        return 1.0;
    }
    w.clamp(0.0, 1.0)
}

// ── LLM 调用（抄 N.E.K.O. llm_enrichment.call_topic_candidates）──

/// 话题筛选 prompt（zh，抄 prompts_activity.py TOPIC_CANDIDATE_PROMPTS["zh"]）
pub const TOPIC_CANDIDATE_PROMPT: &str = r#"你是一个陪伴产品的话题筛选助手。你的任务不是总结最近一句话，而是从下面这段最近对话里挑 1-2 个真的值得以后低频开口的深话题机会。

======以下为最近对话(按时间顺序)======
{GLOBAL_SIGNALS}
======以上为最近对话(按时间顺序)======

要求：
- 不要复述用户原话，也不要暴露"我在分析聊天记录"
- 只挑用户近期反复出现、明显稳定在意的兴趣 / 计划 / 纠结 / 情绪 / 选择
- 寒暄、语气词、单薄短句、问卷式提问一律忽略
- 不要因为两个词凑巧相邻就硬拼成一个话题；关联不自然就不输出
- 宁缺毋滥：没把握就少出，甚至直接输出空列表
- 每个话题只是给角色的开口素材，不是最终台词
输出严格 JSON（不带 markdown 代码块）：
{"topics": [
  {
    "interest": "用户最近在意、纠结、计划或反复提到的一件具体事，整理成一句，不超过30字",
    "keywords": ["3-6个关键词，用于去重、筛选联网结果和投递前 research seed；围绕用户反复在意的稳定点，不要用偶然冒出的词"],
    "relevance": 0-100,
    "risk": 0-100
  }
]}

评分：
- relevance：这个话题和用户的相关度，结合它是否在对话里反复稳定出现。明显反复出现、确实是用户在意的事 → 高分；只出现一两次、或只是顺口提一句 → 低分。如实打分，不要为了让它被采用而虚高。
- risk：主动提起这个话题会打扰、冒犯、误解或显得硬凑的风险。越可能让用户反感或觉得突兀 → 越高分。

如果没有值得以后接的话题，输出 {"topics": []}。"#;

/// 调用 LLM 提取话题候选
/// 返回: 已过滤（relevance/risk 阈值内）的素材列表；失败 → Err
pub async fn collect_topic_candidates(
    api_key: &str,
    signals_text: &str,
) -> Result<Vec<TopicMaterial>, String> {
    let prompt = TOPIC_CANDIDATE_PROMPT.replace("{GLOBAL_SIGNALS}", signals_text);

    let body = serde_json::json!({
        "model": crate::chat_model(),
        "messages": [
            {"role": "system", "content": "你是话题筛选助手，只输出 JSON。"},
            {"role": "user", "content": prompt}
        ],
        "stream": false,
        "max_tokens": 512,
        "response_format": {"type": "json_object"}
    });

    let client = crate::shared_http_client();
    let resp = client
        .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("话题筛选请求失败: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("话题筛选接口返回 {}: {}", status, text));
    }
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("话题筛选响应解析失败: {}", e))?;
    let content = resp_body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "话题筛选响应无内容".to_string())?;

    let parsed = parse_topics_json(content);
    let mut out = Vec::new();
    let now = now_secs();
    for item in parsed.into_iter().take(4) {
        let interest = item
            .get("interest")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if interest.is_empty() || interest.chars().count() < 4 {
            continue;
        }
        let relevance = item.get("relevance").and_then(|x| x.as_i64()).unwrap_or(70).clamp(0, 100) as i32;
        let risk = item.get("risk").and_then(|x| x.as_i64()).unwrap_or(20).clamp(0, 100) as i32;
        if relevance < TOPIC_RELEVANCE_MIN || risk > TOPIC_RISK_MAX {
            continue;
        }
        let mut keywords = Vec::new();
        if let Some(arr) = item.get("keywords").and_then(|x| x.as_array()) {
            for kw in arr {
                let kw_text = kw.as_str().unwrap_or("").trim().to_string();
                if !kw_text.is_empty() && !keywords.contains(&kw_text) {
                    keywords.push(kw_text);
                }
                if keywords.len() >= 6 {
                    break;
                }
            }
        }
        out.push(TopicMaterial {
            hook_id: format!("topic_{}_{}", now as u64, simple_hash(&interest)),
            interest,
            keywords,
            relevance,
            risk,
            status: "pending".to_string(),
            created_at: now,
        });
        if out.len() >= 2 {
            break;
        }
    }
    Ok(out)
}

/// 解析话题 JSON（兼容 ```json 围栏）
fn parse_topics_json(raw: &str) -> Vec<Value> {
    let mut text = raw.trim().to_string();
    // 剥 markdown 围栏
    if let Some(stripped) = strip_markdown_fence(&text) {
        text = stripped;
    }
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.get("topics")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

fn strip_markdown_fence(text: &str) -> Option<String> {
    let t = text.trim();
    let start = t.find("```")?;
    let after = &t[start + 3..];
    let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after[body_start..];
    let end = body.rfind("```")?;
    Some(body[..end].trim().to_string())
}

// ── Phase-2 生成（抄 delivery.py _DETAIL_TEMPLATES zh）──

/// 话题开场生成 prompt 模板（zh）
pub const TOPIC_OPENING_PROMPT: &str = r#"最近关注：{INTEREST}
请只生成一句自然开场，像随口想起来，不要说"根据你的近期兴趣"，不要像问卷。"#;

/// 为话题素材生成一句自然开场
/// 返回 Ok(Some(text)) = 开场白，Ok(None) = [PASS] 放弃
pub async fn generate_topic_opening(
    api_key: &str,
    material: &TopicMaterial,
    memory_ctx: &str,
    activity_section: &str,
) -> Result<Option<String>, String> {
    let detail = TOPIC_OPENING_PROMPT.replace("{INTEREST}", &material.interest);

    let system = "你是优香（早濑优香），来自《蔚蓝档案》的千年科学学园学生，研讨部（Seminar）的会计，16岁，粉发双马尾，现在是主人的二次元AI桌宠助手。背景：研讨部是千年学园的管理部门，你负责账目与预算，是学园里出了名的'铁算盘'，连一张纸的经费都要精打细算；和同事诺亚是形影不离的好友。性格：认真负责、一丝不苟，对数字和账目极其敏感，讨厌浪费、花钱谨慎（但对自己爱吃的甜食会偷偷留'甜点预算'，甜食面前原则会动摇）；标准傲娇——嘴上嫌弃、刀子嘴豆腐心，心里其实很关心主人，被戳穿时会慌慌张张辩解；容易害羞，被夸会脸红；有会计职业病，看到乱花钱会忍不住念叨；胜负心强，被质疑算数会较真。说话语气严谨又带点娇嗔，偶尔冒出记账/预算相关的话；回复简短自然（40字以内）、口语化，称呼主人为'主人'，自称'优香'，习惯在句尾加'~'、'哦'、'啦'等语气词。虽然嘴上总说主人乱花钱、爱添麻烦，但会默默记下主人的喜好，是'嘴上嫌弃、行动诚实'的傲娇会计。根据提示决定是否主动说话。如果觉得这个话题不适合现在提，只回复 [PASS]。其他情况直接说出想说的话，不要任何格式标记。";

    let user = format!(
        "{}\n\n======以下是你的记忆======\n{}\n======以上是你的记忆======\n\n======以下是主人当前状态======\n{}\n======以上是主人当前状态======\n\n请直接输出你想说的话（40字以内），或 [PASS]",
        detail, memory_ctx, activity_section
    );

    let body = serde_json::json!({
        "model": crate::chat_model(),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ],
        "stream": false,
        "max_tokens": 100
    });

    let client = crate::shared_http_client();
    let resp = client
        .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("话题开场请求失败: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("话题开场接口返回 {}: {}", status, text));
    }
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("话题开场响应解析失败: {}", e))?;
    let content = resp_body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "话题开场响应无内容".to_string())?
        .trim()
        .to_string();

    if content.contains("[PASS]") || content.is_empty() {
        return Ok(None);
    }
    let clean = content
        .trim_matches(|c| c == '"' || c == '「' || c == '」' || c == '“' || c == '”')
        .trim()
        .to_string();
    if clean.is_empty() {
        return Ok(None);
    }
    Ok(Some(clean))
}

// ── 工具函数（抄 signals.py / common.py）──

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn clean_text(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return String::new();
    }
    // 压缩空白 + 截断（每轮信号 ≤500 字，够 LLM 判断）
    let mut out = String::new();
    let mut prev_space = false;
    for c in t.chars() {
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
        if out.chars().count() >= 500 {
            break;
        }
    }
    out.trim().to_string()
}

/// 是否算有意义信号（非寒暄 + 至少 3 个中文字符/字母数字）
fn is_meaningful_turn(text: &str) -> bool {
    let cleaned = clean_text(text);
    if cleaned.is_empty() {
        return false;
    }
    if FILLER_TEXTS.contains(&cleaned.as_str()) {
        return false;
    }
    let signal_len = cleaned
        .chars()
        .filter(|c| (*c >= '\u{4e00}' && *c <= '\u{9fff}') || c.is_alphanumeric())
        .count();
    signal_len >= 3
}

/// 相对时间标签（秒 → "3s前/5min前/2h前"）
fn format_age(age_s: f64) -> String {
    if age_s < 90.0 {
        format!("{}s前", age_s as u64)
    } else if age_s < 3600.0 {
        format!("{}min前", (age_s / 60.0) as u64)
    } else {
        format!("{}h前", (age_s / 3600.0) as u64)
    }
}

/// 简单字符串哈希（DefaultHasher，够去重用）
fn simple_hash(text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    format!("{:x}", h.finish())
}

/// 文本二元组集合（bigram，中文按字符）
fn bigram_set(text: &str) -> HashSet<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .windows(2)
        .map(|w| w.iter().collect::<String>())
        .collect()
}

/// 集合相似度：overlap / min(len_a, len_b)
fn topic_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let overlap = a.intersection(b).count();
    overlap as f64 / a.len().min(b.len()) as f64
}

/// 时间戳 → 本地日期字符串（"YYYY-MM-DD"，配额按天算）
fn local_day(ts: f64) -> String {
    let dt = chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local))
        .unwrap_or_else(chrono::Local::now);
    dt.format("%Y-%m-%d").to_string()
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dagent_topic_test_{}_{}", tag, n));
        // 清掉跨运行残留（COUNTER 每进程归零，目录名会复用）
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn make_material(interest: &str, keywords: &[&str], relevance: i32, risk: i32) -> TopicMaterial {
        TopicMaterial {
            hook_id: format!("test_{}", simple_hash(interest)),
            interest: interest.to_string(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            relevance,
            risk,
            status: "pending".to_string(),
            created_at: now_secs(),
        }
    }

    #[test]
    fn test_signal_meaningful_filter() {
        assert!(!is_meaningful_turn("嗯"));
        assert!(!is_meaningful_turn("哈哈"));
        assert!(!is_meaningful_turn("好的"));
        assert!(is_meaningful_turn("我最近在学 Rust"));
        assert!(is_meaningful_turn("明天要去面试"));
        assert!(!is_meaningful_turn("哦"));
    }

    #[test]
    fn test_ready_threshold() {
        let pool = TopicPool::new(test_dir("ready"));
        // 7 条有意义用户轮 → 不 ready
        for i in 0..7 {
            pool.note_turn("user", &format!("我最近在学 Rust 第 {} 天", i));
        }
        assert!(!pool.is_ready());
        assert_eq!(pool.meaningful_user_turns(), 7);
        // 第 8 条 → ready
        pool.note_turn("user", "今天又把生命周期看了一遍");
        assert!(pool.is_ready());
    }

    #[test]
    fn test_ai_turns_do_not_count() {
        let pool = TopicPool::new(test_dir("ai_turn"));
        for _ in 0..8 {
            pool.note_turn("ai", "优香说的不算");
        }
        assert!(!pool.is_ready());
        assert_eq!(pool.meaningful_user_turns(), 0);
    }

    #[test]
    fn test_signal_window_cap() {
        let pool = TopicPool::new(test_dir("cap"));
        for i in 0..70 {
            pool.note_turn("user", &format!("第 {} 条用户消息", i));
        }
        assert_eq!(pool.signals.lock().unwrap().len(), MAX_SIGNAL_TURNS);
    }

    #[test]
    fn test_material_filter_thresholds() {
        // 阈值过滤发生在分析阶段（collect_topic_candidates），此处验证投递层行为
        let pool = TopicPool::new(test_dir("filter"));
        assert!(!pool.has_pending_material());
        assert!(pool.next_ready_material().is_none());
        // 有 pending 素材 → 可投递
        pool.materials
            .lock()
            .unwrap()
            .push(make_material("高相关话题", &["rust"], 85, 10));
        assert!(pool.has_pending_material());
        assert!(pool.next_ready_material().is_some());
    }

    #[test]
    fn test_daily_quota_weight() {
        let pool = TopicPool::new(test_dir("quota"));
        // 2 条回应过的（weight 1.0）→ 配额满
        pool.used_topics.lock().unwrap().push(UsedTopicRecord {
            used_at: now_secs(),
            weight: 1.0,
            interest: "a".into(),
            keywords: vec![],
            response_deadline: None,
        });
        pool.used_topics.lock().unwrap().push(UsedTopicRecord {
            used_at: now_secs(),
            weight: 1.0,
            interest: "b".into(),
            keywords: vec![],
            response_deadline: None,
        });
        assert!(pool.daily_quota_reached());
    }

    #[test]
    fn test_unanswered_weight_quota() {
        let pool = TopicPool::new(test_dir("unanswered"));
        // 未回应（1/3）→ 需要 6 条才到 2.0
        for i in 0..5 {
            pool.used_topics.lock().unwrap().push(UsedTopicRecord {
                used_at: now_secs(),
                weight: UNANSWERED_TOPIC_WEIGHT,
                interest: format!("t{}", i),
                keywords: vec![],
                response_deadline: None,
            });
        }
        assert!(!pool.daily_quota_reached());
        pool.used_topics.lock().unwrap().push(UsedTopicRecord {
            used_at: now_secs(),
            weight: UNANSWERED_TOPIC_WEIGHT,
            interest: "t5".into(),
            keywords: vec![],
            response_deadline: None,
        });
        assert!(pool.daily_quota_reached());
    }

    #[test]
    fn test_min_gap_4h() {
        let pool = TopicPool::new(test_dir("gap"));
        // 刚用过 → 要等 4h
        pool.used_topics.lock().unwrap().push(UsedTopicRecord {
            used_at: now_secs(),
            weight: 1.0,
            interest: "x".into(),
            keywords: vec![],
            response_deadline: None,
        });
        assert!(pool.seconds_until_next_trigger() > 0.0);
        // 4h 前用过 → 可以
        let mut used = pool.used_topics.lock().unwrap();
        used.clear();
        used.push(UsedTopicRecord {
            used_at: now_secs() - 4.0 * 3600.0 - 1.0,
            weight: 1.0,
            interest: "y".into(),
            keywords: vec![],
            response_deadline: None,
        });
        drop(used);
        assert_eq!(pool.seconds_until_next_trigger(), 0.0);
    }

    #[test]
    fn test_dedup_keyword_intersection() {
        let pool = TopicPool::new(test_dir("dedup_kw"));
        pool.used_topics.lock().unwrap().push(UsedTopicRecord {
            used_at: now_secs(),
            weight: 1.0,
            interest: "学 Rust 遇到借用检查问题".into(),
            keywords: vec!["rust".into(), "借用".into()],
            response_deadline: None,
        });
        // 共享关键词 → 判重
        let dup = make_material("Rust 生命周期问题", &["rust", "所有权"], 90, 5);
        assert!(pool.topic_was_recently_used(&dup));
        // 无共享关键词 → 放行
        let fresh = make_material("想学吉他", &["吉他", "音乐"], 80, 10);
        assert!(!pool.topic_was_recently_used(&fresh));
    }

    #[test]
    fn test_dedup_bigram_fallback() {
        let pool = TopicPool::new(test_dir("dedup_bigram"));
        pool.used_topics.lock().unwrap().push(UsedTopicRecord {
            used_at: now_secs(),
            weight: 1.0,
            interest: "考研数学复习进度".into(),
            keywords: vec![],
            response_deadline: None,
        });
        // 关键词为空，但 bigram 高度相似（仅一字之差）→ 判重
        let dup = make_material("考研数学复习安排", &[], 85, 10);
        assert!(pool.topic_was_recently_used(&dup));
        // 明显不同的话题 → 放行
        let fresh = make_material("学做饭", &[], 80, 10);
        assert!(!pool.topic_was_recently_used(&fresh));
    }

    #[test]
    fn test_mark_used_and_response_upgrade() {
        let pool = TopicPool::new(test_dir("response"));
        let m = make_material("学习 Rust", &["rust"], 90, 5);
        pool.mark_topic_used(&m);
        // 未回应 → 1/3
        let used = pool.used_topics.lock().unwrap();
        assert_eq!(used.last().unwrap().weight, UNANSWERED_TOPIC_WEIGHT);
        drop(used);
        // 窗口内用户回复 → 升级 1.0
        pool.note_turn("user", "其实我还在纠结要不要继续学");
        let used = pool.used_topics.lock().unwrap();
        assert_eq!(used.last().unwrap().weight, 1.0);
    }

    #[test]
    fn test_mark_used_clears_pending() {
        let pool = TopicPool::new(test_dir("clear"));
        pool.materials
            .lock()
            .unwrap()
            .push(make_material("学习 Rust", &["rust"], 90, 5));
        assert!(pool.has_pending_material());
        let m = make_material("学习 Rust", &["rust"], 90, 5);
        pool.mark_topic_used(&m);
        assert!(!pool.has_pending_material());
    }

    #[test]
    fn test_parse_topics_json() {
        let raw = r#"{"topics": [{"interest": "学习 Rust", "keywords": ["rust"], "relevance": 85, "risk": 10}]}"#;
        let topics = parse_topics_json(raw);
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0]["interest"], "学习 Rust");
        // markdown 围栏
        let fenced = format!("```json\n{}\n```", raw);
        assert_eq!(parse_topics_json(&fenced).len(), 1);
        // 空列表
        assert_eq!(parse_topics_json(r#"{"topics": []}"#).len(), 0);
    }

    #[test]
    fn test_bigram_similarity() {
        let a = bigram_set("考研数学复习");
        let b = bigram_set("考研数学的复习");
        let sim = topic_similarity(&a, &b);
        assert!(sim >= 0.6, "bigram 相似度应高: {}", sim);
        let c = bigram_set("今天天气很好");
        let sim2 = topic_similarity(&a, &c);
        assert!(sim2 < 0.6, "无关话题相似度应低: {}", sim2);
    }

    #[test]
    fn test_next_ready_material_quota_block() {
        let pool = TopicPool::new(test_dir("next_ready"));
        pool.materials
            .lock()
            .unwrap()
            .push(make_material("学习 Rust", &["rust"], 90, 5));
        // 今日配额已满 → 无素材可投
        pool.used_topics.lock().unwrap().push(UsedTopicRecord {
            used_at: now_secs(),
            weight: 1.0,
            interest: "a".into(),
            keywords: vec![],
            response_deadline: None,
        });
        pool.used_topics.lock().unwrap().push(UsedTopicRecord {
            used_at: now_secs(),
            weight: 1.0,
            interest: "b".into(),
            keywords: vec![],
            response_deadline: None,
        });
        assert!(pool.next_ready_material().is_none());
    }

    // ── 真实 API e2e（需要 QIANWEN_API_KEY，cargo test e2e_topic -- --ignored --nocapture）──

    #[tokio::test]
    #[ignore]
    async fn e2e_topic_candidates_and_opening() {
        // key 来源：环境变量 → config.json
        let key = std::env::var("QIANWEN_API_KEY").unwrap_or_else(|_| {
            let cfg_path = r"D:\3Dagent\3dagent\config.json";
            let content = std::fs::read_to_string(cfg_path).unwrap_or_default();
            let content = content.trim_start_matches('\u{feff}');
            let v: Value = serde_json::from_str(content).unwrap_or(Value::Null);
            v.get("qianwen_api_key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_string()
        });
        assert!(!key.is_empty(), "需要 QIANWEN_API_KEY 环境变量或 config.json");

        // 模拟一段真实对话（用户反复提到学 Rust / 考研，≥8 条有意义用户轮）
        let pool = TopicPool::new(test_dir("e2e"));
        let turns = [
            "最近在纠结要不要继续学 Rust，感觉很难",
            "今天看生命周期又看懵了，但还是想坚持",
            "室友说 Rust 找工作好，我有点心动",
            "明天打算把 Rust 的智能指针看完",
            "最近有点累，学 Rust 学不动了",
            "考研和学 Rust 有点冲突，时间不够用",
            "我们学校计算机专业挺卷的",
            "晚上想去操场跑步放松一下",
            "听说 Rust 社区对新手挺友好的",
            "周末打算整理一下 Rust 学习笔记",
        ];
        for t in turns {
            pool.note_turn("user", t);
            pool.note_turn("ai", "优香给你加油！慢慢来~");
        }
        assert!(pool.is_ready(), "信号应 ready");
        let signals_text = pool.format_global_signals();
        println!("=== 信号 ===\n{}", signals_text);

        let materials = collect_topic_candidates(&key, &signals_text)
            .await
            .expect("话题提取失败");
        println!("=== 话题素材 ===");
        for m in &materials {
            println!(
                "[relevance={} risk={}] {} | keywords={:?}",
                m.relevance, m.risk, m.interest, m.keywords
            );
        }
        assert!(!materials.is_empty(), "应至少提取 1 个话题");

        // Phase-2 开场生成
        let opening = generate_topic_opening(&key, &materials[0], "（记忆：主人是大二学生，最近在学 Rust）", "（状态：主人空闲）")
            .await
            .expect("开场生成失败");
        println!("=== 开场白 ===\n{:?}", opening);
        match opening {
            Some(text) => assert!(!text.is_empty()),
            None => println!("[PASS] 模型认为不适合现在提"),
        }
    }
}
