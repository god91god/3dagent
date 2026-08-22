// ============================================================
// 记忆存储层 —— 借鉴 N.E.K.O. 五维记忆（L1 + L2 子集）
// 结构参考: N.E.K.O. memory/facts.py / recent.py / persona/
//          memory/reflection/ memory/evidence.py
// 许可: Apache-2.0（同上游）
//
// L1 实现：
//   - facts.json    原子事实库（importance 1-10，SHA-256 精确去重）
//   - recent.json   近期对话滑动窗口（最近 N 轮）
//   - persona.json  人格（master 手写设定 + 长期事实）
// L2 实现：
//   - reflections.json  反思库（pending → confirmed → promoted）
//   - evidence 双通道评分：reinforcement 半衰期 30 天 / disputation 180 天
//   - 晋升：score ≥ 1.0 confirmed，≥ 2.0 promoted（合入 persona）
// 原子写策略：写临时文件 + rename（防写一半损坏）
// ============================================================

use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// ── L2 证据机制常量（抄 N.E.K.O. memory_settings.py）──
/// score ≥ 1.0 → confirmed
pub const EVIDENCE_CONFIRMED_THRESHOLD: f64 = 1.0;
/// score ≥ 2.0 → promoted
pub const EVIDENCE_PROMOTED_THRESHOLD: f64 = 2.0;
/// score ≤ -2.0 → 归档候选
pub const EVIDENCE_ARCHIVE_THRESHOLD: f64 = -2.0;
/// reinforcement 半衰期（天）
pub const EVIDENCE_REIN_HALF_LIFE_DAYS: f64 = 30.0;
/// disputation 半衰期（天）——否定记得更久
pub const EVIDENCE_DISP_HALF_LIFE_DAYS: f64 = 180.0;
/// Stage-2 间接 reinforces 权重
pub const USER_FACT_REINFORCE_DELTA: f64 = 0.5;
/// Stage-2 间接 negates 权重（否定更重）
pub const USER_FACT_NEGATE_DELTA: f64 = 1.0;
/// 反思合成最少需要的未吸收事实数
pub const MIN_FACTS_FOR_REFLECTION: usize = 5;

// ── importance 种子（抄 N.E.K.O. evidence.py _IMPORTANCE_TO_INITIAL_REIN）──
/// 源事实最高 importance ≥ 阈值 → 预置 reinforcement 种子
/// 10→0.8, 9→0.6, 8→0.4, 7→0.2, ≤6→0.0
pub fn initial_reinforcement_from_importance(max_importance: i32) -> f64 {
    match max_importance {
        10 => 0.8,
        9 => 0.6,
        8 => 0.4,
        7 => 0.2,
        _ => 0.0,
    }
}

// ── 时间驱动 fallback（弱记忆模式，抄 N.E.K.O. WEAK_MEMORY_*）──
/// pending 按 created_at 满 7 天自动 confirmed（零 LLM 成本）
pub const WEAK_MEMORY_AUTO_CONFIRM_DAYS: i64 = 7;
/// confirmed 按 confirmed 满 7 天自动 promoted（需要 confirmed_at 字段）
pub const WEAK_MEMORY_AUTO_PROMOTE_DAYS: i64 = 7;

/// 记忆根目录（app data dir / memory）
pub struct MemoryStore {
    dir: PathBuf,
}

/// 进程级全局写锁：所有 MemoryStore 实例（常驻 AppState / 每轮 user 消息 spawn 的
/// 提取 store / 3 分钟维护循环的 store）共用同一把锁。
/// 历史 bug：每实例独立锁 → 多实例并发 read-modify-write 同一 JSON（facts/persona），
/// 新事实互相覆盖，甚至固定 tmp 名互相截断导致整个文件清空。
/// 写操作频率低，全局串行化无性能影响。
static MEMORY_WRITE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

