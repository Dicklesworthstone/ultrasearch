use ipc::{MetricsSnapshot, StatusResponse, VolumeStatus};
use std::env;

/// Build a StatusResponse from provided fragments.
///
/// This keeps server wiring centralized and ensures new fields are populated consistently.
/// `last_index_commit_ts` is reported exactly as recorded: a service that has not
/// committed an index batch yet says so instead of claiming a commit at request time.
#[allow(clippy::too_many_arguments)]
pub fn make_status_response(
    id: uuid::Uuid,
    volumes: Vec<VolumeStatus>,
    scheduler_state: String,
    metrics: Option<MetricsSnapshot>,
    last_index_commit_ts: Option<i64>,
    content_jobs_total: Option<u64>,
    content_jobs_remaining: Option<u64>,
    content_bytes_total: Option<u64>,
    content_bytes_remaining: Option<u64>,
) -> StatusResponse {
    StatusResponse {
        id,
        volumes,
        scheduler_state,
        last_index_commit_ts,
        content_jobs_total,
        content_jobs_remaining,
        content_bytes_total,
        content_bytes_remaining,
        metrics,
        served_by: Some(host_label()),
    }
}

fn host_label() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "service".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn response_with_commit(last_index_commit_ts: Option<i64>) -> StatusResponse {
        make_status_response(
            Uuid::nil(),
            vec![],
            "idle".into(),
            None,
            last_index_commit_ts,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn populates_defaults() {
        assert!(response_with_commit(None).served_by.is_some());
    }

    #[test]
    fn reports_the_recorded_commit_time_and_never_invents_one() {
        assert_eq!(
            response_with_commit(None).last_index_commit_ts,
            None,
            "no commit recorded must not be reported as a commit at request time"
        );
        assert_eq!(
            response_with_commit(Some(1_700_000_000)).last_index_commit_ts,
            Some(1_700_000_000)
        );
    }
}
