//! The "Why?" explanation (spec §6.5): one plain-language paragraph on what is using memory and
//! what to do about it, built from the snapshot, its pressure reason and the classification.

use super::SafetyClass;
use super::classify::ClassifiedGroup;
use super::plan::Suggestion;
use crate::core::{PressureLevel, PressureSignal, Snapshot, format_bytes};

/// Compressed memory worth mentioning on its own.
const NOTABLE_COMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;

fn join_names(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Builds the paragraph. `suggestions` is the "what to close" list (safe apps first).
pub fn explain(
    snapshot: &Snapshot,
    groups: &[ClassifiedGroup],
    suggestions: &[Suggestion],
) -> String {
    let p = &snapshot.pressure;
    let reason = &p.main_reason.message;
    let mut sentences = vec![match p.level {
        PressureLevel::Calm => format!("Memory is calm: {reason}."),
        PressureLevel::Strained => format!("Memory is under pressure because {reason}."),
        PressureLevel::Critical => format!("Memory is critical because {reason}."),
    }];

    let top: Vec<&ClassifiedGroup> = groups
        .iter()
        .filter(|g| g.group.memory_bytes > 0)
        .take(3)
        .collect();
    if !top.is_empty() {
        let listed: Vec<String> = top
            .iter()
            .map(|g| {
                let system = if g.class == SafetyClass::Never {
                    ", not closable"
                } else {
                    ""
                };
                format!(
                    "{} ({}{system})",
                    g.group.name,
                    format_bytes(g.group.memory_bytes)
                )
            })
            .collect();
        sentences.push(format!(
            "The biggest memory users are {} (measured).",
            join_names(&listed)
        ));
    }

    if p.main_reason.signal != PressureSignal::Compression
        && let Some(compressed) = snapshot
            .memory
            .compressed_original_bytes
            .filter(|&b| b >= NOTABLE_COMPRESSED_BYTES)
    {
        sentences.push(format!(
            "The system is also compressing {} of app memory to make room.",
            format_bytes(compressed)
        ));
    }
    if p.main_reason.signal != PressureSignal::SwapGrowing
        && snapshot.swap_trend.as_ref().is_some_and(|t| t.growing)
    {
        sentences.push("Swap is growing, which slows everything down.".to_string());
    }

    match suggestions.first() {
        Some(s) => {
            let caveat = match s.class {
                SafetyClass::Safe => String::new(),
                _ => format!(", but {}", s.reason),
            };
            sentences.push(format!(
                "Closing {} would free about {} (estimated){caveat}.",
                s.name,
                format_bytes(s.memory_bytes)
            ));
        }
        None => {
            let biggest = top.iter().find(|g| g.class == SafetyClass::Never);
            sentences.push(match biggest {
                Some(g) => format!(
                    "canfit has nothing it can suggest closing: the biggest users, such as {}, \
                     are system processes, protected, or hosting canfit itself.",
                    g.group.name
                ),
                None => "canfit has nothing worth suggesting closing.".to_string(),
            });
        }
    }
    sentences.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::classify::tests::{
        CANFIT_PID, GB, ME, classify_all, desktop, proc, snapshot_with,
    };
    use crate::advisor::plan::suggestions;
    use crate::core::sampler::tests::replay;

    #[test]
    fn paragraph_names_the_cause_the_biggest_users_and_what_to_do() {
        let (_, base) = replay("critical");
        let mut s = snapshot_with(desktop());
        s.pressure = base.pressure.clone();
        s.memory = base.memory.clone();
        let groups = classify_all(&s, &[]);
        let list = suggestions(&groups);
        let text = explain(&s, &groups, &list);
        assert!(text.starts_with("Memory is critical because "), "{text}");
        assert!(
            text.contains("biggest memory users are Ollama (4.0 GB)"),
            "{text}"
        );
        assert!(
            text.contains("Closing Google Chrome would free about 2.9 GB (estimated)."),
            "{text}"
        );
    }

    #[test]
    fn system_only_memory_says_nothing_can_be_suggested() {
        let processes = vec![
            proc(1, 0, "/sbin/launchd", "root", GB),
            proc(
                100,
                1,
                "/System/Library/WindowServer",
                "_windowserver",
                8 * GB,
            ),
            proc(CANFIT_PID, 1, "/usr/local/bin/canfit", ME, GB / 100),
        ];
        let s = snapshot_with(processes);
        let groups = classify_all(&s, &[]);
        let text = explain(&s, &groups, &suggestions(&groups));
        assert!(text.starts_with("Memory is calm: "), "{text}");
        assert!(
            text.contains("WindowServer (8.0 GB, not closable)"),
            "{text}"
        );
        assert!(
            text.contains("such as WindowServer, are system processes"),
            "{text}"
        );
    }

    #[test]
    fn names_are_joined_in_english() {
        let names = |v: &[&str]| join_names(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(names(&["a"]), "a");
        assert_eq!(names(&["a", "b"]), "a and b");
        assert_eq!(names(&["a", "b", "c"]), "a, b and c");
    }
}
