use regex::Regex;
use tgm_core::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordMatchType {
    Exact,
    Regex,
}

#[derive(Debug, Clone)]
pub struct RuleKeyword {
    pub id: i64,
    pub word: String,
    pub match_type: KeywordMatchType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Copy,
    Forward,
    Resend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleConditionKind {
    ExcludeWord,
    FileName,
    SizeMin,
    SizeMax,
    Sender,
    ExtAllow,
    ExtDeny,
}

#[derive(Debug, Clone)]
pub struct RuleCondition {
    pub kind: RuleConditionKind,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: i64,
    pub tenant_id: i64,
    pub keyword: RuleKeyword,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub action: RuleAction,
    pub priority: i32,
    pub stop_processing: bool,
    pub version: i32,
    pub enabled: bool,
    pub conditions: Vec<RuleCondition>,
}

#[derive(Debug, Clone, Default)]
pub struct RuleEvent {
    pub source_ref: i64,
    pub media_type: Option<String>,
    pub sender_id: Option<i64>,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    pub rule_id: i64,
    pub keyword_id: i64,
    pub matched_keyword: String,
    pub target_ref: i64,
    pub action: RuleAction,
    pub rule_version: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEvaluationStep {
    pub stage: &'static str,
    pub field: String,
    pub pass: bool,
    pub detail: Option<String>,
    pub matched_at: Option<Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEvaluation {
    pub matched: bool,
    pub trace: Vec<RuleEvaluationStep>,
}

pub fn evaluate_rules(event: &RuleEvent, rules: &[Rule]) -> Result<Vec<RuleMatch>, AppError> {
    let mut ordered: Vec<&Rule> = rules.iter().filter(|rule| rule.enabled).collect();
    ordered.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then(left.id.cmp(&right.id))
    });

    let mut matches = Vec::new();

    for rule in ordered {
        if !evaluate_rule(event, rule)?.matched {
            continue;
        }

        matches.push(RuleMatch {
            rule_id: rule.id,
            keyword_id: rule.keyword.id,
            matched_keyword: rule.keyword.word.clone(),
            target_ref: rule.target_ref,
            action: rule.action,
            rule_version: rule.version,
        });
        if rule.stop_processing {
            break;
        }
    }

