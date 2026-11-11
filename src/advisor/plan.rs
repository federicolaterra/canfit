//! The "what to close" list and the make-room selection (spec §6.4). Pure functions.

use serde::Serialize;

use super::SafetyClass;
use super::classify::ClassifiedGroup;
use crate::core::Provenance;

/// Apps using less than this are not worth suggesting: closing them frees too little.
pub const MIN_SUGGESTION_BYTES: u64 = 64 * 1024 * 1024;

/// An app canfit may suggest closing. Never-class apps never become suggestions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    pub name: String,
    pub class: SafetyClass,
    pub reason: String,
    /// The group's memory, measured (physical footprint on macOS). Closing the app frees about
    /// this much; the freed amount is an estimate.
    pub memory_bytes: u64,
    pub memory_provenance: Provenance,
    pub process_count: usize,
    /// Processes whose memory could not be read (not in `memory_bytes`).
    pub unreadable_count: usize,
    pub pids: Vec<u32>,
    /// How to close it by hand, safest first (ADR-022). canfit never runs these.
    pub manual_commands: Vec<super::manual::ManualCommand>,
}

/// The "what to close" list: every safe or caution app above the size floor, safe apps first,
/// then by memory, largest first.
pub fn suggestions(groups: &[ClassifiedGroup]) -> Vec<Suggestion> {
    let mut list: Vec<Suggestion> = groups
        .iter()
        .filter(|g| g.class != SafetyClass::Never && g.group.memory_bytes >= MIN_SUGGESTION_BYTES)
        .map(|g| Suggestion {
            name: g.group.name.clone(),
            class: g.class,
            reason: g.reason.clone(),
            memory_bytes: g.group.memory_bytes,
            memory_provenance: Provenance::Measured,
            process_count: g.group.process_count,
            unreadable_count: g.group.unreadable_count,
            pids: g.group.pids.clone(),
            manual_commands: Vec::new(),
        })
        .collect();
    list.sort_by(|a, b| {
        a.class
            .cmp(&b.class)
            .then(b.memory_bytes.cmp(&a.memory_bytes))
            .then_with(|| a.name.cmp(&b.name))
    });
    list
}

/// The smallest set of apps to close to free `needed_bytes`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MakeRoomPlan {
    pub needed_bytes: u64,
    /// Safe apps first, then caution, each largest first.
    pub chosen: Vec<Suggestion>,
    /// Sum of the chosen apps' memory: what closing them should free (estimated).
    pub freed_bytes: u64,
    /// False when even closing every suggestable app would not free enough; `chosen` then holds
    /// all of them.
    pub enough: bool,
}

/// Indexes into `items` (memory in bytes) of a minimum-count subset whose sum reaches `need`, or
/// `None` when the whole list is not enough. Among minimum-count subsets it avoids closing a
/// much bigger app than needed: the last pick is the smallest item that still covers the rest.
fn minimal_cover(items: &[u64], need: u64) -> Option<Vec<usize>> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| items[b].cmp(&items[a]).then(a.cmp(&b)));
    let mut chosen = Vec::new();
    let mut remaining = need;
    let mut pool = order;
    while remaining > 0 {
        // One item covers the rest: take the smallest such item.
        if let Some(pos) = pool.iter().rposition(|&i| items[i] >= remaining) {
            chosen.push(pool[pos]);
            return Some(chosen);
        }
        // Otherwise take the largest; the minimum count needs it.
        if pool.is_empty() {
            return None;
        }
        let i = pool.remove(0);
        if items[i] == 0 {
            return None;
        }
        remaining -= items[i];
        chosen.push(i);
    }
    Some(chosen)
}

