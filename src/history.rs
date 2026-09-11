use crate::api::GroupResults;
use crate::models::*;
use std::collections::HashSet;

pub struct PushGroupResults {
    pub push: PushRef,
    pub results: GroupResults,
    /// Task ids of jobs selected by --filter/--platform; None counts every task.
    pub allowed_tasks: Option<HashSet<String>>,
}

pub fn build_group_history(manifest: &str, pushes: Vec<PushGroupResults>) -> GroupHistory {
    let mut entries: Vec<GroupHistoryPush> = pushes
        .into_iter()
        .map(|push| summarize_push(manifest, push))
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.push_id));

    let newest_fail = entries
        .iter()
        .position(|entry| entry.state == GroupState::Fail);
    let last_passed_idx = match newest_fail {
        Some(idx) => entries[idx..]
            .iter()
            .position(|entry| entry.state.counts_as_pass())
            .map(|offset| idx + offset),
        None => entries
            .iter()
            .position(|entry| entry.state.counts_as_pass()),
    };
    let first_failed = newest_fail.map(|idx| {
        let end = last_passed_idx.unwrap_or(entries.len());
        entries[idx..end]
            .iter()
            .rev()
            .find(|entry| entry.state == GroupState::Fail)
            .cloned()
            .unwrap()
    });
    let predates_window = entries
        .iter()
        .rev()
        .find(|entry| entry.state != GroupState::NotRun)
        .is_some_and(|entry| entry.state == GroupState::Fail);

    GroupHistory {
        manifest: manifest.to_string(),
        last_passed: last_passed_idx.map(|idx| entries[idx].clone()),
        first_failed,
        predates_window,
        pushes: entries,
    }
}

fn summarize_push(manifest: &str, push: PushGroupResults) -> GroupHistoryPush {
    let (mut ok, mut fail) = (0, 0);
    for (task_id, groups) in &push.results {
        if push
            .allowed_tasks
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(task_id))
        {
            continue;
        }
        match groups.get(manifest) {
            Some(true) => ok += 1,
            Some(false) => fail += 1,
            None => {}
        }
    }
    GroupHistoryPush {
        push_id: push.push.id,
        revision: push.push.revision,
        timestamp: push.push.push_timestamp,
        ok,
        fail,
        state: GroupState::from_counts(ok, fail),
    }
}

impl GroupState {
    /// A push fails when at least as many tasks fail as pass.
    pub fn from_counts(ok: usize, fail: usize) -> Self {
        match (ok, fail) {
            (0, 0) => GroupState::NotRun,
            (_, 0) => GroupState::Pass,
            (ok, fail) if fail >= ok => GroupState::Fail,
            _ => GroupState::Mixed,
        }
    }

    pub fn counts_as_pass(self) -> bool {
        matches!(self, GroupState::Pass | GroupState::Mixed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[derive(serde::Deserialize)]
    struct Fixture {
        manifest: String,
        pushes: Vec<FixturePush>,
        expected: Expected,
    }

    #[derive(serde::Deserialize)]
    struct FixturePush {
        push: PushRef,
        jobs: Vec<Job>,
        group_results: GroupResults,
    }

    #[derive(serde::Deserialize)]
    struct Expected {
        last_passed_push_id: u64,
        first_failed_push_id: u64,
        predates_window: bool,
        filtered: FilteredExpected,
    }

    #[derive(serde::Deserialize)]
    struct FilteredExpected {
        filter: String,
        last_passed_push_id: u64,
        first_failed_push_id: u64,
    }

    fn load_fixture() -> Fixture {
        serde_json::from_str(include_str!(
            "../tests/fixtures/autoland_group_history.json"
        ))
        .unwrap()
    }

    fn inputs(fixture: &Fixture, filter: Option<&Regex>) -> Vec<PushGroupResults> {
        fixture
            .pushes
            .iter()
            .map(|push| PushGroupResults {
                push: push.push.clone(),
                results: push.group_results.clone(),
                allowed_tasks: filter.map(|regex| {
                    push.jobs
                        .iter()
                        .filter(|job| regex.is_match(&job.job_type_name))
                        .filter_map(|job| job.task_id.clone())
                        .collect()
                }),
            })
            .collect()
    }

    #[test]
    fn replays_manifest_history_fixture() {
        let fixture = load_fixture();
        let history = build_group_history(&fixture.manifest, inputs(&fixture, None));

        let ids: Vec<_> = history.pushes.iter().map(|p| p.push_id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(ids, sorted, "pushes are newest first");

        assert_eq!(
            history.last_passed.as_ref().map(|p| p.push_id),
            Some(fixture.expected.last_passed_push_id)
        );
        assert_eq!(
            history.first_failed.as_ref().map(|p| p.push_id),
            Some(fixture.expected.first_failed_push_id)
        );
        assert_eq!(history.predates_window, fixture.expected.predates_window);
        assert!(history
            .pushes
            .iter()
            .any(|p| p.state == GroupState::NotRun && p.ok == 0 && p.fail == 0));
    }

    #[test]
    fn joins_tasks_to_filtered_jobs() {
        let fixture = load_fixture();
        let regex = Regex::new(&fixture.expected.filtered.filter).unwrap();
        let history = build_group_history(&fixture.manifest, inputs(&fixture, Some(&regex)));

        assert_eq!(
            history.last_passed.as_ref().map(|p| p.push_id),
            Some(fixture.expected.filtered.last_passed_push_id)
        );
        let first_failed = history.first_failed.unwrap();
        assert_eq!(
            first_failed.push_id,
            fixture.expected.filtered.first_failed_push_id
        );
        assert_eq!((first_failed.ok, first_failed.fail), (0, 1));
    }

    #[test]
    fn failure_predates_window_when_oldest_result_fails() {
        let fixture = load_fixture();
        let failing_only: Vec<_> = inputs(&fixture, None)
            .into_iter()
            .filter(|push| push.push.id >= fixture.expected.first_failed_push_id)
            .collect();
        let history = build_group_history(&fixture.manifest, failing_only);

        assert!(history.predates_window);
        assert!(history.last_passed.is_none());
        assert_eq!(
            history.first_failed.map(|p| p.push_id),
            Some(fixture.expected.first_failed_push_id)
        );
    }

    #[test]
    fn state_from_counts() {
        assert_eq!(GroupState::from_counts(0, 0), GroupState::NotRun);
        assert_eq!(GroupState::from_counts(3, 0), GroupState::Pass);
        assert_eq!(GroupState::from_counts(25, 1), GroupState::Mixed);
        assert_eq!(GroupState::from_counts(3, 3), GroupState::Fail);
        assert_eq!(GroupState::from_counts(0, 2), GroupState::Fail);
    }
}
