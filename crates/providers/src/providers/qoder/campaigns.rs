//! Qoder 活动的只读清单与领取请求。每日签到和手动福利共用鉴权，互不代领。
//!
//! ── 观测口径（#446 / #507「上游未下发」族）──────────────────
//! 上游真没下发、本地过滤误杀、风控身份不被采信三者会落到同一条终态文案。
//! 逐行四元组与拉取阶段统一在这里进 verbose，终态分档只需消费这份摘要，
//! 不必再把拉取链路拆一遍。

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::ApiResponse;
use crate::logging;

use super::credentials::Credentials;
use super::endpoints::{self, Site};
use super::{auth, refresh, risk};

pub(super) struct CampaignSession {
    pub credentials: Credentials,
    pub proxy: Option<WireProxy>,
    pub headers: Vec<(String, String)>,
    pub list: Value,
    pub risk_note: Option<String>,
}

pub(super) async fn load(
    ctx: &PluginCtx,
    site: Site,
    account: &str,
) -> Result<CampaignSession, GatewayError> {
    let credentials = refresh::ensure_fresh(ctx, site, account, false).await?;
    match load_with(ctx, site, account, credentials).await {
        Err(error) if error.status_code == 401 => {
            let credentials = refresh::ensure_fresh(ctx, site, account, true).await?;
            load_with(ctx, site, account, credentials).await
        }
        result => result,
    }
}

async fn load_with(
    ctx: &PluginCtx,
    site: Site,
    account: &str,
    credentials: Credentials,
) -> Result<CampaignSession, GatewayError> {
    if !site.supports_checkin() {
        return Err(GatewayError::with_status(400, "该提供商没有活动领取接口"));
    }
    let (record, _) = refresh::snapshot(ctx, site, account).await?;
    let proxy = auth::account_proxy(&record)?;
    let mut headers = endpoints::sash_headers(&credentials.access_token);
    let mut list = fetch(&credentials, proxy.as_ref(), &headers, "首拉").await?;
    let mut risk_note = None;
    if site == Site::QoderIntl {
        let uid = uid_of(&list);
        let (augmented, note) = risk::augment_headers(headers, site, uid).await;
        headers = augmented;
        risk_note = note;
        // 国际版不下发积分活动最常见的成因是 UMID 组件缺失或身份不被采信；
        // 把「头里有没有风控身份」一并留痕，供终态分档区分「没下发」与「没资格」。
        let attached = risk_note.is_none();
        list = fetch(&credentials, proxy.as_ref(), &headers, "风控重拉").await?;
        logging::verbose(
            "[Qoder]",
            &format!(
                "风控重拉阶段：风控身份{}",
                if attached { "已带上" } else { "未带上（见 riskNote）" },
            ),
        );
    }
    Ok(CampaignSession { credentials, proxy, headers, list, risk_note })
}

/// 顶层 uid 是风控身份的输入；缺失时 `augment_headers` 会给出 UMID 组件提示。
pub(super) fn uid_of(list: &Value) -> &str {
    list.get("uid").and_then(Value::as_str).unwrap_or("").trim()
}

pub(super) async fn fetch(
    credentials: &Credentials,
    proxy: Option<&WireProxy>,
    headers: &[(String, String)],
    stage: &str,
) -> Result<Value, GatewayError> {
    let url = format!("{}{}{}", credentials.site.open_api(), endpoints::CAMPAIGNS_PATH, endpoints::CAMPAIGNS_QUERY);
    let response = auth::request("GET", &url, None, headers, proxy).await?;
    let value = unwrap_data(auth::payload(response, "活动查询")?);
    validate_campaign_list(&value)?;
    logging::verbose("[Qoder]", &describe_rows(stage, &value));
    Ok(value)
}

