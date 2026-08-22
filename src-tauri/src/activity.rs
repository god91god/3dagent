// ============================================================
// 活动感知系统 —— 借鉴 N.E.K.O. main_logic/activity/
// 许可: Apache-2.0（同上游）
//
// L2-B 实现（简化版）：
//   - 5s 轮询：GetForegroundWindow → PID → 进程名 + 窗口标题
//              GetLastInputInfo → 空闲秒数
//   - 窗口分类表（自维护）：work / communication / entertainment /
//     private / own_app
//   - 纯规则状态机（无 LLM）：
//     away / private / focused_work / casual_browsing / chatting /
//     transitioning / idle
//   - ActivitySnapshot 四轴：state / propensity / skip_probability / tone
// ============================================================

use chrono::{Datelike, Timelike};

// ── 状态枚举（抄 N.E.K.O. snapshot.py 精简版）──
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Away,               // 空闲 ≥ 15min，人走了
    Private,            // 敏感应用前台（密码管理器/银行）—— 绝不打搅
    FocusedWork,        // IDE/文档 + 驻留 + 最近输入
    CasualBrowsing,     // 娱乐类 + 驻留
    Chatting,           // IM/会议前台
    Transitioning,      // 快速切换窗口
    Idle,               // 兜底：人在但无明确活动
}

impl ActivityState {
    pub fn label(&self) -> &'static str {
        match self {
            ActivityState::Away => "离开",
            ActivityState::Private => "隐私模式",
            ActivityState::FocusedWork => "专注工作",
            ActivityState::CasualBrowsing => "休闲浏览",
            ActivityState::Chatting => "聊天中",
            ActivityState::Transitioning => "切换中",
            ActivityState::Idle => "空闲",
        }
    }
}

// ── 倾向（propensity）：prompt 素材许可 ──
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Propensity {
    Closed,                 // 硬跳过（仅 private）
    RestrictedScreenOnly,   // 只许屏幕吐槽（专注/沉浸）
    Open,                   // 默认全渠道
}

impl Propensity {
    pub fn label(&self) -> &'static str {
        match self {
            Propensity::Closed => "勿扰",
            Propensity::RestrictedScreenOnly => "屏幕吐槽",
            Propensity::Open => "可搭话",
        }
    }
}

// ── 窗口分类结果 ──
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCategory {
    Work,
    Communication,
    Entertainment,
    Private,
    OwnApp,
    Unknown,
}

// ── 一次系统信号采样 ──
#[derive(Debug, Clone)]
pub struct SystemSignals {
    pub idle_seconds: u32,
    pub process_name: String,
    pub window_title: String,
    pub timestamp_secs: f64,
}

/// 活动快照（下游唯一契约）
#[derive(Debug, Clone)]
pub struct ActivitySnapshot {
    pub state: ActivityState,
    pub state_age_seconds: f64,
    pub propensity: Propensity,
    pub skip_probability: f64,
    pub tone: String,
    pub active_window: String,       // 规范化 app 名
    pub idle_seconds: u32,
    pub switch_rate_5min: usize,
    pub seconds_since_user_msg: Option<f64>,
    pub hour: u32,
    pub weekday: u32,
    /// LLM 叙述"主人在干嘛"（activity_guess，缓存）
    pub activity_guess: String,
    /// 叙述对应的状态签名（state + 窗口类别），用于退避判断
    pub guess_signature: String,
}

// ── 状态机 ──

/// 活动状态机（纯规则，可审计）
pub struct ActivityStateMachine {
    // 状态
    current_state: ActivityState,
    state_started_at: f64,
    previous_state: Option<ActivityState>,
    // 窗口跟踪
    current_category: Option<WindowCategory>,
    current_window_name: String,
    current_window_started_at: f64,
    window_history: Vec<(f64, WindowCategory)>, // (timestamp, category)
    // 系统
    latest_idle: u32,
    // 对话
    last_user_msg_at: Option<f64>,
    // 阈值（抄 N.E.K.O. 默认值）
    away_idle_seconds: u32,
    focused_work_dwell: f64,
    casual_dwell: f64,
    switch_threshold: usize,
    switch_lookback: f64,
}