impl MemoryStore {
    fn global_lock(&self) -> std::sync::MutexGuard<'static, ()> {
        MEMORY_WRITE_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

/// 一条事实
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Fact {
    pub id: String,
    pub text: String,
    pub importance: i32,
    pub entity: String,
    pub created_at: String,
    #[serde(default)]
    pub absorbed: bool,
    /// 是否已参与过 Stage-2 信号检测（防同一事实每 3 分钟重复打信号 → 证据分膨胀）
    #[serde(default)]
    pub signals_checked: bool,
}

/// 一条反思（L2）—— N.E.K.O. reflections.json 精简版
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Reflection {
    pub id: String,
    pub text: String,
    pub entity: String,
    #[serde(default)]
    pub relation_type: String,
    #[serde(default)]
    pub temporal_scope: String,
    /// pending | confirmed | promoted | archived
    pub status: String,
    /// 合成的来源事实 id 列表（确定性 ID 依据）
    #[serde(default)]
    pub source_fact_ids: Vec<String>,
    /// 证据：正向确认分
    #[serde(default)]
    pub reinforcement: f64,
    /// 证据：负向反驳分
    #[serde(default)]
    pub disputation: f64,
    #[serde(default)]
    pub rein_last_signal_at: Option<String>,
    #[serde(default)]
    pub disp_last_signal_at: Option<String>,
    pub created_at: String,
    /// confirmed 时间（时间驱动 promote fallback 用）
    #[serde(default)]
    pub confirmed_at: Option<String>,
}

// ── evidence 数学（抄 N.E.K.O. memory/evidence.py）──

/// 计算带时间衰减的有效证据分数
/// effective_rein = rein * 0.5 ^ (rein_age_days / 30)
/// effective_disp  = disp  * 0.5 ^ (disp_age_days / 180)
/// score = effective_rein - effective_disp
pub fn evidence_score(
    reinforcement: f64,
    disputation: f64,
    rein_last_signal_at: Option<&str>,
    disp_last_signal_at: Option<&str>,
    now_iso: &str,
) -> f64 {
    let eff_rein = reinforcement * half_life_decay(rein_last_signal_at, now_iso, EVIDENCE_REIN_HALF_LIFE_DAYS);
    let eff_disp = disputation * half_life_decay(disp_last_signal_at, now_iso, EVIDENCE_DISP_HALF_LIFE_DAYS);
    eff_rein - eff_disp
}

/// 0.5 ^ (age_days / half_life)；无时间戳视为 0 天（不衰减）
/// 兼容两种 ISO 风格："2026-08-19 20:00:00"（空格）和 "2026-08-19T20:00:00"（T）
fn half_life_decay(last_signal_at: Option<&str>, now_iso: &str, half_life_days: f64) -> f64 {
    let Some(ts) = last_signal_at else {
        return 1.0;
    };
    let Some(last) = parse_local_dt(ts) else {
        return 1.0;
    };
    let Some(now) = parse_local_dt(now_iso) else {
        return 1.0;
    };
    let age_days = (now - last).num_minutes() as f64 / 1440.0;
    0.5f64.powf(age_days / half_life_days)
}

/// 解析本地时间字符串，兼容空格/T 两种分隔
fn parse_local_dt(s: &str) -> Option<chrono::NaiveDateTime> {
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }
    None
}

/// 两个 ISO 字符串之间的天数差（now - then）
fn days_between(then_iso: &str, now_iso: &str) -> i64 {
    let Some(then) = parse_local_dt(then_iso) else {
        return 0;
    };
    let Some(now) = parse_local_dt(now_iso) else {
        return 0;
    };
    (now - then).num_days()
}

