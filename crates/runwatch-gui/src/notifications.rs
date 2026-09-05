use crate::model::{RunRow, status_label};
#[cfg(test)]
use crate::settings::GuiSettings;
use runwatch_core::RunStatus;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Success,
    Attention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub title: String,
    pub body: String,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeNotice {
    pub kind: NoticeKind,
    pub title: String,
    pub body: String,
}

#[cfg(test)]
pub fn coalesce_native(notices: &[Notice], settings: &GuiSettings) -> Option<NativeNotice> {
    if !settings.native_notifications {
        return None;
    }
    let eligible: Vec<&Notice> = notices
        .iter()
        .filter(|notice| match notice.kind {
            NoticeKind::Success => settings.notify_success,
            NoticeKind::Attention => settings.notify_attention,
        })
        .collect();
    if eligible.is_empty() {
        return None;
    }
    if eligible.len() == 1 {
        let notice = eligible[0];
        return Some(NativeNotice {
            kind: notice.kind,
            title: if settings.include_run_name {
                bounded_text(&notice.title, 80)
            } else {
                "runwatch".to_string()
            },
            body: match notice.kind {
                NoticeKind::Success => "Run completed successfully".to_string(),
                NoticeKind::Attention => bounded_text(&notice.body, 160),
            },
        });
    }
    let attention = eligible
        .iter()
        .filter(|notice| notice.kind == NoticeKind::Attention)
        .count();
    let success = eligible.len() - attention;
    let body = match (attention, success) {
        (0, success) => format!("{success} runs completed"),
        (attention, 0) => format!("{attention} runs need attention"),
        (attention, success) => format!("{attention} need attention · {success} completed"),
    };
    Some(NativeNotice {
        kind: if attention > 0 {
            NoticeKind::Attention
        } else {
            NoticeKind::Success
        },
        title: "runwatch".to_string(),
        body,
    })
}

#[cfg(test)]
fn bounded_text(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    format!("{}…", value.chars().take(keep).collect::<String>())
}

#[derive(Debug, Clone, Copy)]
struct Previous {
    status: RunStatus,
    attention: bool,
}

#[derive(Default)]
pub struct TransitionTracker {
    previous: HashMap<String, Previous>,
}

impl TransitionTracker {
    pub fn observe(&mut self, rows: &[RunRow]) -> Vec<Notice> {
        let mut notices = Vec::new();
        for row in rows {
            if let Some(previous) = self.previous.get(&row.run_id) {
                if !previous.status.is_terminal() && row.status.is_terminal() {
                    notices.push(Notice {
                        kind: if row.status == RunStatus::Succeeded {
                            NoticeKind::Success
                        } else {
                            NoticeKind::Attention
                        },
                        title: row.name.clone(),
                        body: format!(
                            "{} -> {}",
                            status_label(previous.status),
                            status_label(row.status)
                        ),
                    });
                } else if !previous.attention && row.attention {
                    notices.push(Notice {
                        kind: NoticeKind::Attention,
                        title: row.name.clone(),
                        body: row
                            .attention_reason
                            .clone()
                            .unwrap_or_else(|| "Run requires attention".into()),
                    });
                }
            }
        }
        self.previous = rows
            .iter()
            .map(|row| {
                (
                    row.run_id.clone(),
                    Previous {
                        status: row.status,
                        attention: row.attention,
                    },
                )
            })
            .collect();
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use runwatch_core::RunnerKind;

    fn row(id: &str, status: RunStatus, attention: bool, reason: Option<&str>) -> RunRow {
        RunRow {
            run_id: id.into(),
            name: id.into(),
            status,
            runner: RunnerKind::Slurm,
            host: "gm00".into(),
            handle: "31842".into(),
            observation: "fresh".into(),
            continuation: "-".into(),
            updated: "1s".into(),
            workspace: "/share/work".into(),
            active: !status.is_terminal(),
            attention,
            attention_reason: reason.map(str::to_string),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn multiple_terminal_transitions_are_all_reported() {
        let mut tracker = TransitionTracker::default();
        assert!(
            tracker
                .observe(&[
                    row("a", RunStatus::Running, false, None),
                    row("b", RunStatus::Queued, false, None),
                ])
                .is_empty()
        );
        let notices = tracker.observe(&[
            row("a", RunStatus::Succeeded, false, None),
            row("b", RunStatus::Failed, true, Some("Run failed")),
        ]);
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[0].kind, NoticeKind::Success);
        assert_eq!(notices[1].kind, NoticeKind::Attention);
    }

    #[test]
    fn new_attention_is_notified_once_and_recovery_rearms_it() {
        let mut tracker = TransitionTracker::default();
        assert!(
            tracker
                .observe(&[row("r", RunStatus::Running, false, None)])
                .is_empty()
        );
        let notices = tracker.observe(&[row(
            "r",
            RunStatus::Running,
            true,
            Some("Observation is unreachable"),
        )]);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].kind, NoticeKind::Attention);
        assert_eq!(notices[0].body, "Observation is unreachable");
        assert!(
            tracker
                .observe(&[row(
                    "r",
                    RunStatus::Running,
                    true,
                    Some("Observation is unreachable"),
                )])
                .is_empty()
        );
        assert!(
            tracker
                .observe(&[row("r", RunStatus::Running, false, None)])
                .is_empty()
        );
        assert_eq!(
            tracker
                .observe(&[row(
                    "r",
                    RunStatus::Running,
                    true,
                    Some("Continuation needs explicit rebind"),
                )])
                .len(),
            1
        );
    }

    #[test]
    fn native_policy_coalesces_one_refresh_and_prioritizes_attention() {
        let settings = GuiSettings {
            native_notifications: true,
            ..GuiSettings::default()
        };
        let notices = vec![
            Notice {
                kind: NoticeKind::Success,
                title: "done-a".into(),
                body: "running -> succeeded".into(),
            },
            Notice {
                kind: NoticeKind::Success,
                title: "done-b".into(),
                body: "running -> succeeded".into(),
            },
            Notice {
                kind: NoticeKind::Attention,
                title: "failed-c".into(),
                body: "running -> failed".into(),
            },
        ];
        let native = coalesce_native(&notices, &settings).unwrap();
        assert_eq!(native.kind, NoticeKind::Attention);
        assert_eq!(native.title, "runwatch");
        assert_eq!(native.body, "1 need attention · 2 completed");
    }

    #[test]
    fn native_policy_respects_categories_and_run_name_privacy() {
        let notice = Notice {
            kind: NoticeKind::Success,
            title: "private-run-name".into(),
            body: "running -> succeeded".into(),
        };
        let mut settings = GuiSettings {
            native_notifications: true,
            ..GuiSettings::default()
        };
        settings.include_run_name = false;
        let native = coalesce_native(std::slice::from_ref(&notice), &settings).unwrap();
        assert_eq!(native.title, "runwatch");
        assert_eq!(native.body, "Run completed successfully");
        assert!(!native.body.contains("private-run-name"));

        settings.notify_success = false;
        assert!(coalesce_native(std::slice::from_ref(&notice), &settings).is_none());
        settings.native_notifications = false;
        settings.notify_success = true;
        assert!(coalesce_native(std::slice::from_ref(&notice), &settings).is_none());
    }

    #[test]
    fn initial_terminal_snapshot_does_not_replay_old_completion() {
        let mut tracker = TransitionTracker::default();
        assert!(
            tracker
                .observe(&[row("old", RunStatus::Succeeded, false, None)])
                .is_empty()
        );
    }
}
