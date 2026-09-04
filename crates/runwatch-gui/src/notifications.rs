use crate::model::{RunRow, status_label};
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
    fn initial_terminal_snapshot_does_not_replay_old_completion() {
        let mut tracker = TransitionTracker::default();
        assert!(
            tracker
                .observe(&[row("old", RunStatus::Succeeded, false, None)])
                .is_empty()
        );
    }
}