/// Chooses what to close to free `needed_bytes`: as few caution apps as possible (none when the
/// safe ones are enough), then as few safe apps as possible.
pub fn make_room(candidates: &[Suggestion], needed_bytes: u64) -> MakeRoomPlan {
    let safe: Vec<&Suggestion> = candidates
        .iter()
        .filter(|s| s.class == SafetyClass::Safe)
        .collect();
    let caution: Vec<&Suggestion> = candidates
        .iter()
        .filter(|s| s.class == SafetyClass::Caution)
        .collect();
    let sizes = |list: &[&Suggestion]| list.iter().map(|s| s.memory_bytes).collect::<Vec<_>>();
    let pick = |list: &[&Suggestion], idx: Vec<usize>| -> Vec<Suggestion> {
        let mut out: Vec<Suggestion> = idx.into_iter().map(|i| list[i].clone()).collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.memory_bytes));
        out
    };
    let safe_total: u64 = safe.iter().map(|s| s.memory_bytes).sum();
    let caution_total: u64 = caution.iter().map(|s| s.memory_bytes).sum();

    let (chosen, enough) = if needed_bytes == 0 {
        (Vec::new(), true)
    } else if safe_total >= needed_bytes {
        let idx = minimal_cover(&sizes(&safe), needed_bytes).expect("safe apps cover the need");
        (pick(&safe, idx), true)
    } else if safe_total + caution_total >= needed_bytes {
        let idx = minimal_cover(&sizes(&caution), needed_bytes - safe_total)
            .expect("caution apps cover the rest");
        let caution_chosen = pick(&caution, idx);
        let caution_freed: u64 = caution_chosen.iter().map(|s| s.memory_bytes).sum();
        let safe_need = needed_bytes.saturating_sub(caution_freed);
        let idx = minimal_cover(&sizes(&safe), safe_need).expect("safe apps cover the rest");
        let mut chosen = pick(&safe, idx);
        chosen.extend(caution_chosen);
        (chosen, true)
    } else {
        let mut chosen: Vec<Suggestion> = safe.into_iter().cloned().collect();
        chosen.extend(caution.into_iter().cloned());
        (chosen, false)
    };
    let freed_bytes = chosen.iter().map(|s| s.memory_bytes).sum();
    MakeRoomPlan {
        needed_bytes,
        chosen,
        freed_bytes,
        enough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::classify::tests::{
        CANFIT_PID, GB, MB, ME, classify_all, desktop, proc, snapshot_with,
    };

    fn names(plan: &MakeRoomPlan) -> Vec<&str> {
        plan.chosen.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn minimal_cover_uses_fewest_items_and_smallest_last_pick() {
        assert_eq!(minimal_cover(&[5, 3, 9, 1], 0), Some(vec![]));
        // One item is enough: the smallest that covers it (5, not 9).
        assert_eq!(minimal_cover(&[5, 3, 9, 1], 4), Some(vec![0]));
        // Needs two: the largest, then the smallest covering the rest.
        assert_eq!(minimal_cover(&[5, 3, 9, 1], 11), Some(vec![2, 1]));
        assert_eq!(minimal_cover(&[5, 3, 9, 1], 18), Some(vec![2, 0, 1, 3]));
        assert_eq!(minimal_cover(&[5, 3, 9, 1], 19), None);
        assert_eq!(minimal_cover(&[], 1), None);
        assert_eq!(minimal_cover(&[0, 0], 1), None);
    }

    #[test]
    fn what_to_close_lists_safe_first_without_never_or_tiny_apps() {
        let list = suggestions(&classify_all(&snapshot_with(desktop()), &[]));
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Google Chrome",
                "Slack",
                "Spotify",
                "Ollama",
                "Visual Studio Code",
                "Some Tool"
            ]
        );
        assert_eq!(list[0].memory_bytes, 3000 * MB);
        assert_eq!(list[0].pids, [300, 301]);
        assert!(list.iter().all(|s| s.class != SafetyClass::Never));
    }

    #[test]
    fn safe_apps_are_enough_when_they_can_be() {
        let list = suggestions(&classify_all(&snapshot_with(desktop()), &[]));
        // 900 MB: Slack (1000 MB) is the smallest single safe app that covers it.
        let plan = make_room(&list, 900 * MB);
        assert!(plan.enough);
        assert_eq!(names(&plan), ["Slack"]);
        // 3200 MB: Chrome (3000 MB) plus the smallest safe app covering the rest (Spotify).
        let plan = make_room(&list, 3200 * MB);
        assert_eq!(names(&plan), ["Google Chrome", "Spotify"]);
        assert_eq!(plan.freed_bytes, 3300 * MB);
        assert_eq!(make_room(&list, 0).chosen, []);
    }

    #[test]
    fn caution_apps_are_added_only_when_safe_ones_are_not_enough() {
        let list = suggestions(&classify_all(&snapshot_with(desktop()), &[]));
        // Safe total is 4300 MB; 6 GB needs one caution app (Ollama, 4 GB) and then only as many
        // safe apps as the rest requires.
        let plan = make_room(&list, 6 * GB);
        assert!(plan.enough);
        assert_eq!(names(&plan), ["Google Chrome", "Ollama"]);
        assert!(plan.freed_bytes >= 6 * GB);
    }

    #[test]
    fn canfits_own_parent_being_the_biggest_process_is_never_chosen() {
        // canfit runs inside a huge app (an IDE terminal): that app must not be suggested.
        let mut processes = desktop();
        processes.push(proc(
            950,
            1,
            "/Applications/Cursor.app/Contents/MacOS/Cursor",
            ME,
            20 * GB,
        ));
        processes.iter_mut().find(|p| p.pid == 202).unwrap().ppid = Some(950);
        let groups = classify_all(&snapshot_with(processes), &[]);
        let list = suggestions(&groups);
        assert!(list.iter().all(|s| s.name != "Cursor"));
        let plan = make_room(&list, 15 * GB);
        assert!(!plan.enough);
        assert!(plan.chosen.iter().all(|s| s.name != "Cursor"));
        assert!(plan.chosen.iter().all(|s| !s.pids.contains(&CANFIT_PID)));
    }

    #[test]
    fn only_never_class_processes_being_large_gives_no_plan() {
        let processes = vec![
            proc(1, 0, "/sbin/launchd", "root", 30 * MB),
            proc(
                100,
                1,
                "/System/Library/WindowServer",
                "_windowserver",
                8 * GB,
            ),
            proc(
                101,
                1,
                "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder",
                ME,
                3 * GB,
            ),
            proc(
                200,
                1,
                "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
                ME,
                2 * GB,
            ),
            proc(202, 200, "/bin/zsh", ME, 5 * MB),
            proc(CANFIT_PID, 202, "/usr/local/bin/canfit", ME, 10 * MB),
            proc(
                300,
                1,
                "/Applications/Notes Helper.app/Contents/MacOS/x",
                ME,
                10 * MB,
            ),
        ];
        let groups = classify_all(&snapshot_with(processes), &[]);
        let list = suggestions(&groups);
        assert!(list.is_empty(), "{list:?}");
        let plan = make_room(&list, GB);
        assert!(!plan.enough);
        assert!(plan.chosen.is_empty());
        assert_eq!(plan.freed_bytes, 0);
    }

    #[test]
    fn never_class_never_appears_in_any_plan() {
        // Deterministic pseudo-random process tables with every kind of owner and path.
        let exes = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Slack.app/Contents/MacOS/Slack",
            "/Applications/Xcode.app/Contents/MacOS/Xcode",
            "/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock",
            "/usr/libexec/trustd",
            "/opt/homebrew/bin/llama-server",
            "/Applications/Other.app/Contents/MacOS/Other",
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
            "/bin/zsh",
        ];
        let users = [ME, ME, ME, "root", "_windowserver"];
        let mut seed: u64 = 0x5eed;
        let mut next = |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for _ in 0..200 {
            let mut processes = vec![
                proc(1, 0, "/sbin/launchd", "root", MB),
                proc(CANFIT_PID, 1, "/usr/local/bin/canfit", ME, 10 * MB),
            ];
            for pid in 2..(2 + next(12) as u32) {
                let parent = if next(4) == 0 { CANFIT_PID } else { 1 };
                processes.push(proc(
                    pid,
                    parent,
                    exes[next(exes.len() as u64) as usize],
                    users[next(users.len() as u64) as usize],
                    next(4 * GB) + 1,
                ));
            }
            // Sometimes canfit runs inside one of the apps.
            if next(2) == 0 {
                let host = 2 + next(3) as u32;
                if processes.iter().any(|p| p.pid == host) {
                    processes[1].ppid = Some(host);
                }
            }
            let groups = classify_all(&snapshot_with(processes), &[]);
            let never: Vec<&str> = groups
                .iter()
                .filter(|g| g.class == SafetyClass::Never)
                .map(|g| g.group.name.as_str())
                .collect();
            let list = suggestions(&groups);
            for need in [0, GB, 3 * GB, 100 * GB] {
                let plan = make_room(&list, need);
                for s in &plan.chosen {
                    assert!(!never.contains(&s.name.as_str()), "{} chosen", s.name);
                    assert_ne!(s.class, SafetyClass::Never);
                    assert!(!s.pids.contains(&CANFIT_PID));
                }
                if plan.enough {
                    assert!(plan.freed_bytes >= need);
                }
            }
        }
    }
}
