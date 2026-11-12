use serde_json::{json, Map, Value};

use crate::model_rules;

const BASE_ID: &str = "gemini-4-argon";
const VARIANTS: [(&str, &str); 3] = [
    ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
    ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
    ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
];

pub(super) fn is_model(id: &str) -> bool {
    default_tier(id).is_some()
}

fn default_tier(id: &str) -> Option<usize> {
    let id = id.trim();
    if id.eq_ignore_ascii_case(BASE_ID) {
        return Some(2);
    }
    VARIANTS
        .iter()
        .position(|(known, _)| id.eq_ignore_ascii_case(known))
}

pub(super) fn resolve_model(id: &str, effort: Option<&str>) -> Option<&'static str> {
    let default = default_tier(id)?;
    let tier = effort
        .and_then(model_rules::reasoning_rank)
        .map(|rank| match rank {
            0 | 1 => 0,
            2 => 1,
            _ => 2,
        })
        .unwrap_or(default);
    VARIANTS.get(tier).map(|(id, _)| *id)
}

pub(super) fn thinking_config(chat: &Value, model: &str) -> Option<Value> {
    let effort = model_rules::read_client_level(chat);
    if effort.as_deref().is_some_and(model_rules::reasoning_is_off) {
        return None;
    }
    let model = resolve_model(model, effort.as_deref())?;
    let level = match default_tier(model)? {
        0 => "LOW",
        1 => "MEDIUM",
        _ => "HIGH",
    };
    // Argon 使用等级而非数字预算；低档不能降成不受支持的 MINIMAL。
    Some(json!({ "includeThoughts": true, "thinkingLevel": level }))
}

