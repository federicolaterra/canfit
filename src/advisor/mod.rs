//! Process advisor (spec §6.4, §6.5, §10; ADR-013): safe · caution · never classification, the
//! "what to close" list, the make-room selection, the "Why?" explanation and confirmed
//! graceful-then-forced closing.
//!
//! Everything except [`close`] is a pure function of a snapshot, so the live TUI (milestone 05)
//! can reuse it unchanged.

pub mod classify;
pub mod close;
pub mod config;
pub mod manual;
pub mod plan;
pub mod rules;
pub mod why;

use serde::{Deserialize, Serialize};

use crate::core::Snapshot;

/// How safe it is to close an app. Ordered from least to most restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyClass {
    /// Suggested first: apps that restore their state (browsers, chat apps).
    Safe,
    /// Suggested with a warning: you may lose unsaved work.
    Caution,
    /// Never suggested and never closed.
    Never,
}

impl SafetyClass {
    pub fn label(self) -> &'static str {
        match self {
            SafetyClass::Safe => "safe",
            SafetyClass::Caution => "caution",
            SafetyClass::Never => "never",
        }
    }

    /// Colour-blind-safe symbol (spec §11).
    pub fn symbol(self) -> &'static str {
        match self {
            SafetyClass::Safe => "●",
            SafetyClass::Caution => "▲",
            SafetyClass::Never => "✗",
        }
    }
}

/// Everything the advisor concludes from one snapshot.
#[derive(Debug, Clone)]
pub struct Advice {
    pub groups: Vec<classify::ClassifiedGroup>,
    /// The "what to close" list, safe apps first.
    pub suggestions: Vec<plan::Suggestion>,
    /// The "Why?" paragraph.
    pub why: String,
}

/// Classifies the snapshot's apps for the canfit process `own_pid` with the user's protected
/// list, and builds the suggestions and the explanation.
pub fn advise(snapshot: &Snapshot, own_pid: u32, protected: Vec<String>) -> Advice {
    let ctx = classify::Context::from_snapshot(snapshot, own_pid, protected);
    let rules = rules::for_os(&snapshot.platform.os);
    let groups = classify::classify(snapshot, &ctx, rules.as_ref());
    let mut suggestions = plan::suggestions(&groups);
    for s in &mut suggestions {
        if let Some(g) = groups.iter().find(|g| g.group.name == s.name) {
            s.manual_commands = manual::commands(
                &g.group,
                g.class,
                &snapshot.processes,
                &snapshot.platform.os,
            );
        }
    }
    let why = why::explain(snapshot, &groups, &suggestions);
    Advice {
        groups,
        suggestions,
        why,
    }
}