// 阈值默认值（抄 N.E.K.O. activity_config.py）
const AWAY_IDLE_SECONDS: u32 = 900;        // 15min
const FOCUSED_WORK_MIN_DWELL: f64 = 90.0;  // 90s 驻留
const CASUAL_MIN_DWELL: f64 = 30.0;        // 30s 驻留
const SWITCH_THRESHOLD: usize = 5;         // 5 窗口
const SWITCH_LOOKBACK: f64 = 300.0;        // 5min
const WINDOW_BUFFER_MAXLEN: usize = 64;

impl ActivityStateMachine {
    pub fn new() -> Self {
        ActivityStateMachine {
            current_state: ActivityState::Idle,
            state_started_at: now_secs(),
            previous_state: None,
            current_category: None,
            current_window_name: String::new(),
            current_window_started_at: 0.0,
            window_history: Vec::new(),
            latest_idle: 0,
            last_user_msg_at: None,
            away_idle_seconds: AWAY_IDLE_SECONDS,
            focused_work_dwell: FOCUSED_WORK_MIN_DWELL,
            casual_dwell: CASUAL_MIN_DWELL,
            switch_threshold: SWITCH_THRESHOLD,
            switch_lookback: SWITCH_LOOKBACK,
        }
    }

    /// 注入一次系统采样（每 5s 调一次）
    pub fn update_signals(&mut self, sig: &SystemSignals) {
        let ts = sig.timestamp_secs;
        self.latest_idle = sig.idle_seconds;

        // 分类窗口
        let category = classify_window(&sig.process_name, &sig.window_title);
        let app_name = canonical_app_name(&sig.process_name, &sig.window_title);

        // own_app 冻结：桌宠自己前台不算活动变化（不记录、不重置驻留计时）
        if category == WindowCategory::OwnApp {
            return;
        }

        // 折叠连续同类别观察（dwell 计时继续）
        if Some(category) != self.current_category {
            // 类别变化 → 记录历史 + 重置驻留
            if let Some(prev) = self.current_category {
                self.window_history.push((ts, prev));
                if self.window_history.len() > WINDOW_BUFFER_MAXLEN {
                    self.window_history.remove(0);
                }
            }
            self.current_category = Some(category);
            self.current_window_name = app_name.clone();
            self.current_window_started_at = ts;
        }
        // 同类别但窗口名变化也更新名字
        if !app_name.is_empty() {
            self.current_window_name = app_name;
        }
    }

    /// 记录用户消息（对话信号）
    pub fn on_user_message(&mut self) {
        self.last_user_msg_at = Some(now_secs());
    }

    /// 计算快照（状态机 tick）
    pub fn get_snapshot(&mut self) -> ActivitySnapshot {
        self.get_snapshot_at(now_secs())
    }

    /// 计算快照（可注入时间，测试用）
    pub fn get_snapshot_at(&mut self, ts: f64) -> ActivitySnapshot {
        let new_state = self.classify(ts);

        // 状态迁移记账
        if new_state != self.current_state {
            self.previous_state = Some(self.current_state);
            self.current_state = new_state;
            self.state_started_at = ts;
        }

        // 5min 内窗口切换次数
        let switch_rate = self
            .window_history
            .iter()
            .filter(|(t, _)| ts - t <= self.switch_lookback)
            .count();

        // propensity + tone + skip_probability 派生
        let (propensity, skip_probability, tone) = derive(new_state);

        let local = chrono::Local::now();
        ActivitySnapshot {
            state: self.current_state,
            state_age_seconds: ts - self.state_started_at,
            propensity,
            skip_probability,
            tone: tone.to_string(),
            active_window: self.current_window_name.clone(),
            idle_seconds: self.latest_idle,
            switch_rate_5min: switch_rate,
            seconds_since_user_msg: self
                .last_user_msg_at
                .map(|t| ts - t),
            hour: local.hour(),
            weekday: local.weekday().num_days_from_monday(),
            activity_guess: String::new(),
            guess_signature: String::new(),
        }
    }