pub(super) fn is_quota_bucket(group: &Value, bucket: &Value) -> bool {
    [
        group.get("displayName"),
        bucket.get("bucketId"),
        bucket.get("displayName"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .any(|text| text.to_ascii_lowercase().contains("argon"))
}

pub(super) fn has_dedicated_quota(summary: &Value) -> bool {
    summary
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|group| {
            group
                .get("buckets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|bucket| is_quota_bucket(group, bucket))
        })
}

fn quota_info(summary: &Value) -> Value {
    let tightest = summary
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|group| {
            group
                .get("buckets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(move |bucket| is_quota_bucket(group, bucket))
        })
        .filter_map(|bucket| {
            let fraction = bucket.get("remainingFraction")?.as_f64()?;
            (fraction.is_finite() && (0.0..=1.0).contains(&fraction)).then_some((fraction, bucket))
        })
        .min_by(|(left, _), (right, _)| left.total_cmp(right));
    let Some((fraction, bucket)) = tightest else {
        return Value::Null;
    };
    let mut quota = Map::new();
    quota.insert("remainingFraction".to_string(), json!(fraction));
    if let Some(reset) = bucket
        .get("resetTime")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        quota.insert("resetTime".to_string(), json!(reset));
    }
    Value::Object(quota)
}

pub(super) fn supplement(models: &mut Vec<Value>, summary: Option<&Value>) {
    let listed = models.iter().any(|model| {
        model
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(is_model)
    });
    // 只接受同一账号的专属额度证据；不从 Pro 标签或普通 Gemini 额度推断权限。
    if !listed {
        if let Some(summary) = summary.filter(|value| has_dedicated_quota(value)) {
            let quota = quota_info(summary);
            for (id, name) in VARIANTS {
                let info = json!({
                    "displayName": name,
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1_048_576,
                    "maxOutputTokens": 65_536,
                    "quotaInfo": quota,
                });
                if let Some(model) = super::models::map_entry(id, &info) {
                    models.push(model);
                }
            }
        }
    }
    if models
        .iter()
        .any(|model| model.get("id").and_then(Value::as_str) == Some(BASE_ID))
    {
        return;
    }
    if let Some(mut alias) = models
        .iter()
        .find(|model| model.get("id").and_then(Value::as_str) == Some("gemini-4-argon-high"))
        .cloned()
    {
        alias["id"] = json!(BASE_ID);
        alias["name"] = json!("Gemini 4 Argon");
        models.push(alias);
    }
}

pub(super) fn quota_models(summary: &Value, group: &Value, bucket: &Value) -> Vec<String> {
    let catalog = super::models::list();
    quota_model_ids(&catalog, summary, group, bucket)
}

fn quota_model_ids(
    known: &[Value],
    summary: &Value,
    group: &Value,
    bucket: &Value,
) -> Vec<String> {
    let dedicated = is_quota_bucket(group, bucket);
    let separate_argon = has_dedicated_quota(summary);
    let name = group
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let bucket_id = bucket
        .get("bucketId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let third_party =
        name.contains("claude") || name.contains("gpt") || bucket_id.starts_with("3p-");
    let gemini = !third_party && (name.contains("gemini") || bucket_id.starts_with("gemini-"));
    let mut ids: Vec<String> = known
        .iter()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            let lower = id.to_ascii_lowercase();
            let matches = if dedicated {
                is_model(id)
            } else if third_party {
                lower.starts_with("claude-") || lower.starts_with("gpt-oss-")
            } else {
                gemini && lower.starts_with("gemini-") && !(separate_argon && is_model(id))
            };
            matches.then(|| id.to_string())
        })
        .collect();
    if dedicated || (gemini && !separate_argon) {
        // 余额可能先于目录刷新；明确模型范围不等于声明该账号已获得模型权限。
        for id in std::iter::once(BASE_ID).chain(VARIANTS.iter().map(|(id, _)| *id)) {
            if !ids.iter().any(|known| known == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::{
        has_dedicated_quota, is_model, is_quota_bucket, quota_model_ids, quota_models,
        resolve_model, supplement, thinking_config, BASE_ID, VARIANTS,
    };
    use serde_json::{json, Value};

    fn dedicated_summary() -> Value {
        json!({
            "groups": [
                {
                    "displayName": "Gemini 4 Argon",
                    "buckets": [{
                        "bucketId": "argon-5h",
                        "remainingFraction": 0.25,
                        "resetTime": "2026-11-13T00:00:00Z"
                    }]
                },
                {
                    "displayName": "Gemini Models",
                    "buckets": [{
                        "bucketId": "gemini-5h",
                        "remainingFraction": 0.75
                    }]
                }
            ]
        })
    }

    fn generic_summary() -> Value {
        json!({
            "groups": [{
                "displayName": "Gemini Models",
                "buckets": [{
                    "bucketId": "gemini-5h",
                    "remainingFraction": 0.75
                }]
            }]
        })
    }

    fn known_catalog() -> Vec<Value> {
        [
            ("gemini-3-flash", json!({"displayName": "Gemini 3 Flash"})),
            ("claude-sonnet-4-6", Value::Null),
        ]
        .into_iter()
        .filter_map(|(id, info)| super::super::models::map_entry(id, &info))
        .collect()
    }

    fn argon_ids() -> Vec<String> {
        std::iter::once(BASE_ID.to_string())
            .chain(VARIANTS.iter().map(|(id, _)| (*id).to_string()))
            .collect()
    }

    fn ids_of(models: &[Value]) -> Vec<&str> {
        models
            .iter()
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .collect()
    }

    fn count_id(models: &[Value], id: &str) -> usize {
        ids_of(models)
            .into_iter()
            .filter(|known| *known == id)
            .count()
    }

    fn count_string(ids: &[String], id: &str) -> usize {
        ids.iter().filter(|known| known.as_str() == id).count()
    }

    #[test]
    fn three_tiers_have_ids_and_display_names() {
        assert_eq!(VARIANTS.len(), 3);
        assert_eq!(VARIANTS[0], ("gemini-4-argon-low", "Gemini 4 Argon (Low)"));
        assert_eq!(
            VARIANTS[1],
            ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)")
        );
        assert_eq!(VARIANTS[2], ("gemini-4-argon-high", "Gemini 4 Argon (High)"));
        assert_eq!(BASE_ID, "gemini-4-argon");
    }

    #[test]
    fn resolve_model_maps_effort_and_keeps_default_tier() {
        assert_eq!(resolve_model(BASE_ID, Some("minimal")), Some(VARIANTS[0].0));
        assert_eq!(resolve_model(BASE_ID, Some("low")), Some(VARIANTS[0].0));
        assert_eq!(resolve_model(BASE_ID, Some("medium")), Some(VARIANTS[1].0));
        assert_eq!(resolve_model(BASE_ID, Some("high")), Some(VARIANTS[2].0));
        assert_eq!(resolve_model(BASE_ID, Some("xhigh")), Some(VARIANTS[2].0));
        assert_eq!(resolve_model(BASE_ID, None), Some(VARIANTS[2].0));
        assert_eq!(resolve_model(BASE_ID, Some("unknown")), Some(VARIANTS[2].0));
        assert_eq!(resolve_model(BASE_ID, Some("off")), Some(VARIANTS[2].0));
        assert_eq!(resolve_model(VARIANTS[0].0, None), Some(VARIANTS[0].0));
        assert_eq!(resolve_model(VARIANTS[1].0, None), Some(VARIANTS[1].0));
        assert_eq!(resolve_model(VARIANTS[2].0, None), Some(VARIANTS[2].0));
        let upper = BASE_ID.to_ascii_uppercase();
        assert_eq!(resolve_model(&upper, Some("medium")), Some(VARIANTS[1].0));
        assert_eq!(resolve_model("gemini-3-flash", Some("high")), None);
    }

    #[test]
    fn thinking_config_uses_levels_and_omits_off() {
        for (effort, level) in [("low", "LOW"), ("medium", "MEDIUM"), ("high", "HIGH")] {
            assert_eq!(
                thinking_config(&json!({"reasoning_effort": effort}), BASE_ID),
                Some(json!({ "includeThoughts": true, "thinkingLevel": level }))
            );
        }
        assert_eq!(
            thinking_config(&json!({"reasoning": {"effort": "medium"}}), VARIANTS[0].0),
            Some(json!({ "includeThoughts": true, "thinkingLevel": "MEDIUM" }))
        );
        assert_eq!(
            thinking_config(&json!({}), VARIANTS[2].0),
            Some(json!({ "includeThoughts": true, "thinkingLevel": "HIGH" }))
        );
        for level in ["off", "none"] {
            assert_eq!(
                thinking_config(&json!({"reasoning_effort": level}), BASE_ID),
                None
            );
        }
        assert_eq!(
            thinking_config(&json!({"reasoning_effort": "high"}), "gemini-3-flash"),
            None
        );
    }

    #[test]
    fn is_offered_model_accepts_argon_without_changing_other_prefixes() {
        assert!(super::super::models::is_offered_model(BASE_ID));
        for (id, _) in VARIANTS {
            assert!(super::super::models::is_offered_model(id));
        }
        assert!(super::super::models::is_offered_model("gemini-3-flash"));
        assert!(super::super::models::is_offered_model("claude-sonnet-4-6"));
        assert!(super::super::models::is_offered_model("gpt-oss-120b-medium"));
        assert!(!super::super::models::is_offered_model("veo-3"));

        let entry = super::super::models::map_entry(VARIANTS[0].0, &json!({}));
        assert!(entry.is_some(), "Argon 条目应通过目录白名单");
        let Some(entry) = entry else { return; };
        assert_eq!(
            entry.get("supportsReasoning").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            entry.get("supportsThinking").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            entry.get("visibility").and_then(Value::as_str),
            Some("shown")
        );
        assert_eq!(super::super::models::upstream_model_id(BASE_ID), VARIANTS[2].0);
        assert_eq!(
            super::super::models::upstream_model_id(VARIANTS[0].0),
            VARIANTS[0].0
        );
        assert_eq!(
            super::super::models::upstream_model_id("gemini-3.6-flash"),
            "gemini-3.6-flash-tiered"
        );
        assert_eq!(
            super::super::models::upstream_model_id("gemini-3-flash"),
            "gemini-3-flash"
        );
    }

    #[test]
    fn supplement_adds_three_tiers_once_and_keeps_existing_models() {
        let summary = dedicated_summary();
        let mut models = known_catalog();
        assert_eq!(models.len(), 2);
        supplement(&mut models, Some(&summary));
        supplement(&mut models, Some(&summary));

        for (id, name) in VARIANTS {
            assert_eq!(count_id(&models, id), 1);
            let entry = models
                .iter()
                .find(|model| model.get("id").and_then(Value::as_str) == Some(id));
            assert!(entry.is_some(), "三档模型应补齐");
            let Some(entry) = entry else { return; };
            assert_eq!(entry.get("name").and_then(Value::as_str), Some(name));
            assert_eq!(
                entry
                    .get("quota")
                    .and_then(Value::as_object)
                    .and_then(|quota| quota.get("remainingFraction"))
                    .and_then(Value::as_f64),
                Some(0.25)
            );
            assert_eq!(
                entry
                    .get("quota")
                    .and_then(Value::as_object)
                    .and_then(|quota| quota.get("resetTime"))
                    .and_then(Value::as_str),
                Some("2026-11-13T00:00:00Z")
            );
        }
        assert_eq!(count_id(&models, BASE_ID), 1);
        let alias = models
            .iter()
            .find(|model| model.get("id").and_then(Value::as_str) == Some(BASE_ID));
        assert!(alias.is_some(), "应补齐聚合别名");
        let Some(alias) = alias else { return; };
        assert_eq!(
            alias.get("name").and_then(Value::as_str),
            Some("Gemini 4 Argon")
        );
        assert_eq!(count_id(&models, "gemini-3-flash"), 1);
        assert_eq!(count_id(&models, "claude-sonnet-4-6"), 1);
        assert_eq!(models.len(), known_catalog().len() + VARIANTS.len() + 1);
    }

    #[test]
    fn supplement_ignores_missing_or_generic_quota() {
        let mut models = known_catalog();
        assert_eq!(models.len(), 2);
        supplement(&mut models, None);
        assert_eq!(models.len(), 2);
        supplement(&mut models, Some(&generic_summary()));
        assert_eq!(models.len(), 2);
        assert!(ids_of(&models).into_iter().all(|id| !is_model(id)));
    }

    #[test]
    fn has_dedicated_quota_checks_group_and_bucket_names() {
        assert!(has_dedicated_quota(&dedicated_summary()));
        assert!(!has_dedicated_quota(&generic_summary()));
        assert!(!has_dedicated_quota(&json!({})));
        let group = json!({"displayName": "Other"});
        let bucket = json!({"bucketId": "shared", "displayName": "Gemini 4 Argon"});
        assert!(is_quota_bucket(&group, &bucket));
    }

    #[test]
    fn quota_models_split_dedicated_and_generic_buckets() {
        let known = known_catalog();
        assert_eq!(known.len(), 2);
        let summary = dedicated_summary();
        let dedicated_group = &summary["groups"][0];
        let dedicated_bucket = &dedicated_group["buckets"][0];
        let dedicated = quota_model_ids(&known, &summary, dedicated_group, dedicated_bucket);
        for id in argon_ids() {
            assert_eq!(count_string(&dedicated, &id), 1);
        }
        assert_eq!(dedicated.len(), argon_ids().len());
        assert!(!dedicated.contains(&"gemini-3-flash".to_string()));
        assert!(!dedicated.contains(&"claude-sonnet-4-6".to_string()));

        let generic_group = &summary["groups"][1];
        let generic_bucket = &generic_group["buckets"][0];
        let generic = quota_model_ids(&known, &summary, generic_group, generic_bucket);
        assert!(generic.contains(&"gemini-3-flash".to_string()));
        assert!(!generic.contains(&"claude-sonnet-4-6".to_string()));
        for id in argon_ids() {
            assert_eq!(count_string(&generic, &id), 0);
        }
    }

    #[test]
    fn quota_models_include_argon_without_dedicated_bucket() {
        let known = known_catalog();
        assert_eq!(known.len(), 2);
        let summary = generic_summary();
        let group = &summary["groups"][0];
        let bucket = &group["buckets"][0];
        let ids = quota_model_ids(&known, &summary, group, bucket);
        assert!(!has_dedicated_quota(&summary));
        assert!(ids.contains(&"gemini-3-flash".to_string()));
        assert!(!ids.contains(&"claude-sonnet-4-6".to_string()));
        for id in argon_ids() {
            assert_eq!(count_string(&ids, &id), 1);
        }
        assert_eq!(ids.len(), 1 + argon_ids().len());
    }

    #[test]
    fn quota_models_uses_current_catalog() {
        let summary = generic_summary();
        let group = &summary["groups"][0];
        let bucket = &group["buckets"][0];
        let ids = quota_models(&summary, group, bucket);
        let known = super::super::models::list()
            .iter()
            .find_map(|model| {
                let id = model.get("id").and_then(Value::as_str)?;
                (id.starts_with("gemini-") && !is_model(id)).then(|| id.to_string())
            });
        assert!(known.is_some(), "当前目录应有非 Argon 的 Gemini 模型");
        let Some(known) = known else { return; };
        assert!(ids.contains(&known));
        assert!(ids.iter().any(|id| is_model(id)));
    }
}