impl MemoryStore {
    pub fn new(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).ok();
        MemoryStore { dir }
    }

    /// 记忆目录路径（供后台任务重建 store 用）
    pub fn dir(&self) -> PathBuf {
        self.dir.clone()
    }

    // ---------- 文件路径 ----------

    fn facts_path(&self) -> PathBuf {
        self.dir.join("facts.json")
    }
    fn recent_path(&self) -> PathBuf {
        self.dir.join("recent.json")
    }
    fn persona_path(&self) -> PathBuf {
        self.dir.join("persona.json")
    }
    fn reflections_path(&self) -> PathBuf {
        self.dir.join("reflections.json")
    }

    // ---------- 原子读写 ----------

    /// 原子写：写临时文件后 rename（Windows 上先删目标再 rename，保证替换）
    /// tmp 文件名带 pid+纳秒后缀：即使并发写者（理论上有全局锁兜底）也不会共用一个 tmp
    fn atomic_write(&self, path: &Path, content: &str) -> Result<(), String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp = path.with_extension(format!("{}.{}.{}.tmp", "json", std::process::id(), nanos));
        let mut f = fs::File::create(&tmp).map_err(|e| format!("创建临时文件失败: {}", e))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("写临时文件失败: {}", e))?;
        f.flush().ok();
        // Windows: rename 到已存在目标会拒绝访问，先删旧文件
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        fs::rename(&tmp, path).map_err(|e| format!("替换文件失败: {}", e))
    }

    fn read_json(&self, path: &Path, default: Value) -> Value {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(default)
    }

    // ---------- 事实记忆 ----------

    /// 读取全部事实（未吸收优先，其次按 importance DESC）
    pub fn get_facts(&self) -> Vec<Fact> {
        let v = self.read_json(&self.facts_path(), json!([]));
        let mut facts: Vec<Fact> = v
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| serde_json::from_value(x.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        // 排序：未吸收在前；同吸收状态按 importance DESC
        facts.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then(b.created_at.cmp(&a.created_at))
        });
        facts
    }

    /// 新增事实（SHA-256 精确去重）
    /// 返回: Ok(true)=已添加, Ok(false)=重复跳过
    pub fn add_fact(&self, text: &str, importance: i32, entity: &str) -> Result<bool, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(false);
        }
        let _g = self.global_lock();

        let mut facts = self.read_json(&self.facts_path(), json!([]));
        let arr = facts.as_array_mut().ok_or("facts.json 不是数组")?;

        // SHA-256 精确去重（按 text 去重）
        let hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            text.hash(&mut h);
            h.finish().to_string()
        };
        for f in arr.iter() {
            if f.get("text").and_then(|t| t.as_str()) == Some(text) {
                return Ok(false); // 完全重复，跳过
            }
        }

        let now = chrono::Local::now();
        let fact = json!({
            "id": format!("fact_{}_{}", now.format("%Y%m%d%H%M%S"), &hash[..8]),
            "text": text,
            "importance": importance.clamp(1, 10),
            "entity": entity,
            "created_at": now.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "absorbed": false,
            "signals_checked": false,
        });
        arr.push(fact);
        self.atomic_write(
            &self.facts_path(),
            &serde_json::to_string_pretty(&facts).map_err(|e| e.to_string())?,
        )?;
        Ok(true)
    }

    /// 批量添加事实（带重复跳过统计）
    pub fn add_facts(&self, facts: &[(String, i32, String)]) -> (usize, usize) {
        let mut added = 0;
        let mut dup = 0;
        for (t, i, e) in facts {
            match self.add_fact(t, *i, e) {
                Ok(true) => added += 1,
                _ => dup += 1,
            }
        }
        (added, dup)
    }

    // ---------- 反思记忆（L2） ----------

    /// 读取全部反思
    pub fn get_reflections(&self) -> Vec<Reflection> {
        let v = self.read_json(&self.reflections_path(), json!([]));
        v.as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| serde_json::from_value(x.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 按状态过滤反思（如 "pending" / "confirmed"）
    pub fn get_reflections_by_status(&self, status: &str) -> Vec<Reflection> {
        self.get_reflections()
            .into_iter()
            .filter(|r| r.status == status)
            .collect()
    }

    /// 写入反思列表（整体替换）
    pub fn save_reflections(&self, reflections: &[Reflection]) -> Result<(), String> {
        let _g = self.global_lock();
        self.atomic_write(
            &self.reflections_path(),
            &serde_json::to_string_pretty(&reflections).map_err(|e| e.to_string())?,
        )
    }

    /// 新增一条反思（按 id 去重，幂等）—— 返回 true=新添加
    pub fn add_reflection(&self, r: Reflection) -> Result<bool, String> {
        let mut all = self.get_reflections();
        if all.iter().any(|x| x.id == r.id) {
            return Ok(false); // 幂等：同批事实已合成过
        }
        all.push(r);
        self.save_reflections(&all)?;
        Ok(true)
    }

    /// 更新反思状态/证据字段（按 id 定位）
    pub fn update_reflection(&self, id: &str, updater: impl FnOnce(&mut Reflection)) -> Result<bool, String> {
        let mut all = self.get_reflections();
        let mut found = false;
        for r in all.iter_mut() {
            if r.id == id {
                updater(r);
                found = true;
                break;
            }
        }
        if found {
            self.save_reflections(&all)?;
        }
        Ok(found)
    }

    /// 标记一批事实为"已吸收"（被反思消费）
    pub fn mark_facts_absorbed(&self, fact_ids: &[String]) -> Result<(), String> {
        let _g = self.global_lock();
        let mut facts = self.read_json(&self.facts_path(), json!([]));
        let arr = facts.as_array_mut().ok_or("facts.json 不是数组")?;
        let mut changed = false;
        for f in arr.iter_mut() {
            if let Some(id) = f.get("id").and_then(|i| i.as_str()) {
                if fact_ids.contains(&id.to_string()) {
                    f["absorbed"] = json!(true);
                    changed = true;
                }
            }
        }
        if changed {
            self.atomic_write(
                &self.facts_path(),
                &serde_json::to_string_pretty(&facts).map_err(|e| e.to_string())?,
            )?;
        }
        Ok(())
    }

    /// 获取未吸收且 importance ≥ min_imp 的事实（反思合成候选）
    pub fn get_unabsorbed_facts(&self, min_importance: i32) -> Vec<Fact> {
        self.get_facts()
            .into_iter()
            .filter(|f| !f.absorbed && f.importance >= min_importance)
            .collect()
    }

    /// 获取"待信号检测"的事实（未吸收 + 未参与过信号检测 + importance ≥ min_imp）
    /// 历史 bug：用 get_unabsorbed_facts 取信号检测候选 → 同一批事实每 3 分钟
    /// 重复打信号，reinforcement 无限膨胀 + 持续烧 LLM token
    pub fn get_pending_signal_facts(&self, min_importance: i32, max: usize) -> Vec<Fact> {
        self.get_facts()
            .into_iter()
            .filter(|f| !f.absorbed && !f.signals_checked && f.importance >= min_importance)
            .take(max)
            .collect()
    }

    /// 标记一批事实"已完成信号检测"（无论是否命中信号，避免重复判定）
    pub fn mark_facts_signals_checked(&self, fact_ids: &[String]) -> Result<(), String> {
        let _g = self.global_lock();
        let mut facts = self.read_json(&self.facts_path(), json!([]));
        let arr = facts.as_array_mut().ok_or("facts.json 不是数组")?;
        let mut changed = false;
        for f in arr.iter_mut() {
            if let Some(id) = f.get("id").and_then(|i| i.as_str()) {
                if fact_ids.contains(&id.to_string()) {
                    f["signals_checked"] = json!(true);
                    changed = true;
                }
            }
        }
        if changed {
            self.atomic_write(
                &self.facts_path(),
                &serde_json::to_string_pretty(&facts).map_err(|e| e.to_string())?,
            )?;
        }
        Ok(())
    }

    /// 对反思应用一条证据信号（reinforces / negates）
    /// delta: 正=reinforce，负=negate
    pub fn apply_signal(&self, reflection_id: &str, delta: f64) -> Result<(), String> {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        self.update_reflection(reflection_id, |r| {
            if delta >= 0.0 {
                r.reinforcement += delta;
                r.rein_last_signal_at = Some(now.clone());
            } else {
                r.disputation += -delta;
                r.disp_last_signal_at = Some(now.clone());
            }
        })?;
        Ok(())
    }

    /// 计算反思当前证据分数
    pub fn reflection_score(&self, r: &Reflection) -> f64 {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        evidence_score(
            r.reinforcement,
            r.disputation,
            r.rein_last_signal_at.as_deref(),
            r.disp_last_signal_at.as_deref(),
            &now,
        )
    }

    /// 晋升扫描：pending → confirmed（score≥1.0 或满 7 天），confirmed → promoted（score≥2.0 或再满 7 天）
    /// 返回晋升统计 (confirmed_count, promoted_count)
    pub fn promote_reflections(&self) -> (usize, usize) {
        let mut all = self.get_reflections();
        let mut confirmed_count = 0;
        let mut promoted_count = 0;
        let now = chrono::Local::now();
        let now_iso = now.format("%Y-%m-%d %H:%M:%S").to_string();

        for r in all.iter_mut() {
            if r.status == "archived" {
                continue;
            }
            let score = evidence_score(
                r.reinforcement,
                r.disputation,
                r.rein_last_signal_at.as_deref(),
                r.disp_last_signal_at.as_deref(),
                &now_iso,
            );
            let created_age_days = days_between(&r.created_at, &now_iso);
            let confirmed_age_days = r
                .confirmed_at
                .as_deref()
                .map(|t| days_between(t, &now_iso))
                .unwrap_or(i64::MAX);

            if r.status == "pending" {
                // score ≥ 1.0（证据驱动）或满 7 天（时间 fallback）
                if score >= EVIDENCE_CONFIRMED_THRESHOLD
                    || created_age_days >= WEAK_MEMORY_AUTO_CONFIRM_DAYS
                {
                    r.status = "confirmed".to_string();
                    r.confirmed_at = Some(now_iso.clone());
                    confirmed_count += 1;
                } else if score <= EVIDENCE_ARCHIVE_THRESHOLD {
                    r.status = "archived".to_string();
                }
            } else if r.status == "confirmed" {
                // score ≥ 2.0（证据驱动）或 confirmed 后满 7 天（时间 fallback）
                if score >= EVIDENCE_PROMOTED_THRESHOLD
                    || confirmed_age_days >= WEAK_MEMORY_AUTO_PROMOTE_DAYS
                {
                    r.status = "promoted".to_string();
                    promoted_count += 1;
                    // 合入 persona
                    let _ = self.add_persona_fact(&r.entity, &r.text);
                } else if score <= EVIDENCE_ARCHIVE_THRESHOLD {
                    r.status = "archived".to_string();
                }
            }
        }
        let _ = self.save_reflections(&all);
        (confirmed_count, promoted_count)
    }

    // ---------- 近期对话 ----------

    /// 追加一轮对话（user + assistant），保持最近 MAX_RECENT 轮
    pub fn append_recent(&self, role: &str, content: &str) -> Result<(), String> {
        const MAX_RECENT: usize = 20;
        let _g = self.global_lock();

        let mut recent = self.read_json(&self.recent_path(), json!([]));
        let arr = recent.as_array_mut().ok_or("recent.json 不是数组")?;
        arr.push(json!({
            "role": role,
            "content": content,
            "ts": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        }));
        // 只保留最近 MAX_RECENT 条
        let len = arr.len();
        if len > MAX_RECENT {
            *arr = arr[len - MAX_RECENT..].to_vec();
        }
        self.atomic_write(
            &self.recent_path(),
            &serde_json::to_string_pretty(&recent).map_err(|e| e.to_string())?,
        )
    }

    /// 读取近期对话（返回 [{role, content, ts}...] 数组）
    pub fn get_recent(&self) -> Vec<Value> {
        let v = self.read_json(&self.recent_path(), json!([]));
        v.as_array().cloned().unwrap_or_default()
    }

    /// 读取近期对话纯文本（用于事实提取的 CONVERSATION 输入）
    pub fn get_recent_conversation_text(&self, max_chars: usize) -> String {
        let mut out = String::new();
        for m in self.get_recent() {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let speaker = if role == "user" { "主人" } else { "优香" };
            let line = format!("{}: {}\n", speaker, content);
            if out.len() + line.len() > max_chars {
                break;
            }
            out.push_str(&line);
        }
        out
    }

    // ---------- 人格记忆 ----------

    /// 读取人格：{master: [text,...], neko: [text,...], relationship: [text,...]}
    pub fn get_persona(&self) -> Value {
        self.read_json(&self.persona_path(), json!({"master": [], "neko": [], "relationship": []}))
    }

    /// 追加一条人格设定（去重）
    pub fn add_persona_fact(&self, entity: &str, text: &str) -> Result<bool, String> {
        let _g = self.global_lock();
        let mut persona = self.get_persona();
        let section = persona[entity]
            .as_array_mut()
            .ok_or("persona section 不是数组")?;
        if section.iter().any(|e| e.get("text").and_then(|t| t.as_str()) == Some(text)) {
            return Ok(false);
        }
        section.push(json!({
            "text": text,
            "source": "manual",
            "ts": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        }));
        self.atomic_write(
            &self.persona_path(),
            &serde_json::to_string_pretty(&persona).map_err(|e| e.to_string())?,
        )?;
        Ok(true)
    }

    // ---------- 记忆注入（llm_chat 用） ----------

    /// 组装记忆上下文块：人格 + 长期事实（importance 排序截断）
    /// 注入到 system prompt 末尾
    pub fn render_memory_context(&self, max_facts: usize, max_chars: usize) -> String {
        let mut out = String::new();

        // 1. 人格 section
        let persona = self.get_persona();
        for (entity, label) in [
            ("master", "关于主人"),
            ("neko", "关于优香"),
            ("relationship", "关于我们的关系"),
        ] {
            if let Some(arr) = persona.get(entity).and_then(|a| a.as_array()) {
                let texts: Vec<String> = arr
                    .iter()
                    .filter_map(|e| e.get("text").and_then(|t| t.as_str()).map(String::from))
                    .collect();
                if !texts.is_empty() {
                    let mut section = format!("【{}】\n", label);
                    for t in texts {
                        section.push_str(&format!("- {}\n", t));
                    }
                    if out.len() + section.len() <= max_chars {
                        out.push_str(&section);
                    }
                }
            }
        }

        // 2. 长期事实（importance 排序，已吸收+未吸收都注入）
        //    历史 bug：只注入未吸收（!absorbed）→ 反思合成把事实 mark absorbed 后
        //    既不注入也不进 persona（promote 门槛高）→ 记忆"中间态丢失"，优香啥都不记得
        let facts = self.get_facts();
        let mut top: Vec<&Fact> = facts.iter().filter(|f| f.importance >= 5).collect();
        // 按 importance DESC，再按创建时间 DESC（同分取最新）
        top.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then(b.created_at.cmp(&a.created_at))
        });
        // 相似度去重：同一事实的不同版本（"主人叫小李" vs "主人的名字叫小李"）只留一条，
        // 防止重复信息占满注入额度。方法：去填充字后子串包含（名字不同不会误杀）
        let mut kept: Vec<&Fact> = Vec::new();
        for f in top {
            let dup = kept.iter().any(|k| similar_enough(&k.text, &f.text));
            if !dup {
                kept.push(f);
                if kept.len() >= max_facts {
                    break;
                }
            }
        }
        if !kept.is_empty() {
            let mut section = String::from("【记住的事】\n");
            for f in kept {
                let label = crate::memory_prompts::entity_label(&f.entity);
                let days = days_since(&f.created_at);
                let ago = crate::memory_prompts::time_since_label(days);
                section.push_str(&format!("- [{}] {} ({})\n", label, f.text, ago));
            }
            if out.len() + section.len() <= max_chars {
                out.push_str(&section);
            }
        }

        // 3. 反思（pending + confirmed 都注入，按证据分数排序；score ≤ 0 不渲染）
        //    历史 bug：只注入 confirmed → pending 反思（其实是最有价值的总结）全被过滤
        let reflections = self.get_reflections();
        let mut candidates: Vec<&Reflection> = reflections
            .iter()
            .filter(|r| r.status == "confirmed" || r.status == "pending")
            .collect();
        candidates.sort_by(|a, b| {
            self.reflection_score(b)
                .partial_cmp(&self.reflection_score(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let candidates: Vec<&Reflection> = candidates
            .into_iter()
            .filter(|r| self.reflection_score(r) > 0.0)
            .take(5)
            .collect();
        if !candidates.is_empty() {
            let mut section = String::from("【关于你的印象】\n");
            for r in candidates {
                let label = crate::memory_prompts::entity_label(&r.entity);
                section.push_str(&format!("- [{}] {}\n", label, r.text));
            }
            if out.len() + section.len() <= max_chars {
                out.push_str(&section);
            }
        }

        out
    }

    /// 按关键词简单召回（L1 用子串/包含匹配，L2 换 BM25）
    /// 返回渲染好的编号块文本
    pub fn recall(&self, query: &str, max_items: usize) -> String {
        let q = query.to_lowercase();
        let q_chars: Vec<char> = q.chars().collect();
        let mut hits: Vec<(i32, String, String, String)> = Vec::new(); // (importance, text, entity, created_at)

        // 事实库
        for f in self.get_facts() {
            let t = f.text.to_lowercase();
            let score = overlap_score(&q_chars, &t.chars().collect::<Vec<_>>());
            if score > 0 {
                hits.push((f.importance, f.text, f.entity, f.created_at));
            }
        }
        // 人格库
        let persona = self.get_persona();
        if let Some(arr) = persona.get("master").and_then(|a| a.as_array()) {
            for e in arr {
                if let Some(t) = e.get("text").and_then(|x| x.as_str()) {
                    let tl = t.to_lowercase();
                    if tl.contains(&q) || q.contains(&tl) || overlap_score(&q_chars, &tl.chars().collect::<Vec<_>>()) > 0 {
                        hits.push((10, t.to_string(), "master".to_string(), String::new()));
                    }
                }
            }
        }

        // 按 importance DESC 排序，去重文本，渲染
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        let mut seen = std::collections::HashSet::new();
        let mut lines: Vec<String> = Vec::new();
        for (imp, text, entity, created_at) in hits {
            if !seen.insert(text.clone()) {
                continue;
            }
            let label = crate::memory_prompts::entity_label(&entity);
            let ago = if created_at.is_empty() {
                String::new()
            } else {
                format!(" ({})", crate::memory_prompts::time_since_label(days_since(&created_at)))
            };
            lines.push(format!("{}. [{}] {}{}", lines.len() + 1, label, text, ago));
            if lines.len() >= max_items {
                break;
            }
            let _ = imp;
        }
        if lines.is_empty() {
            return String::new();
        }
        format!("【回忆到的】\n{}", lines.join("\n"))
    }

    // ---------- 事实去重仲裁（抄 N.E.K.O. memory/fact_dedup.py FactDedupResolver）----------
    // 不是规则硬去重（"主人喜欢猫" vs "主人讨厌猫" 相似度极高但语义相反），
    // 而是：相似对 → 排队 → LLM 判 merge/replace/keep_both → 应用

    /// 找候选去重对：同 entity + 字符 bigram 相似度 > 阈值
    /// 返回 (existing=created_at 早, candidate=created_at 晚) 对，按相似度降序，一条事实只用一次
    pub fn find_dedup_candidates(&self, max_pairs: usize) -> Vec<(Fact, Fact)> {
        let facts = self.get_facts();
        let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
        for i in 0..facts.len() {
            for j in (i + 1)..facts.len() {
                if facts[i].entity == facts[j].entity && facts[i].id != facts[j].id {
                    let s = crate::proactive::similarity(&facts[i].text, &facts[j].text);
                    if s > 0.5 {
                        pairs.push((s, i, j));
                    }
                }
            }
        }
        pairs.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut used: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut result: Vec<(Fact, Fact)> = Vec::new();
        for (_, i, j) in pairs {
            if used.contains(&i) || used.contains(&j) {
                continue;
            }
            used.insert(i);
            used.insert(j);
            let (existing, candidate) = if facts[i].created_at <= facts[j].created_at {
                (facts[i].clone(), facts[j].clone())
            } else {
                (facts[j].clone(), facts[i].clone())
            };
            result.push((existing, candidate));
            if result.len() >= max_pairs {
                break;
            }
        }
        result
    }

    /// LLM 仲裁去重：候选对 → LLM 判 merge/replace/keep_both → 应用
    /// merge: 保留 existing（importance = max 两者），删 candidate
    /// replace: candidate 更准确/更新 → 保留 candidate，删 existing
    /// keep_both: 不动（"喜欢" vs "讨厌" 必须都留）
    /// 返回 (merged, replaced, kept)
    pub async fn resolve_fact_dedup(&self, api_key: &str) -> Result<(usize, usize, usize), String> {
        // batch 10（N.E.K.O. 上限 20，我们 10 更稳：LLM 长 prompt 响应慢）
        let pairs = self.find_dedup_candidates(10);
        if pairs.is_empty() {
            return Ok((0, 0, 0));
        }

        // 组装 prompt（抄 N.E.K.O. FACT_DEDUP_PROMPT）
        let pairs_text: Vec<String> = pairs
            .iter()
            .enumerate()
            .map(|(idx, (e, c))| {
                format!(
                    "{}.\nexisting({}): {}\ncandidate({}): {}",
                    idx + 1,
                    e.id,
                    e.text,
                    c.id,
                    c.text
                )
            })
            .collect();
        let prompt = crate::memory_prompts::FACT_DEDUP_PROMPT
            .replace("{COUNT}", &pairs.len().to_string())
            .replace("{PAIRS}", &pairs_text.join("\n"));

        let body = serde_json::json!({
            "model": crate::chat_model(),
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
            "response_format": {"type": "json_object"}
        });
        // 独立 Client：仲裁是重任务，共享 Client 60s 超时不够（实测 dashscope 响应 >60s）
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("构建 Client 失败: {}", e))?;
        let resp = client
            .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("去重仲裁请求失败: {}", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("去重仲裁返回 {}: {}", status, text));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("去重仲裁响应解析失败: {}", e))?;
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| "去重仲裁无内容".to_string())?;
        // 容错：去 ```json 围栏
        let cleaned = content
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        let parsed: serde_json::Value = serde_json::from_str(cleaned).map_err(|e| {
            format!(
                "仲裁结果解析失败: {}（原始: {}）",
                e,
                content.chars().take(200).collect::<String>()
            )
        })?;
        // 兼容数组或 {"results": [...]}
        let arr: Vec<serde_json::Value> = parsed
            .as_array()
            .cloned()
            .unwrap_or_else(|| {
                parsed
                    .get("results")
                    .and_then(|r| r.as_array())
                    .cloned()
                    .unwrap_or_default()
            });

        // 收集裁决
        let mut decisions: Vec<(usize, String)> = Vec::new();
        for item in arr {
            let idx = item.get("index").and_then(|i| i.as_i64()).unwrap_or(-1) as usize;
            let action = item.get("action").and_then(|a| a.as_str()).unwrap_or("");
            if idx < pairs.len() && (action == "merge" || action == "replace") {
                decisions.push((idx, action.to_string()));
            }
        }

        // 应用（一次性读写，全局锁内）
        let _g = self.global_lock();
        let mut facts = self.read_json(&self.facts_path(), json!([]));
        let arr = facts.as_array_mut().ok_or("facts.json 不是数组")?;
        let mut merged = 0usize;
        let mut replaced = 0usize;
        let kept = pairs.len() - decisions.len();
        for (idx, action) in &decisions {
            let (existing, candidate) = &pairs[*idx];
            if action == "merge" {
                // 保留 existing，importance 取两者 max（比 N.E.K.O. +1 更稳，防虚高）
                if let Some(ef) = arr.iter_mut().find(|f| {
                    f.get("id").and_then(|i| i.as_str()) == Some(existing.id.as_str())
                }) {
                    let cur = ef.get("importance").and_then(|v| v.as_i64()).unwrap_or(0);
                    let cand_imp = candidate.importance as i64;
                    ef["importance"] = json!(cur.max(cand_imp));
                }
                arr.retain(|f| {
                    f.get("id").and_then(|i| i.as_str()) != Some(candidate.id.as_str())
                });
                merged += 1;
            } else {
                // replace：保留 candidate（更准确/更新），删 existing
                arr.retain(|f| {
                    f.get("id").and_then(|i| i.as_str()) != Some(existing.id.as_str())
                });
                replaced += 1;
            }
        }
        if merged > 0 || replaced > 0 {
            self.atomic_write(
                &self.facts_path(),
                &serde_json::to_string_pretty(&facts).map_err(|e| e.to_string())?,
            )?;
        }
        Ok((merged, replaced, kept))
    }
}