/// 格式异常不能当「今天没活动」：重复或空的活动 ID 会让领取状态无从判断。
pub(super) fn validate_campaign_list(value: &Value) -> Result<(), GatewayError> {
    if let Some(rows) = value.get("campaigns").and_then(Value::as_array) {
        let mut ids = std::collections::HashSet::new();
        if rows.iter().all(|row| row.is_object()
            && row.get("campaignId").and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty() && ids.insert(id))) {
            return Ok(());
        }
    }
    Err(GatewayError::with_status(502, "活动清单格式异常，未确认领取状态"))
}

/// 逐行四元组进 verbose；终态分档与「上游真没给」的区分都靠这份原始读数。
pub(super) fn describe_rows(stage: &str, list: &Value) -> String {
    let rows: Vec<String> = list.get("campaigns").and_then(Value::as_array)
        .into_iter().flatten().take(32).map(|row| {
            format!(
                "{}/{}/{}/{}",
                row.get("actionType").and_then(Value::as_str).unwrap_or("?"),
                row.get("claimStatus").and_then(Value::as_str).unwrap_or("?"),
                row.get("startAt").map(Value::to_string).unwrap_or("?".to_string()),
                row.get("endAt").map(Value::to_string).unwrap_or("?".to_string()),
            )
        }).collect();
    let total = list.get("campaigns").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    format!("活动查询 {stage}：共 {total} 行（逐行 actionType/claimStatus/startAt/endAt）[{}]", rows.join(" | "))
}

pub(super) async fn submit(
    session: &CampaignSession,
    campaign_id: &str,
) -> Result<ApiResponse, GatewayError> {
    let mut url = url::Url::parse(session.credentials.site.open_api())
        .map_err(|_| GatewayError::with_status(500, "活动接口地址无效"))?;
    url.set_path(endpoints::CAMPAIGNS_PATH);
    url.path_segments_mut()
        .map_err(|_| GatewayError::with_status(500, "活动接口地址无效"))?
        .push(campaign_id).push("claim");
    let mut headers = session.headers.clone();
    headers.push(("content-type".to_string(), "application/json".to_string()));
    headers.push(("origin".to_string(), session.credentials.site.open_api().to_string()));
    auth::request("POST", url.as_str(), Some(&json!({})), &headers, session.proxy.as_ref()).await
}

pub(super) fn unwrap_data(value: Value) -> Value {
    match value.get("data") {
        Some(data) if data.is_object() => data.clone(),
        Some(data) if data.is_array() => json!({ "campaigns": data }),
        _ => value,
    }
}

pub(super) fn campaign_time(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64().or_else(|| value.as_str()?.parse().ok()) {
        return (number > 0).then(|| if number < 100_000_000_000 { number.saturating_mul(1000) } else { number });
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?).ok().map(|time| time.timestamp_millis())
}

pub(super) fn campaign_text(row: &Value, locale: &str, field: &str) -> String {
    let placements = row.get("placements").and_then(Value::as_array);
    let text = placements.and_then(|placements| {
        ["POPUP", "USAGE"].into_iter().find_map(|kind| {
            placements.iter()
                .filter(|placement| placement.get("type").and_then(Value::as_str) == Some(kind))
                .find_map(|placement| placement.get("content")?.get(locale)?.get(field)?.as_str()
                    .filter(|text| !text.trim().is_empty()))
        })
    });
    text.or_else(|| row.get(field).and_then(Value::as_str)).unwrap_or("").trim().to_string()
}

pub(super) fn is_credit_benefit(row: &Value) -> bool {
    row.get("actionType").and_then(Value::as_str) == Some("CLAIM_BENEFIT")
        && row.pointer("/benefit/kind").and_then(Value::as_str) == Some("CREDITS")
        && row.get("campaignId").and_then(Value::as_str).is_some_and(|id| !id.trim().is_empty())
}

