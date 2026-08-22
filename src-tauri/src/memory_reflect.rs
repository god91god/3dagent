// ============================================================
// 反思合成 + 证据闭环 —— 借鉴 N.E.K.O. memory/reflection/ + evidence.py
// 许可: Apache-2.0（同上游）
//
// 流程（对应 N.E.K.O. 后台维护循环）：
//   synthesize: unabsorbed facts ≥ 5 → LLM 五步法 → reflections.json（确定性 ID 幂等）
//   detect_signals: 新事实 × 已有反思 → LLM 判 reinforces/negates → 更新 evidence
//   promote: score ≥ 1.0 confirmed / ≥ 2.0 promoted（合入 persona）
// ============================================================

use crate::memory::{
    MemoryStore, Reflection, USER_FACT_NEGATE_DELTA, USER_FACT_REINFORCE_DELTA,
};
use crate::memory_prompts::{REFLECTION_PROMPT, SIGNAL_DETECTION_PROMPT};

/// 一次 qwen LLM 调用（JSON 模式），返回原始文本
/// 后台重任务（反思/信号检测），用长超时 Client（共享 60s 对长 prompt 不够）
/// 反思合成/信号检测是记忆质量命门：用 memory_model（qwen-max），不用聊天模型
async fn llm_json_call(prompt: &str, api_key: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "model": crate::memory_model(),
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        "response_format": {"type": "json_object"}
    });
    let client = crate::long_timeout_client()?;
    let resp = client
        .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("接口返回 {}: {}", status, text));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("响应解析失败: {}", e))?;
    body["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "响应无内容".to_string())
}

/// 确定性反思 ID：sha256(sorted(source_fact_ids)) 前 16 位
fn reflection_id_from_facts(fact_ids: &[String]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut sorted = fact_ids.to_vec();
    sorted.sort();
    let mut h = DefaultHasher::new();
    sorted.hash(&mut h);
    format!("ref_{:016x}", h.finish())
}

/// 反思合成：unabsorbed facts ≥ MIN 时调用 LLM 五步法，幂等落库
/// 返回 Ok(是否合成了新反思)
pub async fn synthesize_reflection(
    store: &MemoryStore,
    api_key: &str,
    lanlan_name: &str,
    master_name: &str,
) -> Result<bool, String> {
    // 1. 取未吸收的高分事实
    let unabsorbed = store.get_unabsorbed_facts(5);
    if unabsorbed.len() < crate::memory::MIN_FACTS_FOR_REFLECTION {
        return Ok(false); // 不足 5 条，跳过
    }
    // 2. 数量上限：取前 20 条（按 importance 排序）
    let mut pool = unabsorbed;
    pool.sort_by(|a, b| b.importance.cmp(&a.importance));
    let pool: Vec<_> = pool.into_iter().take(20).collect();
    let fact_ids: Vec<String> = pool.iter().map(|f| f.id.clone()).collect();

    // 3. 幂等检查：同批事实的反思已存在则跳过
    let rid = reflection_id_from_facts(&fact_ids);
    if store
        .get_reflections()
        .iter()
        .any(|r| r.id == rid)
    {
        // 已合成过——仍要 mark absorbed 防重跑
        let _ = store.mark_facts_absorbed(&fact_ids);
        return Ok(false);
    }

    // 4. 组装 prompt（RELATED_CONTEXT_BLOCK 简化版：已吸收旧事实作为锚）
    let mut related = String::new();
    let absorbed_old: Vec<String> = store
        .get_facts()
        .into_iter()
        .filter(|f| f.absorbed)
        .map(|f| format!("- {} (importance: {})", f.text, f.importance))
        .collect();
    if !absorbed_old.is_empty() {
        related.push_str("以下为此前已确认的相关事实（避免新反思与其矛盾）：\n");
        for t in absorbed_old.iter().take(10) {
            related.push_str(t);
            related.push('\n');
        }
        related.push('\n');
    }

    let facts_text = pool
        .iter()
        .map(|f| format!("- {} (importance: {})", f.text, f.importance))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = REFLECTION_PROMPT
        .replace("{LANLAN_NAME}", lanlan_name)
        .replace("{MASTER_NAME}", master_name)
        .replace("{RELATED_CONTEXT_BLOCK}", &related)
        .replace("{FACTS}", &facts_text);

    // 5. LLM 调用（失败不 mark absorbed，下次重试）
    let content = llm_json_call(&prompt, api_key).await?;
    let cleaned = strip_markdown_fence(&content);
    let parsed: serde_json::Value =
        serde_json::from_str(&cleaned).map_err(|e| format!("反思解析失败: {}（原始: {}）", e, content.chars().take(150).collect::<String>()))?;

    let entity = parsed
        .get("entity")
        .and_then(|e| e.as_str())
        .unwrap_or("master")
        .to_string();
    let text = parsed
        .get("reflection")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("反思内容为空".to_string());
    }

    let reflection = Reflection {
        id: rid,
        text,
        entity,
        relation_type: parsed
            .get("relation_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        temporal_scope: parsed
            .get("temporal_scope")
            .and_then(|v| v.as_str())
            .unwrap_or("pattern")
            .to_string(),
        status: "pending".to_string(),
        source_fact_ids: fact_ids.clone(),
        // importance 种子（抄 N.E.K.O.）：批量最高 importance ≥ 7 预置正分，
        // 关键记忆不必等多次自然确认。用 MAX 不用 AVG——一条 10 分事实足以标记整批重要。
        reinforcement: crate::memory::initial_reinforcement_from_importance(
            pool.iter().map(|f| f.importance).max().unwrap_or(0),
        ),
        disputation: 0.0,
        rein_last_signal_at: Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()),
        disp_last_signal_at: None,
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        confirmed_at: None,
    };

    // 6. 落库（幂等） + mark absorbed
    let added = store.add_reflection(reflection)?;
    if added {
        let _ = store.mark_facts_absorbed(&fact_ids);
        return Ok(true);
    }
    let _ = store.mark_facts_absorbed(&fact_ids);
    Ok(false)
}