/// 计算 ISO 时间到现在的天数
fn days_since(iso: &str) -> f64 {
    let Ok(dt) = chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S") else {
        return 0.0;
    };
    let now = chrono::Local::now().naive_local();
    (now - dt).num_days() as f64
}

/// 判断两条记忆文本是否"表达同一信息"（注入去重用）
/// 方法：①去掉中文填充字后，短者被长者包含 → 同一信息（"主人叫小李" ⊂ "主人的名字叫小李"）
/// ②或字符 bigram Dice 相似度 ≥ 0.85（轻微变体兜底）
/// 名字不同（"主人叫小李" vs "主人叫小明"）不会子串包含，Dice 0.75 < 0.85 → 不误杀
fn similar_enough(a: &str, b: &str) -> bool {
    // 中文填充字（去掉后只剩信息核心）
    let strip = |s: &str| -> String {
        s.chars()
            .filter(|c| !"的了在是一下和与及就都还很也这那被把为".contains(*c))
            .collect()
    };
    let ca = strip(a);
    let cb = strip(b);
    if ca.len() >= 4 && cb.contains(&ca) {
        return true;
    }
    if cb.len() >= 4 && ca.contains(&cb) {
        return true;
    }
    crate::proactive::similarity(a, b) >= 0.85
}