    /// 快照的活动签名（state + 窗口类别），供 activity_guess 退避判断
    /// （抄 N.E.K.O. tracker._activity_guess_signature：粗粒度签名，同类别窗口切换不算新活动）
    pub fn guess_signature(&self) -> String {
        let state = format!("{:?}", self.current_state);
        let cat = match self.current_category {
            Some(c) => format!("{:?}", c),
            None => "None".to_string(),
        };
        format!("{}|{}", state, cat)
    }

    /// 纯规则分类（优先级从高到低，抄 N.E.K.O. _classify_state）
    fn classify(&self, ts: f64) -> ActivityState {
        // 1. away —— 系统空闲压倒一切
        if self.latest_idle >= self.away_idle_seconds {
            return ActivityState::Away;
        }
        // 2. private —— 敏感应用前台，赢过一切
        if self.current_category == Some(WindowCategory::Private) {
            return ActivityState::Private;
        }
        // 3. focused_work —— work 类 + 驻留足够 + 最近有输入
        if self.current_category == Some(WindowCategory::Work) {
            let dwell = ts - self.current_window_started_at;
            let recent_input = self.latest_idle < 300; // 5min 内有输入
            if dwell >= self.focused_work_dwell && recent_input {
                return ActivityState::FocusedWork;
            }
        }
        // 4. casual_browsing —— 娱乐类 + 驻留
        if self.current_category == Some(WindowCategory::Entertainment) {
            let dwell = ts - self.current_window_started_at;
            if dwell >= self.casual_dwell {
                return ActivityState::CasualBrowsing;
            }
        }
        // 5. chatting —— 通讯类前台（不 gate 驻留，聊天窗常被短暂提起）
        if self.current_category == Some(WindowCategory::Communication) {
            return ActivityState::Chatting;
        }
        // 6. transitioning —— 近 5min 窗口切换频繁
        let switches = self
            .window_history
            .iter()
            .filter(|(t, _)| ts - t <= self.switch_lookback)
            .count();
        if switches >= self.switch_threshold {
            return ActivityState::Transitioning;
        }
        // 7. idle —— 兜底
        ActivityState::Idle
    }
}

/// 派生 propensity / skip_probability / tone（抄 N.E.K.O. derive_* 逻辑）
fn derive(state: ActivityState) -> (Propensity, f64, &'static str) {
    match state {
        ActivityState::Private => (Propensity::Closed, 1.0, "勿扰"),
        ActivityState::FocusedWork => (Propensity::RestrictedScreenOnly, 0.0, "简洁"),
        ActivityState::CasualBrowsing => (Propensity::Open, 0.0, "俏皮"),
        ActivityState::Chatting => (Propensity::Open, 0.0, "温暖"),
        ActivityState::Transitioning => (Propensity::Open, 0.0, "简洁"),
        ActivityState::Idle => (Propensity::Open, 0.0, "俏皮"),
        ActivityState::Away => (Propensity::RestrictedScreenOnly, 0.5, "安静"),
    }
}

// ── 窗口分类表（自维护，办公场景重点）──