/// Stage-2 信号检测：新事实 × 已有反思 → reinforces/negates → 更新 evidence
/// 返回 (signals_applied, skipped)
pub async fn detect_signals(
    store: &MemoryStore,
    api_key: &str,
) -> Result<(usize, usize), String> {
    // 候选观察 = pending + confirmed + promoted 反思
    // N.E.K.O. 原版只取 confirmed+promoted；我们单用户无 surfacing 确认通道，
    // 让 pending 也参与信号检测（reinforce 权重减半防误强化），加快闭环
    let observations: Vec<Reflection> = store
        .get_reflections()
        .into_iter()
        .filter(|r| r.status != "archived")
        .collect();
    if observations.is_empty() {
        return Ok((0, 0));
    }

    // 待检测事实 = 未吸收 + 未参与过信号检测（importance ≥ 5）
    // 历史 bug：用 get_unabsorbed_facts → 同一批事实每 3 分钟重复判定，
    // reinforcement 无限膨胀 + 白烧 LLM；现在检测后统一标记 signals_checked
    let new_facts = store.get_pending_signal_facts(5, 10);
    if new_facts.is_empty() {
        return Ok((0, 0));
    }

    // 组装 prompt（新事实最多 10 条，观察最多 15 条控制预算）
    let new_facts_text = new_facts
        .iter()
        .take(10)
        .map(|f| format!("{}: {}", f.id, f.text))
        .collect::<Vec<_>>()
        .join("\n");
    let obs_text = observations
        .iter()
        .take(15)
        .map(|r| format!("reflection.{}.{}: {}", r.entity, r.id, r.text))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = SIGNAL_DETECTION_PROMPT
        .replace("{NEW_FACTS}", &new_facts_text)
        .replace("{EXISTING_OBSERVATIONS}", &obs_text);

    let content = llm_json_call(&prompt, api_key).await?;
    let cleaned = strip_markdown_fence(&content);
    let parsed: serde_json::Value =
        serde_json::from_str(&cleaned).map_err(|e| format!("信号解析失败: {}", e))?;

    let signals = parsed
        .get("signals")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    let mut applied = 0;
    let mut skipped = 0;
    let known: Vec<(String, String)> = observations
        .iter()
        .map(|r| (r.id.clone(), r.status.clone()))
        .collect();

    for sig in signals {
        let target_id = sig
            .get("target_id")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let signal = sig
            .get("signal")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        // 校验 target_id 必须来自观察区（防 LLM 幻觉凭空 ID）
        let status = match known.iter().find(|(id, _)| id == target_id) {
            Some((_, st)) => st.clone(),
            None => {
                skipped += 1;
                continue;
            }
        };
        let mut delta = match signal {
            "reinforces" => USER_FACT_REINFORCE_DELTA,
            "negates" => -USER_FACT_NEGATE_DELTA,
            _ => {
                skipped += 1;
                continue;
            }
        };
        // pending 反思权重减半（还没被确认过，防 LLM 误强化把错误印象推上去）
        if status == "pending" {
            delta *= 0.5;
        }
        if store.apply_signal(target_id, delta).is_ok() {
            applied += 1;
        } else {
            skipped += 1;
        }
    }

    // 无论信号是否命中：这批事实已完成信号检测，标记防重复（下次不再参与）
    let fact_ids: Vec<String> = new_facts.iter().map(|f| f.id.clone()).collect();
    let _ = store.mark_facts_signals_checked(&fact_ids);

    Ok((applied, skipped))
}