/// 简单重叠评分：查询词元与文本词元的交集比例（L1 简化版 BM25 替代）
fn overlap_score(q: &[char], t: &[char]) -> i32 {
    let q_words: Vec<String> = split_words(q);
    if q_words.is_empty() {
        return 0;
    }
    let t_text = t.iter().collect::<String>();
    let mut hit = 0;
    for w in &q_words {
        // 中文单字词元也参与匹配（历史 bug：w.len()>=2 按字节长度判断会把
        // 单字中文词元全滤掉 → 纯中文查询如"咖啡"永远零命中）
        if t_text.contains(w) {
            hit += 1;
        }
    }
    hit
}

/// 拆分词元：连续 ASCII 字母数字为一个词；连续中文按 2-gram 滑动窗口切分
/// （单字查询保留单字词元，保证"咖啡"这类短查询能命中）
fn split_words(chars: &[char]) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cjk: Vec<char> = Vec::new();

    // 把累积的中文段转成 2-gram 词元
    let flush_cjk = |words: &mut Vec<String>, cjk: &mut Vec<char>| {
        match cjk.len() {
            0 => {}
            1 => words.push(cjk[0].to_string()),
            _ => {
                for i in 0..cjk.len() - 1 {
                    let mut g = String::with_capacity(2);
                    g.push(cjk[i]);
                    g.push(cjk[i + 1]);
                    words.push(g);
                }
            }
        }
        cjk.clear();
    };

    for &c in chars {
        if c.is_ascii_alphanumeric() {
            flush_cjk(&mut words, &mut cjk);
            cur.push(c);
        } else if c.is_ascii() {
            flush_cjk(&mut words, &mut cjk);
            if !cur.is_empty() {
                words.push(cur.clone());
                cur.clear();
            }
        } else {
            // 中文等 CJK 字符：先落 ASCII 词，再入 CJK 缓冲
            if !cur.is_empty() {
                words.push(cur.clone());
                cur.clear();
            }
            cjk.push(c);
        }
    }
    flush_cjk(&mut words, &mut cjk);
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> MemoryStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dagent_mem_test_{}_{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        MemoryStore::new(dir)
    }

    #[test]
    fn test_add_fact_dedup() {
        let s = tmp_store();
        assert!(s.add_fact("主人喜欢喝咖啡", 7, "master").unwrap());
        // 完全重复 → 跳过
        assert!(!s.add_fact("主人喜欢喝咖啡", 7, "master").unwrap());
        assert!(s.add_fact("主人是大学生", 8, "master").unwrap());
        assert_eq!(s.get_facts().len(), 2);
    }

    #[test]
    fn test_facts_sorted_by_importance() {
        let s = tmp_store();
        s.add_fact("低优先级", 3, "master").unwrap();
        s.add_fact("高优先级", 9, "master").unwrap();
        let facts = s.get_facts();
        assert_eq!(facts[0].importance, 9);
        assert_eq!(facts[0].text, "高优先级");
    }

    #[test]
    fn test_recent_window() {
        let s = tmp_store();
        for i in 0..30 {
            s.append_recent("user", &format!("消息{}", i)).unwrap();
        }
        let recent = s.get_recent();
        assert_eq!(recent.len(), 20); // 只保留最近 20 条
        assert!(recent[0].get("content").unwrap().as_str().unwrap().starts_with("消息10"));
    }

    #[test]
    fn test_recall_hits() {
        let s = tmp_store();
        s.add_fact("主人喜欢喝咖啡，每天至少两杯", 8, "master").unwrap();
        s.add_fact("主人最近在学 Rust", 7, "master").unwrap();
        let result = s.recall("咖啡", 5);
        assert!(result.contains("咖啡"), "recall 应命中咖啡: {}", result);
        let result2 = s.recall("Rust", 5);
        assert!(result2.contains("Rust"), "recall 应命中 Rust: {}", result2);
        // 无关查询应返回空
        let result3 = s.recall("完全无关的话题xyz", 5);
        assert_eq!(result3, "", "无关查询应返回空: {}", result3);
    }

    #[test]
    fn test_render_memory_context() {
        let s = tmp_store();
        s.add_fact("主人喜欢喝咖啡", 8, "master").unwrap();
        s.add_persona_fact("master", "用户是大二学生").unwrap();
        let ctx = s.render_memory_context(8, 2000);
        assert!(ctx.contains("喜欢喝咖啡"));
        assert!(ctx.contains("大二学生"));
    }

    #[test]
    fn test_time_label() {
        assert_eq!(crate::memory_prompts::time_since_label(0.5), "今天");
        assert_eq!(crate::memory_prompts::time_since_label(3.0), "3 天前");
        assert_eq!(crate::memory_prompts::time_since_label(14.0), "2 周前");
        assert_eq!(crate::memory_prompts::time_since_label(90.0), "3 个月前");
    }

    #[test]
    fn test_render_real_memory() {
        // 调试用：渲染用户真实记忆数据（验证注入修复后"关于我"有内容）
        let store = MemoryStore::new(std::path::PathBuf::from(
            r"C:\Users\20728\AppData\Roaming\com.tauri-app.dagent\memory",
        ));
        let ctx = store.render_memory_context(30, 5000);
        println!("=== 真实记忆渲染 ===\n{}", ctx);
        assert!(!ctx.trim().is_empty(), "记忆渲染为空——注入仍有问题");
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_resolve_fact_dedup_real() {
        // 手动运行: cargo test e2e_resolve_fact_dedup_real -- --ignored --nocapture
        // 对用户真实记忆跑一次 LLM 仲裁去重（验证 merge/replace 效果）
        let store = MemoryStore::new(std::path::PathBuf::from(
            r"C:\Users\20728\AppData\Roaming\com.tauri-app.dagent\memory",
        ));
        let before = store.get_facts().len();
        println!("去重前事实数: {}", before);
        let candidates = store.find_dedup_candidates(20);
        println!("候选对: {}", candidates.len());
        for (i, (e, c)) in candidates.iter().enumerate() {
            println!("  对{}: [{}] {}  ~  [{}] {}", i + 1, e.id, e.text, c.id, c.text);
        }
        let cfg = std::fs::read_to_string(r"D:\3Dagent\3dagent\config.json").unwrap();
        let v: serde_json::Value = serde_json::from_str(&cfg).unwrap();
        let key = v["qianwen_api_key"].as_str().unwrap().to_string();
        let (m, r, k) = store.resolve_fact_dedup(&key).await.expect("仲裁失败");
        println!("仲裁结果: merge {} 条, replace {} 条, keep {} 对", m, r, k);
        let after = store.get_facts().len();
        println!("去重后事实数: {}（减少 {}）", after, before - after);
        let ctx = store.render_memory_context(30, 5000);
        println!("=== 去重后渲染 ===\n{}", ctx);
    }
}
