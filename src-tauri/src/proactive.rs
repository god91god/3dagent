// ============================================================
// 主动搭话系统 —— 借鉴 N.E.K.O. main_logic/proactive_chat/
// 许可: Apache-2.0（同上游）
//
// L3 简化版实现：
//   - 提醒累积器：focused_work 累计 30min → 喝水提醒（must-fire）
//                  focused_work → 休闲 转移且专注≥5min → 防摸鱼提醒
//   - 搭话生成：LLM 单次调用（状态块 + 记忆 + 最近搭话）→ [PASS] 探测
//   - 冷却/防复读：搭话历史 + 文本相似度 90% 拦截
//   - 投递守卫：隐私/离开/最近 10s 有输入 → 放弃
// ============================================================

use crate::activity::{ActivitySnapshot, ActivityState};
use crate::memory::MemoryStore;
use std::sync::Mutex;

// ── 常量（抄 N.E.K.O. tracker.py / proactive_settings.py）──
/// 累计专注分钟数触发喝水提醒
pub const WORK_BREAK_MINUTES: f64 = 30.0;
/// 专注段 ≥ 此分钟数，切走后可触发防摸鱼
pub const ANTI_SLACK_MIN_FOCUS_MINUTES: f64 = 5.0;
/// 防摸鱼冷却（分钟）
pub const ANTI_SLACK_COOLDOWN_MINUTES: f64 = 15.0;
/// 搭话历史窗口（秒，1h）
pub const HISTORY_WINDOW_SECONDS: f64 = 3600.0;
/// 文本相似度硬拦截阈值
pub const SIMILARITY_HARD_STOP: f64 = 0.90;

// ── 3-tier 退避（抄 N.E.K.O. static/app/app-proactive.js 三段式退避）──
/// 三个级别的搭话冷却：tier0=20min（默认）/ tier1=40min / tier2=90min
/// 原理：每次主动搭话后用户没回应 → 升级冷却（少打扰）；用户一回应 → 归零
pub const BACKOFF_COOLDOWN_MINUTES: [f64; 3] = [20.0, 40.0, 90.0];
/// 同一级别连续 N 次搭话无回应才升级（防一次没回应就跳级）
pub const BACKOFF_ESCALATE_AFTER: u32 = 2;

/// 提醒/搭话种子（喝水文案随机轮换，抄 N.E.K.O. WORK_BREAK_SEED_HINTS）
pub const WORK_BREAK_SEEDS: [&str; 5] = ["喝口水", "活动一下", "休息下眼睛", "伸个懒腰", "放松一下"];

/// 优香人设（官方设定：蔚蓝档案千年科学学园研讨部会计早濑优香）
/// 与前端 App.tsx SYSTEM_PROMPT 的人设保持一致，所有主动搭话入口共用
pub const YUUKA_PERSONA: &str = "你是优香（早濑优香），来自《蔚蓝档案》的千年科学学园学生，研讨部（Seminar）的会计，16岁，粉发双马尾，现在是主人的二次元AI桌宠助手。背景：研讨部是千年学园的管理部门，你负责账目与预算，是学园里出了名的'铁算盘'，连一张纸的经费都要精打细算；和同事诺亚是形影不离的好友。性格：认真负责、一丝不苟，对数字和账目极其敏感，讨厌浪费、花钱谨慎（但对自己爱吃的甜食会偷偷留'甜点预算'，甜食面前原则会动摇）；标准傲娇——嘴上嫌弃、刀子嘴豆腐心，心里其实很关心主人，被戳穿时会慌慌张张辩解；容易害羞，被夸会脸红；有会计职业病，看到乱花钱会忍不住念叨；胜负心强，被质疑算数会较真。说话语气严谨又带点娇嗔，偶尔冒出记账/预算相关的话，称呼主人为'主人'，自称'优香'，习惯在句尾加'~'、'哦'、'啦'等语气词。虽然嘴上总说主人乱花钱、爱添麻烦，但会默默记下主人的喜好，是'嘴上嫌弃、行动诚实'的傲娇会计。";