/// 去掉 markdown 代码围栏
fn strip_markdown_fence(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("```") {
        let mut lines = s.lines();
        lines.next();
        let mut body: Vec<&str> = lines.collect();
        if let Some(last) = body.last() {
            if last.trim().starts_with("```") {
                body.pop();
            }
        }
        return body.join("\n");
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reflection_id_deterministic() {
        let ids1 = vec!["b".to_string(), "a".to_string(), "c".to_string()];
        let ids2 = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(
            reflection_id_from_facts(&ids1),
            reflection_id_from_facts(&ids2),
            "同批事实（乱序）应产生相同 ID"
        );
        let ids3 = vec!["a".to_string(), "b".to_string(), "d".to_string()];
        assert_ne!(
            reflection_id_from_facts(&ids1),
            reflection_id_from_facts(&ids3),
            "不同批事实应产生不同 ID"
        );
    }

    #[test]
    fn test_evidence_decay() {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        // 无信号时间戳 → 不衰减
        let s1 = crate::memory::evidence_score(1.0, 0.0, None, None, &now);
        assert!((s1 - 1.0).abs() < 1e-9);
        // 30 天前 reinforce → 衰减一半
        let old = chrono::Local::now()
            .checked_sub_signed(chrono::Duration::days(30))
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let s2 = crate::memory::evidence_score(1.0, 0.0, Some(&old), None, &now);
        assert!((s2 - 0.5).abs() < 0.05, "30天半衰期应衰减到 ~0.5, got {}", s2);
        // negate 权重更高 → 同样 +1/-1 但 negate 半衰期 180 天衰减慢
        let s3 = crate::memory::evidence_score(0.0, 1.0, None, Some(&old), &now);
        assert!(s3 < -0.8, "180天半衰期下 30 天前 negate 应接近 -1, got {}", s3);
    }

    /// 真实 API 端到端：提取 → 合成反思 → 信号检测 → 晋升
    #[tokio::test]
    #[ignore]
    async fn e2e_full_evidence_loop() {
        let dir = std::env::temp_dir().join("dagent_e2e_evidence");
        let _ = std::fs::remove_dir_all(&dir);
        let store = MemoryStore::new(dir.clone());

        let cfg = std::fs::read_to_string(r"D:\3Dagent\3dagent\config.json").unwrap();
        let v: serde_json::Value = serde_json::from_str(&cfg).unwrap();
        let key = v["qianwen_api_key"].as_str().unwrap().to_string();

        // 1. 造 6 条未吸收事实（达到反思合成阈值）
        let conv = "主人: 我最近在学机器学习，看了很多书\n优香: 很棒！\n主人: 我每天都会刷两小时算法题\n优香: 加油！\n主人: 我特别喜欢用 Python 写东西\n优香: Python 很适合你\n主人: 我想毕业以后做 AI 工程师\n优香: 一定可以的！\n主人: 我室友也在学深度学习，我们经常讨论\n优香: 有个一起学的伙伴真好\n主人: 我讨厌吃香菜\n优香: 哈哈记住了";
        let (added, _) = crate::memory_extract::extract_facts(&store, conv, &key, "优香", "主人")
            .await
            .expect("事实提取失败");
        println!("[1] 提取事实 {} 条", added);
        assert!(added >= 6, "应有 ≥6 条事实，实际 {}", added);

        // 2. 反思合成（≥5 条未吸收触发）
        let synthesized = crate::memory_reflect::synthesize_reflection(&store, &key, "优香", "主人")
            .await
            .expect("反思合成失败");
        println!("[2] 反思合成: {}", if synthesized { "新增 1 条" } else { "未触发" });
        let reflections = store.get_reflections();
        println!("    反思库: {} 条", reflections.len());
        for r in &reflections {
            println!("    [{}] status={} score={:.2} {}", r.id, r.status, store.reflection_score(r), r.text);
        }

        // 3. 信号检测：再来一轮相似事实 → reinforces 已有反思
        let conv2 = "主人: 我又学了新的机器学习模型\n优香: 好厉害！\n主人: 今天也刷了两小时算法题\n优香: 坚持就是胜利";
        let (added2, _) = crate::memory_extract::extract_facts(&store, conv2, &key, "优香", "主人")
            .await
            .expect("二次提取失败");
        println!("[3] 二次提取 {} 条", added2);
        let (applied, skipped) = crate::memory_reflect::detect_signals(&store, &key)
            .await
            .expect("信号检测失败");
        println!("    信号应用 {} 条, 跳过 {} 条", applied, skipped);

        // 4. 晋升
        let (c, p) = store.promote_reflections();
        println!("[4] 晋升: confirmed {} 条, promoted {} 条", c, p);

        // 5. 最终状态 + 注入
        let ctx = store.render_memory_context(8, 2000);
        println!("[5] 记忆上下文注入:\n{}", ctx);
    }

    #[test]
    fn test_promote_pipeline() {
        let dir = std::env::temp_dir().join(format!(
            "dagent_promote_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MemoryStore::new(dir);

        // 造一条 pending 反思，分数足够 → promote 应确认
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let r = Reflection {
            id: "ref_test1".to_string(),
            text: "主人是个喜欢学习的人".to_string(),
            entity: "master".to_string(),
            relation_type: "trait".to_string(),
            temporal_scope: "pattern".to_string(),
            status: "pending".to_string(),
            source_fact_ids: vec!["f1".to_string()],
            reinforcement: 1.5, // ≥1.0 → confirmed
            disputation: 0.0,
            rein_last_signal_at: Some(now.clone()),
            disp_last_signal_at: None,
            created_at: now.clone(),
            confirmed_at: None,
        };
        store.add_reflection(r).unwrap();

        // 造一条 confirmed 反思，分数 ≥2 → promoted 合入 persona
        let r2 = Reflection {
            id: "ref_test2".to_string(),
            text: "主人对 AI 技术充满热情".to_string(),
            entity: "master".to_string(),
            relation_type: "trait".to_string(),
            temporal_scope: "pattern".to_string(),
            status: "confirmed".to_string(),
            source_fact_ids: vec!["f2".to_string()],
            reinforcement: 2.5, // ≥2.0 → promoted
            disputation: 0.0,
            rein_last_signal_at: Some(now.clone()),
            disp_last_signal_at: None,
            created_at: now.clone(),
            confirmed_at: Some(now.clone()),
        };
        store.add_reflection(r2).unwrap();

        let (confirmed, promoted) = store.promote_reflections();
        assert_eq!(confirmed, 1, "pending→confirmed 应有 1 条");
        assert_eq!(promoted, 1, "confirmed→promoted 应有 1 条");

        let persona = store.get_persona();
        let master = persona.get("master").and_then(|a| a.as_array()).unwrap();
        assert!(
            master.iter().any(|e| e.get("text").and_then(|t| t.as_str()) == Some("主人对 AI 技术充满热情")),
            "promoted 反思应合入 persona"
        );
    }
}