pub(super) fn is_daily(row: &Value) -> bool {
    if !is_credit_benefit(row) { return false; }
    let title = [campaign_text(row, "zh", "title"), campaign_text(row, "en", "title")].join(" ").to_lowercase();
    let scope = row.pointer("/benefit/modelScope");
    let all_models = scope.and_then(|scope| scope.pointer("/modelSeries/key")).and_then(Value::as_str) == Some("ALL_MODELS");
    let scoped = !all_models && scope.is_some_and(|scope| !scope.is_null() && scope.as_object().is_none_or(|object| !object.is_empty()));
    if scoped || title.contains("sonus") || title.contains("sota") { return false; }
    if title.contains("每日") || title.contains("每天") || title.contains("daily") || title.contains("every day") { return true; }
    // 旧活动未附标题时，仅认已确认的「100 Credits、30 天有效、单日窗口」。
    let one_day = match (campaign_time(row.get("startAt")), campaign_time(row.get("endAt"))) {
        (Some(start), Some(end)) => end > start && end - start <= 86_400_000,
        _ => false,
    };
    one_day && row.pointer("/benefit/amount").and_then(Value::as_f64) == Some(100.0)
        && row.pointer("/benefit/validity/days").and_then(Value::as_f64) == Some(30.0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::credentials::Credentials;
    use super::super::endpoints::Site;
    use super::{
        campaign_text, campaign_time, describe_rows, is_credit_benefit, is_daily, uid_of,
        unwrap_data, validate_campaign_list,
    };

    fn daily_row(id: &str) -> serde_json::Value {
        json!({
            "campaignId": id,
            "actionType": "CLAIM_BENEFIT",
            "claimStatus": "CLAIMABLE",
            "startAt": 1_700_000_000,
            "endAt": 1_700_000_000 + 86_400,
            "benefit": { "kind": "CREDITS", "amount": 100, "validity": { "days": 30 } },
        })
    }

    #[test]
    fn daily_campaign_is_recognised_by_its_window_and_reward() {
        assert!(is_daily(&daily_row("d1")));
        // 标题直接写「每日」也认，窗口形状不必完全吻合。
        let mut titled = daily_row("d2");
        titled["placements"] = json!([{ "type": "POPUP", "content": { "zh": { "title": "每日签到" } } }]);
        assert!(is_daily(&titled));
    }

    #[test]
    fn sonus_and_model_scoped_campaigns_are_not_daily() {
        // 限时福利由「活动福利」手动逐项领取，不能被每日签到误领。
        let mut sonus = daily_row("sonus");
        sonus["placements"] = json!([{ "type": "POPUP", "content": { "en": { "title": "Sonus limited trial" } } }]);
        assert!(!is_daily(&sonus));

        let mut scoped = daily_row("scoped");
        scoped["benefit"]["modelScope"] = json!({ "modelSeries": { "key": "GPT" } });
        assert!(!is_daily(&scoped));

        // 促销行与缺 ID 的行连候选都不是。
        let mut promo = daily_row("promo");
        promo["actionType"] = json!("VIEW_DETAILS");
        assert!(!is_credit_benefit(&promo));
        let mut no_id = daily_row("");
        no_id["campaignId"] = json!("");
        assert!(!is_credit_benefit(&no_id));
    }

    #[test]
    fn campaign_list_rejects_duplicate_or_malformed_rows() {
        assert!(validate_campaign_list(&json!({ "campaigns": [daily_row("a"), daily_row("b")] })).is_ok());
        let duplicate = json!({ "campaigns": [daily_row("same"), daily_row("same")] });
        assert_eq!(validate_campaign_list(&duplicate).unwrap_err().status_code, 502);
        let non_object = json!({ "campaigns": ["oops"] });
        assert_eq!(validate_campaign_list(&non_object).unwrap_err().status_code, 502);
        assert_eq!(validate_campaign_list(&json!({})).unwrap_err().status_code, 502);
        let mut blank = daily_row(" ");
        blank["campaignId"] = json!("   ");
        assert_eq!(validate_campaign_list(&json!({ "campaigns": [blank] })).unwrap_err().status_code, 502);
    }

    #[test]
    fn campaign_time_accepts_seconds_millis_and_rfc3339() {
        assert_eq!(campaign_time(Some(&json!(1_700_000_000))), Some(1_700_000_000_000_i64));
        assert_eq!(campaign_time(Some(&json!(1_700_000_000_000_i64))), Some(1_700_000_000_000_i64));
        assert_eq!(campaign_time(Some(&json!("1700000000"))), Some(1_700_000_000_000_i64));
        assert!(campaign_time(Some(&json!("2026-11-06T10:00:00+08:00"))).is_some());
        assert_eq!(campaign_time(Some(&json!(0))), None);
        assert_eq!(campaign_time(Some(&json!(-1))), None);
        assert_eq!(campaign_time(Some(&json!("garbage"))), None);
        assert_eq!(campaign_time(None), None);
    }

    #[test]
    fn campaign_text_reads_placements_before_top_level() {
        let row = json!({
            "title": "兜底标题",
            "description": "兜底描述",
            "placements": [
                { "type": "POPUP", "content": { "zh": { "title": "中文标题" }, "en": { "title": "English title" } } },
            ],
        });
        assert_eq!(campaign_text(&row, "zh", "title"), "中文标题");
        assert_eq!(campaign_text(&row, "en", "title"), "English title");
        assert_eq!(campaign_text(&row, "zh", "description"), "兜底描述");
        assert_eq!(campaign_text(&json!({}), "zh", "title"), "");
    }

    #[test]
    fn unwrap_data_handles_object_array_and_bare_shapes() {
        assert_eq!(unwrap_data(json!({ "data": { "a": 1 } })), json!({ "a": 1 }));
        assert_eq!(unwrap_data(json!({ "data": [daily_row("x")] })), json!({ "campaigns": [daily_row("x")] }));
        assert_eq!(unwrap_data(json!({ "a": 1 })), json!({ "a": 1 }));
    }

    #[test]
    fn uid_of_drives_the_risk_refetch() {
        assert_eq!(uid_of(&json!({ "uid": "u-1" })), "u-1");
        assert_eq!(uid_of(&json!({ "uid": "  u-2  " })), "u-2");
        assert_eq!(uid_of(&json!({ "campaigns": [] })), "");
        assert_eq!(uid_of(&json!({ "uid": 42 })), "");
        assert_eq!(uid_of(&json!({ "uid": "   " })), "");
    }

    #[test]
    fn describe_rows_keeps_the_raw_four_tuple_for_no_claim_tiering() {
        let list = json!({ "campaigns": [daily_row("d1"), daily_row("d2")] });
        let text = describe_rows("首拉", &list);
        assert!(text.contains("共 2 行"), "要带活动总数：{text}");
        assert!(text.contains("CLAIM_BENEFIT/CLAIMABLE/"), "要带逐行四元组：{text}");
        assert!(text.contains("首拉"), "要区分首拉与风控重拉：{text}");
    }

    #[test]
    fn provider_site_overrides_the_mode_derived_site_for_umid_lookup() {
        // 记录里的 mode 只有 cn/global，分不出 Qoder 与千问办公；风控身份的
        // 组件查找与缓存键拿 provider 身份才算数（refresh_models 的补齐点）。
        let payload = json!({
            "mode": "cn",
            "accessToken": "at",
            "refreshToken": "rt",
            "userId": "u1",
            "machineId": "m1",
        });
        let mut credentials = Credentials::from_payload(&payload).expect("凭证可解析");
        assert_eq!(credentials.site.provider_id(), "qoder");
        credentials.set_site(Site::QoderIntl);
        assert_eq!(credentials.site.provider_id(), "qoder-intl");
        assert_eq!(credentials.site.id(), "global");
    }

    #[test]
    fn one_day_window_boundary_is_not_daily_when_it_spans_more_than_a_day() {
        let mut long = daily_row("long");
        long["endAt"] = json!(1_700_000_000 + 86_401);
        assert!(!is_daily(&long));
    }
}