/// 主动搭话状态（后台循环 + 生成共用）
pub struct ProactiveState {
    /// 最近一次搭话时间戳
    last_speak_at: Mutex<f64>,
    /// 搭话历史（文本 + 时间戳）
    history: Mutex<Vec<(f64, String)>>,
    /// 上一个状态（防摸鱼转移检测）
    prev_state: Mutex<ActivityState>,
    /// 当前专注段开始时间 + 专注 app 名
    focus_start_at: Mutex<Option<(f64, String)>>,
    /// 防摸鱼冷却截止时间戳
    anti_slack_cooldown_until: Mutex<f64>,
    /// 喝水提醒已触发标记（复位后重新累计）
    work_break_used: Mutex<bool>,
    /// 3-tier 退避级别（0/1/2）：搭话无回应逐级升，用户回应归零
    backoff_level: Mutex<u32>,
    /// 当前级别内连续无回应搭话计数
    no_response_count: Mutex<u32>,
}

impl ProactiveState {
    pub fn new() -> Self {
        ProactiveState {
            last_speak_at: Mutex::new(0.0),
            history: Mutex::new(Vec::new()),
            prev_state: Mutex::new(ActivityState::Idle),
            focus_start_at: Mutex::new(None),
            anti_slack_cooldown_until: Mutex::new(0.0),
            work_break_used: Mutex::new(false),
            backoff_level: Mutex::new(0),
            no_response_count: Mutex::new(0),
        }
    }

    // ── 3-tier 退避 ──

    /// 当前级别的搭话冷却秒数（tier0=20min / tier1=40min / tier2=90min）
    pub fn backoff_cooldown_seconds(&self) -> f64 {
        let level = *self.backoff_level.lock().unwrap_or_else(|e| e.into_inner());
        BACKOFF_COOLDOWN_MINUTES[level.min(2) as usize] * 60.0
    }