    Ok(matches)
}

pub fn evaluate_rule(event: &RuleEvent, rule: &Rule) -> Result<RuleEvaluation, AppError> {
    let body = event_body(event);
    let mut trace = Vec::new();

    if !rule.enabled {
        trace.push(RuleEvaluationStep {
            stage: "enabled",
            field: "rule".to_owned(),
            pass: false,
            detail: Some("规则已停用".to_owned()),
            matched_at: None,
        });
        return Ok(RuleEvaluation {
            matched: false,
            trace,
        });
    }

    let source_pass = rule
        .source_ref
        .is_none_or(|source| source == event.source_ref);
    trace.push(RuleEvaluationStep {
        stage: "scope_gate",
        field: "source_ref".to_owned(),
        pass: source_pass,
        detail: (!source_pass).then(|| {
            format!(
                "消息来源 {} 不匹配规则来源 {}",
                event.source_ref,
                rule.source_ref.unwrap_or_default()
            )
        }),
        matched_at: None,
    });

    let media_pass = rule.media_type.as_deref().is_none_or(|expected| {
        expected.eq_ignore_ascii_case("ANY")
            || event
                .media_type
                .as_deref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
    });
    trace.push(RuleEvaluationStep {
        stage: "scope_gate",
        field: "media_type".to_owned(),
        pass: media_pass,
        detail: (!media_pass).then(|| {
            format!(
                "消息媒体类型 {:?} 不匹配规则媒体类型 {:?}",
                event.media_type, rule.media_type
            )
        }),
        matched_at: None,
    });
    if !source_pass || !media_pass {
        return Ok(RuleEvaluation {
            matched: false,
            trace,
        });
    }

    let keyword_match = keyword_matches_text_with_offset(&body, &rule.keyword)?;
    trace.push(RuleEvaluationStep {
        stage: "trigger",
        field: "keyword".to_owned(),
        pass: keyword_match.is_some(),
        detail: (!keyword_match.is_some()).then(|| "未命中关键词".to_owned()),
        matched_at: keyword_match.clone(),
    });
    if keyword_match.is_none() {
        return Ok(RuleEvaluation {
            matched: false,
            trace,
        });
    }

    validate_condition_counts(&rule.conditions)?;
    let mut has_narrowing = false;
    for condition in &rule.conditions {
        let is_veto = matches!(
            condition.kind,
            RuleConditionKind::ExcludeWord | RuleConditionKind::ExtDeny
        );
        if !is_veto {
            continue;
        }
        let denied = match condition.kind {
            RuleConditionKind::ExcludeWord => body.contains(&condition.value),
            RuleConditionKind::ExtDeny => extension_matches(event, &condition.value),
            _ => false,
        };
        trace.push(RuleEvaluationStep {
            stage: "veto",
            field: condition_kind_name(condition.kind).to_owned(),
            pass: !denied,
            detail: denied
                .then(|| format!("命中否决条件 {:?}: {}", condition.kind, condition.value)),
            matched_at: if denied && condition.kind == RuleConditionKind::ExcludeWord {
                body.find(&condition.value)
                    .map(|start| vec![start, start + condition.value.len()])
            } else {
                None
            },
        });
        if denied {
            return Ok(RuleEvaluation {
                matched: false,
                trace,
            });
        }
    }

    let mut narrowing_pass = true;
    for kind in [
        RuleConditionKind::Sender,
        RuleConditionKind::FileName,
        RuleConditionKind::SizeMin,
        RuleConditionKind::SizeMax,
        RuleConditionKind::ExtAllow,
    ] {
        let conditions: Vec<&RuleCondition> = rule
            .conditions
            .iter()
            .filter(|condition| condition.kind == kind)
            .collect();
        if conditions.is_empty() {
            continue;
        }
        has_narrowing = true;
        let pass = match kind {
            RuleConditionKind::Sender => match event.sender_id {
                Some(sender) => {
                    let sender = sender.to_string();
                    conditions.iter().any(|condition| condition.value == sender)
                }
                None => false,
            },
            RuleConditionKind::FileName => event.file_name.as_deref().is_some_and(|file_name| {
                conditions
                    .iter()
                    .any(|condition| file_name.contains(&condition.value))
            }),
            RuleConditionKind::SizeMin => {
                let minimum = single_i64(&rule.conditions, kind)?
                    .ok_or_else(|| AppError::ConditionInvalid("size_min 条件缺少值".to_owned()))?;
                event.file_size.is_some_and(|size| size >= minimum)
            }
            RuleConditionKind::SizeMax => {
                let maximum = single_i64(&rule.conditions, kind)?
                    .ok_or_else(|| AppError::ConditionInvalid("size_max 条件缺少值".to_owned()))?;
                event.file_size.is_some_and(|size| size <= maximum)
            }
            RuleConditionKind::ExtAllow => {
                event.file_name.is_some()
                    && conditions
                        .iter()
                        .any(|condition| extension_matches(event, &condition.value))
            }
            _ => true,
        };
        trace.push(RuleEvaluationStep {
            stage: "narrowing",
            field: condition_kind_name(kind).to_owned(),
            pass,
            detail: (!pass).then(|| "收窄条件未满足".to_owned()),
            matched_at: None,
        });
        narrowing_pass &= pass;
        if !pass {
            break;
        }
    }

    if !has_narrowing {
        trace.push(RuleEvaluationStep {
            stage: "narrowing",
            field: "conditions".to_owned(),
            pass: true,
            detail: None,
            matched_at: None,
        });
    }

    Ok(RuleEvaluation {
        matched: narrowing_pass,
        trace,
    })
}

fn event_body(event: &RuleEvent) -> String {
    [event.text.as_deref(), event.caption.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn keyword_matches_text(body: &str, keyword: &RuleKeyword) -> Result<bool, AppError> {
    Ok(keyword_matches_text_with_offset(body, keyword)?.is_some())
}

fn keyword_matches_text_with_offset(
    body: &str,
    keyword: &RuleKeyword,
) -> Result<Option<Vec<usize>>, AppError> {
    if keyword.word.is_empty() {
        return Ok(None);
    }

    match keyword.match_type {
        KeywordMatchType::Exact => Ok(body
            .find(&keyword.word)
            .map(|start| vec![start, start + keyword.word.len()])),
        KeywordMatchType::Regex => {
            if keyword.word.len() > 1024 {
                return Err(AppError::RegexInvalid(
                    "正则表达式长度不得超过 1 KiB".to_string(),
                ));
            }
            let regex = Regex::new(&keyword.word)
                .map_err(|error| AppError::RegexInvalid(error.to_string()))?;
            Ok(regex
                .find(body)
                .map(|matched| vec![matched.start(), matched.end()]))
        }
    }
}

fn condition_kind_name(kind: RuleConditionKind) -> &'static str {
    match kind {
        RuleConditionKind::ExcludeWord => "exclude_word",
        RuleConditionKind::FileName => "file_name",
        RuleConditionKind::SizeMin => "size_min",
        RuleConditionKind::SizeMax => "size_max",
        RuleConditionKind::Sender => "sender",
        RuleConditionKind::ExtAllow => "ext_allow",
        RuleConditionKind::ExtDeny => "ext_deny",
    }
}

fn validate_condition_counts(conditions: &[RuleCondition]) -> Result<(), AppError> {
    for kind in [RuleConditionKind::SizeMin, RuleConditionKind::SizeMax] {
        if conditions
            .iter()
            .filter(|condition| condition.kind == kind)
            .count()
            > 1
        {
            return Err(AppError::ConditionInvalid(format!("{kind:?} 只能出现一次")));
        }
    }
    Ok(())
}

fn single_i64(
    conditions: &[RuleCondition],
    kind: RuleConditionKind,
) -> Result<Option<i64>, AppError> {
    conditions
        .iter()
        .find(|condition| condition.kind == kind)
        .map(|condition| {
            condition
                .value
                .parse::<i64>()
                .map_err(|_| AppError::ConditionInvalid(format!("{kind:?} 必须是整数")))
        })
        .transpose()
}

fn extension_matches(event: &RuleEvent, expected: &str) -> bool {
    let Some(file_name) = event.file_name.as_deref() else {
        return false;
    };
    let Some(extension) = file_name.rsplit_once('.').map(|(_, extension)| extension) else {
        return false;
    };
    let expected = expected.trim_start_matches('.');
    !expected.is_empty() && extension.eq_ignore_ascii_case(expected)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        KeywordMatchType, Rule, RuleAction, RuleCondition, RuleConditionKind, RuleEvent,
        RuleKeyword, evaluate_rules,
    };

    fn rule(id: i64, word: &str, priority: i32) -> Rule {
        Rule {
            id,
            tenant_id: 1,
            keyword: RuleKeyword {
                id: id + 100,
                word: word.to_string(),
                match_type: KeywordMatchType::Exact,
            },
            source_ref: None,
            media_type: None,
            target_ref: id + 1000,
            action: RuleAction::Copy,
            priority,
            stop_processing: false,
            version: 1,
            enabled: true,
            conditions: Vec::new(),
        }
    }

    fn event() -> RuleEvent {
        RuleEvent {
            source_ref: 7,
            media_type: Some("DOCUMENT".to_string()),
            sender_id: Some(42),
            text: Some("urgent price update".to_string()),
            caption: None,
            file_name: Some("report.PDF".to_string()),
            file_size: Some(2048),
        }
    }

    #[test]
    fn matches_exact_keyword_and_stable_priority_order() {
        let matches = evaluate_rules(&event(), &[rule(2, "price", 10), rule(1, "urgent", 10)])
            .expect("规则匹配失败");
        assert_eq!(
            matches
                .iter()
                .map(|matched| matched.rule_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn scope_and_media_gates_run_before_trigger() {
        let mut source_rule = rule(1, "urgent", 1);
        source_rule.source_ref = Some(99);
        let mut media_rule = rule(2, "urgent", 1);
        media_rule.media_type = Some("PHOTO".to_string());

        assert!(
            evaluate_rules(&event(), &[source_rule, media_rule])
                .expect("范围判断失败")
                .is_empty()
        );
    }

    #[test]
    fn deny_conditions_win_over_narrowing_conditions() {
        let mut rule = rule(1, "urgent", 1);
        rule.conditions = vec![
            RuleCondition {
                kind: RuleConditionKind::Sender,
                value: "42".to_string(),
            },
            RuleCondition {
                kind: RuleConditionKind::ExcludeWord,
                value: "price".to_string(),
            },
        ];

        assert!(
            evaluate_rules(&event(), &[rule])
                .expect("否决条件判断失败")
                .is_empty()
        );
    }

    #[test]
    fn sender_file_size_and_extension_filters_are_anded() {
        let mut rule = rule(1, "urgent", 1);
        rule.conditions = vec![
            RuleCondition {
                kind: RuleConditionKind::Sender,
                value: "42".to_string(),
            },
            RuleCondition {
                kind: RuleConditionKind::FileName,
                value: "report".to_string(),
            },
            RuleCondition {
                kind: RuleConditionKind::SizeMin,
                value: "1024".to_string(),
            },
            RuleCondition {
                kind: RuleConditionKind::ExtAllow,
                value: ".pdf".to_string(),
            },
        ];

        assert_eq!(
            evaluate_rules(&event(), &[rule])
                .expect("收窄条件判断失败")
                .len(),
            1
        );
    }

    #[test]
    fn stop_processing_stops_lower_priority_rules() {
        let mut first = rule(1, "urgent", 20);
        first.stop_processing = true;
        let second = rule(2, "price", 10);

        let matches = evaluate_rules(&event(), &[second, first]).expect("停止规则判断失败");
        assert_eq!(
            matches
                .iter()
                .map(|matched| matched.rule_id)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn invalid_regex_and_duplicate_size_conditions_are_rejected() {
        let mut regex_rule = rule(1, "[", 1);
        regex_rule.keyword.match_type = KeywordMatchType::Regex;
        let error = evaluate_rules(&event(), &[regex_rule]).expect_err("非法正则竟然通过");
        assert_eq!(error.code(), "REGEX_INVALID");

        let mut duplicate_size_rule = rule(2, "urgent", 1);
        duplicate_size_rule.conditions = vec![
            RuleCondition {
                kind: RuleConditionKind::SizeMin,
                value: "1".to_string(),
            },
            RuleCondition {
                kind: RuleConditionKind::SizeMin,
                value: "2".to_string(),
            },
        ];
        let error =
            evaluate_rules(&event(), &[duplicate_size_rule]).expect_err("重复 size_min 竟然通过");
        assert_eq!(error.code(), "CONDITION_INVALID");
    }
}
