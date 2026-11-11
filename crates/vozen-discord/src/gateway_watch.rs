//! Pure gateway watchdog decision shared by the runtime's full cutover path.
//!
//! Shadow mode deliberately does not run the timer because it must never restart or interfere
//! with the still-authoritative Node process. Full mode uses the same conservative contract as
//! Node: log sustained non-Ready state and ask the supervisor for a fresh process after 120s.
//! A long scheduler pause invalidates cached readiness and requires a fresh connection.

/// Default watchdog cadence. Kept equal to the Node gateway watcher.
pub const CHECK_INTERVAL_MS: i64 = 60_000;
/// Maximum sustained non-Ready period before the process should be restarted.
pub const MAX_DOWN_MS: i64 = 120_000;
/// A delayed supervisor tick can outlive the Discord session while shard stages stay cached.
pub const MAX_TICK_LAG_MS: i64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatewayDecision {
    pub healthy: bool,
    pub unhealthy_since_ms: Option<i64>,
    pub down_ms: i64,
    pub should_restart: bool,
}

/// Pure watchdog state transition. `status_ready` is the aggregate gateway state, while
/// `unhealthy_since_ms` is fed back from the previous tick. A clock moving backwards cannot
/// produce a negative outage duration or trigger an early restart. Scheduler lag is measured
/// separately with a monotonic clock because cached Ready can survive an expired session.
pub fn evaluate_gateway(
    status_ready: bool,
    unhealthy_since_ms: Option<i64>,
    now_ms: i64,
    max_down_ms: i64,
    tick_lag_ms: i64,
) -> GatewayDecision {
    let scheduler_stalled = tick_lag_ms > MAX_TICK_LAG_MS;
    if status_ready && !scheduler_stalled {
        return GatewayDecision {
            healthy: true,
            unhealthy_since_ms: None,
            down_ms: 0,
            should_restart: false,
        };
    }
    let since = unhealthy_since_ms.unwrap_or(now_ms);
    let down_ms = now_ms.saturating_sub(since).max(0);
    GatewayDecision {
        healthy: false,
        unhealthy_since_ms: Some(since),
        down_ms,
        should_restart: scheduler_stalled || down_ms > max_down_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_tick_resets_an_existing_outage() {
        assert_eq!(
            evaluate_gateway(true, Some(10), 100, MAX_DOWN_MS, 0),
            GatewayDecision {
                healthy: true,
                unhealthy_since_ms: None,
                down_ms: 0,
                should_restart: false,
            }
        );
    }

    #[test]
    fn outage_starts_now_and_restarts_only_after_the_strict_limit() {
        let first = evaluate_gateway(false, None, 1_000, MAX_DOWN_MS, 0);
        assert_eq!(first.unhealthy_since_ms, Some(1_000));
        assert_eq!(first.down_ms, 0);
        assert!(!first.should_restart);

        let at_limit = evaluate_gateway(false, first.unhealthy_since_ms, 121_000, MAX_DOWN_MS, 0);
        assert_eq!(at_limit.down_ms, MAX_DOWN_MS);
        assert!(!at_limit.should_restart);

        let after_limit =
            evaluate_gateway(false, at_limit.unhealthy_since_ms, 121_001, MAX_DOWN_MS, 0);
        assert!(after_limit.should_restart);
    }

    #[test]
    fn clock_rollback_cannot_create_negative_down_time() {
        let decision = evaluate_gateway(false, Some(10_000), 9_000, MAX_DOWN_MS, 0);
        assert_eq!(decision.down_ms, 0);
        assert!(!decision.should_restart);
    }

    #[test]
    fn scheduler_stall_restarts_even_when_the_shard_still_reports_ready() {
        // The observed 99s pause delays a 60s supervisor tick by at least 39s.
        for lag_ms in [39_283, 99_283] {
            let decision = evaluate_gateway(true, None, 200_000, MAX_DOWN_MS, lag_ms);
            assert!(!decision.healthy);
            assert!(decision.should_restart);
        }
    }

    #[test]
    fn brief_scheduler_delays_keep_a_healthy_connection() {
        for lag_ms in [0, 400, 1_827, MAX_TICK_LAG_MS] {
            let decision = evaluate_gateway(true, None, 200_000, MAX_DOWN_MS, lag_ms);
            assert!(decision.healthy);
            assert!(!decision.should_restart);
        }
        assert!(
            evaluate_gateway(true, None, 200_000, MAX_DOWN_MS, MAX_TICK_LAG_MS + 1).should_restart
        );
    }
}