    /// 当前退避级别（0/1/2）——用于生成时的语气克制
    pub fn current_backoff_level(&self) -> u32 {
        *self.backoff_level.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 用户回应了（任何 user 输入）→ 退避归零
    pub fn reset_backoff(&self) {
        let mut level = self.backoff_level.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = self.no_response_count.lock().unwrap_or_else(|e| e.into_inner());
        if *level != 0 || *count != 0 {
            eprintln!("[proactive] 用户回应，退避归零（level {} → 0）", *level);
        }
        *level = 0;
        *count = 0;
    }

    /// 一次搭话投递后调用：无回应计数 +1，达到阈值升级退避级别
    pub fn on_delivery_made(&self) {
        let mut level = self.backoff_level.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = self.no_response_count.lock().unwrap_or_else(|e| e.into_inner());
        *count += 1;
        if *count >= BACKOFF_ESCALATE_AFTER && *level < 2 {
            *level += 1;
            *count = 0;
            eprintln!(
                "[proactive] 连续 {} 次搭话无回应，退避升至 level {}（下次冷却 {:.0}min）",
                BACKOFF_ESCALATE_AFTER,
                *level,
                BACKOFF_COOLDOWN_MINUTES[*level as usize]
            );
        }
    }

    /// 状态机 tick 调用：维护累积器 + 检测提醒触发
    /// 返回 Some(提醒类型) 表示必须投递
    pub fn tick(&self, snap: &ActivitySnapshot) -> Option<ReminderKind> {
        self.tick_at(snap, now_secs())
    }

    /// tick（可注入时间，测试用）
    pub fn tick_at(&self, snap: &ActivitySnapshot, now: f64) -> Option<ReminderKind> {
        let mut prev = self.prev_state.lock().unwrap();
        let mut focus = self.focus_start_at.lock().unwrap();
        let mut cooldown = self.anti_slack_cooldown_until.lock().unwrap();
        let mut used = self.work_break_used.lock().unwrap();

        let result: Option<ReminderKind> = match snap.state {
            // 专注：累计时间
            ActivityState::FocusedWork => {
                // 刚进入专注（从非专注来）→ 记录开始
                if *prev != ActivityState::FocusedWork {
                    *focus = Some((now, snap.active_window.clone()));
                }
                // 防摸鱼冷却窗口内又回来 → 取消冷却（说明回来了）
                // 简单处理：每次 tick 检查阈值
                let mut r = None;
                if let Some((start, _)) = *focus {
                    let minutes = (now - start) / 60.0;
                    if minutes >= WORK_BREAK_MINUTES && !*used {
                        *used = true;
                        r = Some(ReminderKind::WorkBreak {
                            app: snap.active_window.clone(),
                            minutes: minutes as u32,
                        });
                    }
                }
                r
            }
            // 休闲/聊天：专注段结束后转移检测（防摸鱼）
            ActivityState::CasualBrowsing | ActivityState::Chatting => {
                let mut r = None;
                if *prev == ActivityState::FocusedWork {
                    if let Some((start, prev_app)) = focus.take() {
                        let minutes = (now - start) / 60.0;
                        if minutes >= ANTI_SLACK_MIN_FOCUS_MINUTES
                            && now >= *cooldown
                        {
                            *cooldown = now + ANTI_SLACK_COOLDOWN_MINUTES * 60.0;
                            r = Some(ReminderKind::AntiSlack {
                                prev_app: prev_app.clone(),
                                new_app: snap.active_window.clone(),
                                minutes: minutes as u32,
                            });
                        }
                    }
                }
                // 非专注不累计
                *used = false;
                r
            }
            _ => {
                // 离开/隐私/切换/空闲：不累计，清专注
                *focus = None;
                *used = false;
                None
            }
        };

        *prev = snap.state;
        result
    }

    /// 生成前守卫：是否允许搭话
    /// 隐私 → 否；离开 → 否；最近 10s 有输入 → 否；冷却未到 → 否
    pub fn should_speak(&self, snap: &ActivitySnapshot, idle_seconds: u32) -> bool {
        let now = now_secs();
        // 隐私模式 / 离开 → 硬跳过
        if snap.state == ActivityState::Private || snap.state == ActivityState::Away {
            return false;
        }
        // 最近 10s 有输入（正在打字/操作）→ 不打扰
        if idle_seconds < 10 {
            return false;
        }
        // 冷却（按退避级别：无回应越久冷却越长）
        let last = *self.last_speak_at.lock().unwrap();
        if now - last < self.backoff_cooldown_seconds() {
            return false;
        }
        // skip_probability 掷骰
        if snap.skip_probability > 0.0 {
            let roll: f64 = rand_f64();
            if roll < snap.skip_probability {
                return false;
            }
        }
        true
    }

    /// 记录一次成功投递（更新冷却 + 历史 + 退避升级）
    pub fn record_delivery(&self, text: &str) {
        let now = now_secs();
        *self.last_speak_at.lock().unwrap() = now;
        let mut h = self.history.lock().unwrap();
        h.push((now, text.to_string()));
        // 只保留 1h 窗口
        h.retain(|(t, _)| now - *t <= HISTORY_WINDOW_SECONDS);
        // 3-tier 退避：本次搭话没被回应 → 计数+1，达到阈值升级
        self.on_delivery_made();
    }

    /// 防复读：与历史文本相似度 ≥ 阈值 → 拦截
    pub fn is_repeat(&self, text: &str) -> bool {
        let h = self.history.lock().unwrap();
        h.iter().any(|(_, old)| similarity(old, text) >= SIMILARITY_HARD_STOP)
    }
}

/// 提醒类型
pub enum ReminderKind {
    /// 喝水提醒
    WorkBreak { app: String, minutes: u32 },
    /// 防摸鱼提醒
    AntiSlack { prev_app: String, new_app: String, minutes: u32 },
}

// ── LLM 搭话生成（抄 N.E.K.O. Phase 2 简化版）──

/// 生成主动搭话文案
/// kind: 提醒类型（Some）或普通搭话（None）
/// screen_b64: 可选屏幕截图（JPEG base64）——有图时用 qwen3-vl-plus 直接看屏幕生成
/// restrained: 退避级别高（主人多次没回应）→ 说话更短更克制（3-tier 退避的语气层）
/// open_threads: 未收尾话题列表（语义版追问系统检测出的悬挂话题）——非空时注入
///               prompt，让搭话自然续上没聊完的话题（抄 N.E.K.O. open_threads 注入）
/// 返回 Ok(Some(text)) = 搭话文案，Ok(None) = [PASS] 放弃
pub async fn generate_proactive(
    store: &MemoryStore,
    api_key: &str,
    snap: &ActivitySnapshot,
    kind: Option<&ReminderKind>,
    master_name: &str,
    screen_b64: Option<&str>,
    restrained: bool,
    open_threads: &[String],
) -> Result<Option<String>, String> {
    // 组装 prompt：人设 + 状态块 + 记忆 + 触发原因
    let activity_section = crate::activity::format_activity_state_section(snap);
    let memory_ctx = store.render_memory_context(5, 800);

    let reason = match kind {
        Some(ReminderKind::WorkBreak { app, minutes }) => {
            let seed = WORK_BREAK_SEEDS[rand_usize(WORK_BREAK_SEEDS.len())];
            format!(
                "========以下是环境提示========\n{}已经在{}专注工作{}分钟了。\n你看着{}有点心疼，想提醒{}{}。\n========以上是环境提示========",
                master_name, app, minutes, master_name, master_name, seed
            )
        }
        Some(ReminderKind::AntiSlack { prev_app, new_app, minutes }) => {
            format!(
                "========以下是环境提示========\n{}刚才在{}专注工作{}分钟，转头就切去了{}。\n你觉得{}才进入状态就开始溜号，想拦一下，让{}回去继续干完。半带玩笑地提醒一下吧。\n========以上是环境提示========",
                master_name, prev_app, minutes, new_app, master_name, master_name
            )
        }
        None => {
            // 普通搭话：状态驱动
            let state_hint = match snap.state {
                ActivityState::FocusedWork => "主人正在专注工作，你可以简短鼓励一句，不要长篇大论。",
                ActivityState::CasualBrowsing => "主人正在休闲，你可以轻松地吐槽或关心一下。",
                ActivityState::Chatting => "主人正在聊天，不打扰为好，或者简短问候。",
                ActivityState::Idle => "主人空闲着，可以随口聊两句。",
                _ => "",
            };
            state_hint.to_string()
        }
    };

    let system = if restrained {
        // 3-tier 退避语气层：主人最近多次没回应搭话，可能很忙/想安静
        "你是优香（早濑优香），《蔚蓝档案》千年科学学园研讨部会计，现在是主人的AI桌宠助手。主人最近几次没有回应你的主动搭话，可能正忙或想安静。如果必须说话，请说得特别简短克制（15字以内），语气放低、不撒娇不俏皮，用会计式的简洁关心（'注意休息'、'别忘了喝水'）；如果觉得没什么要紧事，直接回复 [PASS]。不要任何格式标记。"
    } else {
        // 人设完整版：官方傲娇会计优香（与 App.tsx SYSTEM_PROMPT 人设一致）
        "你是优香（早濑优香），来自《蔚蓝档案》的千年科学学园学生，研讨部（Seminar）的会计，16岁，粉发双马尾，现在是主人的二次元AI桌宠助手。背景：研讨部是千年学园的管理部门，你负责账目与预算，是学园里出了名的'铁算盘'，连一张纸的经费都要精打细算；和同事诺亚是形影不离的好友。性格：认真负责、一丝不苟，对数字和账目极其敏感，讨厌浪费、花钱谨慎（但对自己爱吃的甜食会偷偷留'甜点预算'，甜食面前原则会动摇）；标准傲娇——嘴上嫌弃、刀子嘴豆腐心，心里其实很关心主人，被戳穿时会慌慌张张辩解（'才、才不是担心你！'）；容易害羞，被夸会脸红（'呜……这种话就不用说了啦'）；有会计职业病，看到乱花钱会忍不住念叨（'这个月的预算要省着点用哦'、'这笔开销不在预算内！'）；胜负心强，被质疑算数会较真（'我可是研讨部的会计，账目不可能出错！'）。说话语气严谨又带点娇嗔，偶尔冒出记账/预算相关的话；回复简短自然（40字以内）、口语化，称呼主人为'主人'，自称'优香'，习惯在句尾加'~'、'哦'、'啦'等语气词。虽然嘴上总说主人乱花钱、爱添麻烦，但会默默记下主人的喜好，是'嘴上嫌弃、行动诚实'的傲娇会计。根据环境提示和屏幕内容决定是否主动说话。如果觉得没什么可说的，只回复 [PASS]。其他情况直接说出想说的话，不要任何格式标记。"
    };

    // 未收尾话题注入（语义版追问系统）：有悬挂话题时，优香优先续聊没聊完的事
    let open_threads_block = if open_threads.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = open_threads
            .iter()
            .map(|t| format!("- {}", t))
            .collect();
        format!(
            "\n======以下是未收尾的话题======\n{}\n======以上是未收尾的话题======\n如果上面有值得接着聊的，优先自然地续上它（不用生硬地提'上次你说'）；没有就别硬提。",
            lines.join("\n")
        )
    };