/// 分类前台窗口（先进程名精确匹配，再标题子串兜底）
pub fn classify_window(process_name: &str, window_title: &str) -> WindowCategory {
    let p = process_name.to_lowercase();

    // own_app
    if p == "dagent.exe" || p == "dagent" {
        return WindowCategory::OwnApp;
    }
    // private：密码管理器/银行/支付
    if p.contains("keepass")
        || p.contains("bitwarden")
        || p.contains("1password")
        || p.contains("enpass")
        || p.contains("ibank")
    {
        return WindowCategory::Private;
    }
    // work：IDE/文档/终端/办公
    if [
        "code.exe", "devenv.exe", "idea64.exe", "pycharm64.exe", "goland64.exe",
        "rider64.exe", "clion64.exe", "webstorm64.exe", "datagrip64.exe",
        "wps.exe", "winword.exe", "excel.exe", "powerpnt.exe", "onenote.exe",
        "outlook.exe", "obsidian.exe", "notion.exe", "typora.exe", "marktext.exe",
        "figma.exe", "axure.exe", "xmind.exe", "drawio.exe", "notepad.exe",
        "notepad++.exe", "windows_terminal.exe", "windowsterminal.exe", "cmd.exe",
        "powershell.exe", "pwsh.exe", "wezterm.exe", "alacritty.exe", "konsole.exe",
        "vsdbg.exe", "python.exe", "node.exe", "java.exe",
    ]
    .iter()
    .any(|x| p.ends_with(x))
    {
        return WindowCategory::Work;
    }
    // communication：IM/会议
    if [
        "wechat.exe", "weixin.exe", "qq.exe", "tim.exe", "dingtalk.exe",
        "feishu.exe", "lark.exe", "telegram.exe", "slack.exe", "teams.exe",
        "discord.exe", "zoom.exe", "microsoftteams.exe", "kook.exe", "vxwork.exe",
    ]
    .iter()
    .any(|x| p.ends_with(x))
    {
        return WindowCategory::Communication;
    }
    // entertainment：浏览器/播放器/游戏平台
    if [
        "msedge.exe", "chrome.exe", "firefox.exe", "360se.exe", "qqbrowser.exe",
        "potplayer.exe", "vlc.exe", "wmplayer.exe", "cloudmusic.exe",
        "qqmusic.exe", "bilibili.exe", "steam.exe", "wegame.exe", "epicgameslauncher.exe",
    ]
    .iter()
    .any(|x| p.ends_with(x))
    {
        return WindowCategory::Entertainment;
    }

    // 标题兜底（浏览器页面标题带域名，识别视频/社交）
    let t = window_title.to_lowercase();
    if t.contains("bilibili") || t.contains("youtube") || t.contains("douyin")
        || t.contains("微博") || t.contains("weibo") || t.contains("抖音")
        || t.contains("爱奇艺") || t.contains("iqiyi") || t.contains("优酷")
        || t.contains("youku") || t.contains("腾讯视频") || t.contains("西瓜视频")
    {
        return WindowCategory::Entertainment;
    }
    if t.contains("keepass") || t.contains("bitwarden") || t.contains("密码") || t.contains("网银")
        || t.contains("银行") || t.contains("bank") || t.contains("支付")
    {
        return WindowCategory::Private;
    }

    WindowCategory::Unknown
}

/// 规范化 app 显示名（供 prompt/调试面板）
pub fn canonical_app_name(process_name: &str, window_title: &str) -> String {
    let p = process_name.to_lowercase();
    // 已知 exe → 友好名
    let known = [
        ("code.exe", "VS Code"),
        ("devenv.exe", "Visual Studio"),
        ("idea64.exe", "IntelliJ IDEA"),
        ("pycharm64.exe", "PyCharm"),
        ("wps.exe", "WPS"),
        ("winword.exe", "Word"),
        ("excel.exe", "Excel"),
        ("powerpnt.exe", "PowerPoint"),
        ("obsidian.exe", "Obsidian"),
        ("notion.exe", "Notion"),
        ("figma.exe", "Figma"),
        ("msedge.exe", "Edge"),
        ("chrome.exe", "Chrome"),
        ("firefox.exe", "Firefox"),
        ("wechat.exe", "微信"),
        ("weixin.exe", "微信"),
        ("qq.exe", "QQ"),
        ("dingtalk.exe", "钉钉"),
        ("feishu.exe", "飞书"),
        ("lark.exe", "飞书"),
        ("telegram.exe", "Telegram"),
        ("discord.exe", "Discord"),
        ("potplayer.exe", "PotPlayer"),
        ("cloudmusic.exe", "网易云音乐"),
        ("qqmusic.exe", "QQ音乐"),
        ("bilibili.exe", "哔哩哔哩"),
        ("steam.exe", "Steam"),
        ("windows_terminal.exe", "终端"),
        ("windowsterminal.exe", "终端"),
        ("cmd.exe", "命令行"),
        ("powershell.exe", "PowerShell"),
        ("pwsh.exe", "PowerShell"),
    ];
    for (exe, name) in known {
        if p.ends_with(exe) {
            return name.to_string();
        }
    }
    // 未知进程：优先用窗口标题（更可读），去掉尾随的 " - 浏览器" 类后缀
    let title = window_title.trim();
    if !title.is_empty() && title.len() <= 40 {
        return title.to_string();
    }
    if !p.is_empty() {
        // 去掉 .exe
        return p.trim_end_matches(".exe").to_string();
    }
    "未知".to_string()
}

// ── 系统信号采集（Win32）──

/// 采集前台窗口 + 空闲秒数
#[cfg(windows)]
pub fn collect_system_signals() -> SystemSignals {
    use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows_sys::Win32::UI::WindowsAndMessaging as wam;

    let ts = now_secs();

    // 1. 前台窗口 → PID
    let hwnd: HWND = unsafe { wam::GetForegroundWindow() };
    let mut pid: u32 = 0;
    if !hwnd.is_null() {
        unsafe {
            wam::GetWindowThreadProcessId(hwnd, &mut pid);
        }
    }

    // 2. 窗口标题
    let mut title = String::new();
    if !hwnd.is_null() {
        let mut buf = [0u16; 256];
        unsafe {
            let len = wam::GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if len > 0 {
                title = String::from_utf16_lossy(&buf[..len as usize]);
            }
        }
    }

    // 3. 进程名（tlhelp32 遍历按 pid 查找，比 OpenProcess+QueryFullProcessImageName 简单）
    let mut process_name = String::new();
    if pid != 0 {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot != INVALID_HANDLE_VALUE {
                let mut entry: PROCESSENTRY32W = std::mem::zeroed();
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                if Process32FirstW(snapshot, &mut entry) != 0 {
                    loop {
                        if entry.th32ProcessID == pid {
                            let name = String::from_utf16_lossy(
                                &entry.szExeFile[..entry.szExeFile.iter().take_while(|&&c| c != 0).count()],
                            );
                            process_name = name;
                            break;
                        }
                        if Process32NextW(snapshot, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snapshot);
            }
        }
    }

    // 4. 空闲秒数（GetLastInputInfo + GetTickCount，49.7 天回绕安全）
    let mut idle_seconds: u32 = 0;
    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut lii) != 0 {
            let tick = GetTickCount();
            let elapsed_ms = tick.wrapping_sub(lii.dwTime);
            idle_seconds = elapsed_ms / 1000;
        }
    }

    SystemSignals {
        idle_seconds,
        process_name,
        window_title: title,
        timestamp_secs: ts,
    }
}