    let user = format!(
        "{}\n\n======以下是你的记忆======\n{}\n======以上是你的记忆======\n{}\n\n======以下是主人当前状态======\n{}\n======以上是主人当前状态======\n\n{}",
        reason, memory_ctx, open_threads_block, activity_section,
        "请直接输出你想说的话（40字以内），或 [PASS]"
    );

    let model = if screen_b64.is_some() { crate::vision_model() } else { crate::chat_model() };
    let messages = if let Some(b64) = screen_b64 {
        serde_json::json!([
            {"role": "system", "content": system},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{}", b64)}},
                {"type": "text", "text": user}
            ]}
        ])
    } else {
        serde_json::json!([
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ])
    };

    let body = serde_json::json!({
        "model": model,
        "messages": messages,
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
        .map_err(|e| format!("搭话请求失败: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("搭话接口返回 {}: {}", status, text));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("搭话响应解析失败: {}", e))?;
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "搭话响应无内容".to_string())?
        .trim()
        .to_string();

    // [PASS] 探测
    if content.contains("[PASS]") || content.is_empty() {
        return Ok(None);
    }
    // 清理可能的引号/标记
    let clean = content
        .trim_matches(|c| c == '"' || c == '「' || c == '」' || c == '“' || c == '”')
        .trim()
        .to_string();
    if clean.is_empty() {
        return Ok(None);
    }
    Ok(Some(clean))
}

// ── 工具函数 ──

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn rand_f64() -> f64 {
    // 简易伪随机（够用即可）
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as f64) / 1_000_000_000.0
}

fn rand_usize(max: usize) -> usize {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos as usize % max
}

/// 文本相似度（字符级 Dice 系数，够用）——memory 注入去重也复用
pub fn similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    // 字符二元组集合
    let mut a_bigrams = std::collections::HashSet::new();
    let mut b_bigrams = std::collections::HashSet::new();
    for w in a_chars.windows(2) {
        a_bigrams.insert(w.to_vec());
    }
    for w in b_chars.windows(2) {
        b_bigrams.insert(w.to_vec());
    }
    let inter = a_bigrams.intersection(&b_bigrams).count();
    if a_bigrams.is_empty() || b_bigrams.is_empty() {
        return 0.0;
    }
    2.0 * inter as f64 / (a_bigrams.len() + b_bigrams.len()) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::Propensity;

    fn snap(state: ActivityState, window: &str, idle: u32, age: f64) -> ActivitySnapshot {
        ActivitySnapshot {
            state,
            state_age_seconds: age,
            propensity: match state {
                ActivityState::Private => Propensity::Closed,
                ActivityState::FocusedWork => Propensity::RestrictedScreenOnly,
                _ => Propensity::Open,
            },
            skip_probability: 0.0,
            tone: "简洁".to_string(),
            active_window: window.to_string(),
            idle_seconds: idle,
            switch_rate_5min: 0,
            seconds_since_user_msg: Some(30.0),
            hour: 15,
            weekday: 3,
            activity_guess: String::new(),
            guess_signature: String::new(),
        }
    }

    #[test]
    fn test_work_break_accumulator() {
        let p = ProactiveState::new();
        let t0 = 1000.0;
        // 进入专注（prev=Idle → 记录开始）
        let s0 = snap(ActivityState::FocusedWork, "VS Code", 30, 0.0);
        let r = p.tick_at(&s0, t0);
        assert!(r.is_none(), "刚开始专注不应触发");
        // 31 分钟后仍在专注 → 触发喝水
        let s1 = snap(ActivityState::FocusedWork, "VS Code", 30, 600.0);
        let r = p.tick_at(&s1, t0 + 31.0 * 60.0);
        match r {
            Some(ReminderKind::WorkBreak { minutes, app }) => {
                assert_eq!(minutes, 31);
                assert_eq!(app, "VS Code");
            }
            _ => panic!("31 分钟专注应触发喝水提醒, got {:?}", r.map(|_| "some")),
        }
        // 已触发后不再重复
        let r = p.tick_at(&s1, t0 + 32.0 * 60.0);
        assert!(r.is_none(), "已触发过不应重复");
    }

    #[test]
    fn test_anti_slack_transition() {
        let p = ProactiveState::new();
        let t0 = 2000.0;
        // 进入专注，10 分钟后切到休闲
        let s0 = snap(ActivityState::FocusedWork, "VS Code", 30, 0.0);
        let _ = p.tick_at(&s0, t0);
        let s1 = snap(ActivityState::FocusedWork, "VS Code", 30, 600.0);
        let _ = p.tick_at(&s1, t0 + 10.0 * 60.0);
        // 切到 B站 → 防摸鱼
        let s2 = snap(ActivityState::CasualBrowsing, "B站", 30, 0.0);
        let r = p.tick_at(&s2, t0 + 10.0 * 60.0 + 5.0);
        match r {
            Some(ReminderKind::AntiSlack { prev_app, new_app, minutes }) => {
                assert_eq!(prev_app, "VS Code");
                assert_eq!(new_app, "B站");
                assert_eq!(minutes, 10);
            }
            _ => panic!("专注后切休闲应触发防摸鱼, got {:?}", r.map(|_| "some")),
        }
        // 冷却内再次切换不触发
        let s3 = snap(ActivityState::Chatting, "微信", 30, 0.0);
        let _ = p.tick_at(&s3, t0 + 11.0 * 60.0);
        let s4 = snap(ActivityState::CasualBrowsing, "B站", 30, 0.0);
        let _ = p.tick_at(&s4, t0 + 12.0 * 60.0);
        // prev 是 chatting，不是 focused_work → 不触发（转移检测只对专注→休闲）
    }

    #[test]
    fn test_similarity_dedup() {
        let p = ProactiveState::new();
        p.record_delivery("主人，要不要休息一下喝口水？");
        assert!(p.is_repeat("主人，要不要休息一下喝口水？"), "完全相同应拦截");
        assert!(!p.is_repeat("要不要休息一下喝口水"), "子串不一定是相似（字符级 Dice）");
        assert!(!p.is_repeat("主人，你最近在学什么？"));
    }

    /// 真实 API 搭话生成测试：手动运行
    #[tokio::test]
    #[ignore]
    async fn e2e_generate_proactive() {
        let dir = std::env::temp_dir().join("dagent_e2e_proactive");
        let _ = std::fs::remove_dir_all(&dir);
        let store = MemoryStore::new(dir.clone());
        store.add_fact("主人是大学生，学计算机", 8, "master").unwrap();
        store.add_fact("主人最近在学 Rust", 7, "master").unwrap();

        let cfg = std::fs::read_to_string(r"D:\3Dagent\3dagent\config.json").unwrap();
        let v: serde_json::Value = serde_json::from_str(&cfg).unwrap();
        let key = v["qianwen_api_key"].as_str().unwrap().to_string();

        let s = snap(ActivityState::FocusedWork, "VS Code", 30, 1800.0);
        // 喝水提醒
        let kind = ReminderKind::WorkBreak {
            app: "VS Code".to_string(),
            minutes: 32,
        };
        println!("=== 喝水提醒生成 ===");
        match generate_proactive(&store, &key, &s, Some(&kind), "主人", None, false, &[]).await {
            Ok(Some(t)) => println!("搭话: {}", t),
            Ok(None) => println!("[PASS]"),
            Err(e) => println!("失败: {}", e),
        }
        // 防摸鱼提醒
        let kind2 = ReminderKind::AntiSlack {
            prev_app: "VS Code".to_string(),
            new_app: "哔哩哔哩".to_string(),
            minutes: 25,
        };
        let s2 = snap(ActivityState::CasualBrowsing, "哔哩哔哩", 30, 10.0);
        println!("=== 防摸鱼提醒生成 ===");
        match generate_proactive(&store, &key, &s2, Some(&kind2), "主人", None, false, &[]).await {
            Ok(Some(t)) => println!("搭话: {}", t),
            Ok(None) => println!("[PASS]"),
            Err(e) => println!("失败: {}", e),
        }
        // 普通搭话（空闲）
        let s3 = snap(ActivityState::Idle, "桌面", 60, 300.0);
        println!("=== 普通搭话生成（含未收尾话题注入）===");
        let open_threads = vec![
            "主人之前说想吃顿好的又想减肥，减肥那条被接住了，'吃点好的'还没人接".to_string(),
        ];
        match generate_proactive(&store, &key, &s3, None, "主人", None, true, &open_threads).await {
            Ok(Some(t)) => println!("搭话: {}", t),
            Ok(None) => println!("[PASS]"),
            Err(e) => println!("失败: {}", e),
        }
    }

    #[test]
    fn test_should_speak_guards() {
        let p = ProactiveState::new();
        // 隐私 → 否
        let priv_snap = snap(ActivityState::Private, "KeePass", 30, 0.0);
        assert!(!p.should_speak(&priv_snap, 30));
        // 离开 → 否
        let away_snap = snap(ActivityState::Away, "VS Code", 1000, 0.0);
        assert!(!p.should_speak(&away_snap, 1000));
        // 最近 10s 有输入 → 否
        let work_snap = snap(ActivityState::FocusedWork, "VS Code", 5, 100.0);
        assert!(!p.should_speak(&work_snap, 5));
        // 正常 → 是（首次无冷却）
        let ok_snap = snap(ActivityState::FocusedWork, "VS Code", 30, 100.0);
        assert!(p.should_speak(&ok_snap, 30));
    }

    // ── 3-tier 退避测试（抄 N.E.K.O. app-proactive.js 三段式）──

    #[test]
    fn test_backoff_cooldown_by_level() {
        let p = ProactiveState::new();
        // 默认 level 0 → 20min 冷却
        assert_eq!(p.backoff_cooldown_seconds(), 20.0 * 60.0);
        assert_eq!(p.current_backoff_level(), 0);
        // 升到 level 2 → 90min
        for _ in 0..4 {
            p.on_delivery_made();
        }
        assert_eq!(p.current_backoff_level(), 2);
        assert_eq!(p.backoff_cooldown_seconds(), 90.0 * 60.0);
        // 封顶：再投递不超 2
        for _ in 0..10 {
            p.on_delivery_made();
        }
        assert_eq!(p.current_backoff_level(), 2);
    }

    #[test]
    fn test_backoff_escalation_pace() {
        let p = ProactiveState::new();
        // 连续 2 次搭话无回应 → 升 level 1
        p.on_delivery_made();
        assert_eq!(p.current_backoff_level(), 0, "1 次无回应不升级");
        p.on_delivery_made();
        assert_eq!(p.current_backoff_level(), 1, "连续 2 次无回应 → level 1");
        assert_eq!(p.backoff_cooldown_seconds(), 40.0 * 60.0);
        // 再 2 次 → level 2
        p.on_delivery_made();
        p.on_delivery_made();
        assert_eq!(p.current_backoff_level(), 2);
        assert_eq!(p.backoff_cooldown_seconds(), 90.0 * 60.0);
    }

    #[test]
    fn test_backoff_reset_on_user_response() {
        let p = ProactiveState::new();
        // 升到 level 2
        for _ in 0..4 {
            p.on_delivery_made();
        }
        assert_eq!(p.current_backoff_level(), 2);
        // 用户回应 → 归零
        p.reset_backoff();
        assert_eq!(p.current_backoff_level(), 0);
        assert_eq!(p.backoff_cooldown_seconds(), 20.0 * 60.0);
        // 归零后再投递从 level 0 重新开始
        p.on_delivery_made();
        assert_eq!(p.current_backoff_level(), 0);
    }
}