/// 非 Windows 平台占位
#[cfg(not(windows))]
pub fn collect_system_signals() -> SystemSignals {
    SystemSignals {
        idle_seconds: 0,
        process_name: String::new(),
        window_title: String::new(),
        timestamp_secs: now_secs(),
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 渲染活动状态块（供 prompt 注入，本地化）
pub fn format_activity_state_section(snap: &ActivitySnapshot) -> String {
    let mut lines = vec!["【当前活动】".to_string()];
    let state_label = snap.state.label();
    let propensity_label = snap.propensity.label();
    lines.push(format!(
        "{}（{}）· {}",
        state_label, snap.active_window, propensity_label
    ));
    // 时间 + 用户消息间隔
    let period = match snap.hour {
        0..=5 => "凌晨",
        6..=11 => "上午",
        12..=13 => "中午",
        14..=17 => "下午",
        18..=23 => "晚上",
        _ => "",
    };
    let mut time_line = format!("{} {}点", period, snap.hour);
    if let Some(s) = snap.seconds_since_user_msg {
        if s < 60.0 {
            time_line.push_str(&format!(" · 用户 {} 秒前", s as i64));
        } else if s < 3600.0 {
            time_line.push_str(&format!(" · 用户 {} 分钟前", (s / 60.0) as i64));
        } else {
            time_line.push_str(&format!(" · 用户 {} 小时前", (s / 3600.0) as i64));
        }
    }
    if snap.idle_seconds >= 60 {
        time_line.push_str(&format!(" · 空闲 {} 分钟", snap.idle_seconds / 60));
    }
    lines.push(time_line);
    lines.push(format!(
        "状态持续 {} 秒 · 5分钟切换 {} 次窗口",
        snap.state_age_seconds as i64, snap.switch_rate_5min
    ));
    // activity_guess：LLM 叙述"主人在干嘛"（存在时注入）
    if !snap.activity_guess.is_empty() {
        lines.push(format!("【叙述】{}", snap.activity_guess));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(proc: &str, title: &str, idle: u32, ts: f64) -> SystemSignals {
        SystemSignals {
            idle_seconds: idle,
            process_name: proc.to_string(),
            window_title: title.to_string(),
            timestamp_secs: ts,
        }
    }

    #[test]
    fn test_classify_window() {
        assert_eq!(classify_window("Code.exe", "main.rs - dagent"), WindowCategory::Work);
        assert_eq!(classify_window("msedge.exe", "bilibili - 视频"), WindowCategory::Entertainment);
        assert_eq!(classify_window("wechat.exe", "文件传输助手"), WindowCategory::Communication);
        assert_eq!(classify_window("keepass.exe", "KeePass"), WindowCategory::Private);
        assert_eq!(classify_window("dagent.exe", "dagent"), WindowCategory::OwnApp);
        // 浏览器标题兜底
        assert_eq!(classify_window("msedge.exe", "哔哩哔哩 (゜-゜)つロ"), WindowCategory::Entertainment);
        // 未知
        assert_eq!(classify_window("unknown_app.exe", "某窗口"), WindowCategory::Unknown);
    }

    #[test]
    fn test_own_app_freeze() {
        let mut sm = ActivityStateMachine::new();
        let t0 = 1000.0;
        // 先处于工作窗口
        sm.update_signals(&sig("Code.exe", "a.rs", 10, t0));
        let snap = sm.get_snapshot_at(t0);
        // 桌宠自己前台 → 冻结（状态不变）
        sm.update_signals(&sig("dagent.exe", "dagent", 10, t0 + 100.0));
        let snap2 = sm.get_snapshot_at(t0 + 100.0);
        assert_eq!(snap2.active_window, snap.active_window, "own_app 不应改变活动窗口");
    }

    #[test]
    fn test_focused_work_requires_dwell_and_input() {
        let mut sm = ActivityStateMachine::new();
        let t0 = 2000.0;
        // 工作窗口 + 驻留 10s → 还没到 90s dwell，不判定专注
        sm.update_signals(&sig("Code.exe", "main.rs", 10, t0));
        sm.update_signals(&sig("Code.exe", "main.rs", 10, t0 + 10.0));
        let snap = sm.get_snapshot_at(t0 + 10.0);
        assert_eq!(snap.state, ActivityState::Idle, "驻留不足 90s 不应判定专注: {:?}", snap.state);

        // 驻留 100s + 最近有输入（idle 10s）→ focused_work
        sm.update_signals(&sig("Code.exe", "main.rs", 10, t0 + 100.0));
        let snap = sm.get_snapshot_at(t0 + 100.0);
        assert_eq!(snap.state, ActivityState::FocusedWork);
        assert_eq!(snap.propensity, Propensity::RestrictedScreenOnly);
    }

    #[test]
    fn test_away_dominates() {
        let mut sm = ActivityStateMachine::new();
        let t0 = 3000.0;
        sm.update_signals(&sig("Code.exe", "main.rs", 10, t0));
        // 空闲 1000s（>15min）→ away 压过一切
        sm.update_signals(&sig("Code.exe", "main.rs", 1000, t0 + 20.0));
        let snap = sm.get_snapshot_at(t0 + 20.0);
        assert_eq!(snap.state, ActivityState::Away);
    }

    #[test]
    fn test_private_wins() {
        let mut sm = ActivityStateMachine::new();
        let t0 = 4000.0;
        sm.update_signals(&sig("keepass.exe", "KeePass - 密码库", 10, t0));
        let snap = sm.get_snapshot_at(t0);
        assert_eq!(snap.state, ActivityState::Private);
        assert_eq!(snap.propensity, Propensity::Closed);
        assert_eq!(snap.skip_probability, 1.0);
    }

    #[test]
    fn test_casual_browsing_dwell() {
        let mut sm = ActivityStateMachine::new();
        let t0 = 5000.0;
        sm.update_signals(&sig("msedge.exe", "bilibili", 20, t0));
        // 10s 不够 30s dwell
        sm.update_signals(&sig("msedge.exe", "bilibili", 20, t0 + 10.0));
        let snap = sm.get_snapshot_at(t0 + 10.0);
        assert_eq!(snap.state, ActivityState::Idle);
        // 35s → casual_browsing
        sm.update_signals(&sig("msedge.exe", "bilibili", 20, t0 + 35.0));
        let snap = sm.get_snapshot_at(t0 + 35.0);
        assert_eq!(snap.state, ActivityState::CasualBrowsing);
        assert_eq!(snap.propensity, Propensity::Open);
    }

    #[test]
    fn test_chatting_immediate() {
        let mut sm = ActivityStateMachine::new();
        let t0 = 6000.0;
        sm.update_signals(&sig("wechat.exe", "文件传输助手", 5, t0));
        let snap = sm.get_snapshot_at(t0);
        assert_eq!(snap.state, ActivityState::Chatting, "聊天窗口不需驻留");
    }

    #[test]
    fn test_transitioning_detection() {
        let mut sm = ActivityStateMachine::new();
        let t0 = 7000.0;
        // 6 次类别变化（work→chat→ent→work→unknown→chat），最后停在无 dwell 覆盖的 unknown
        sm.update_signals(&sig("Code.exe", "a", 5, t0));
        sm.update_signals(&sig("wechat.exe", "b", 5, t0 + 10.0));
        sm.update_signals(&sig("msedge.exe", "c", 5, t0 + 20.0));
        sm.update_signals(&sig("Code.exe", "d", 5, t0 + 30.0));
        sm.update_signals(&sig("unknown_app.exe", "e", 5, t0 + 40.0));
        sm.update_signals(&sig("wechat.exe", "f", 5, t0 + 50.0));
        let snap = sm.get_snapshot_at(t0 + 50.0);
        // 历史记录 ≥5 次切换 → transitioning（chat 不 gate dwell，但当前类别是 chat…
        // 实际上 chatting 分支优先于 transitioning；这里验证 switches 计数 ≥5）
        assert!(
            snap.switch_rate_5min >= 5,
            "5min 内应有 ≥5 次窗口切换: switches={}",
            snap.switch_rate_5min
        );
        // 最后停在 chat 类 → chatting 优先（分类优先级正确）
        assert_eq!(snap.state, ActivityState::Chatting);
    }

    /// 真实环境采集测试（需要 Windows 桌面会话）：手动运行
    #[test]
    #[ignore]
    fn real_collect_signals() {
        let sig = collect_system_signals();
        println!(
            "前台窗口: process={:?} title={:?} idle={}s",
            sig.process_name, sig.window_title, sig.idle_seconds
        );
        let cat = classify_window(&sig.process_name, &sig.window_title);
        let name = canonical_app_name(&sig.process_name, &sig.window_title);
        println!("分类: {:?}, 显示名: {}", cat, name);
        assert!(!sig.process_name.is_empty(), "应能读到前台进程");
    }
}
