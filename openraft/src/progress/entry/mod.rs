use std::borrow::Borrow;
use std::error::Error;
use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Formatter;

use validit::Validate;

use crate::display_ext::DisplayOptionExt;
use crate::progress::inflight::Inflight;
use crate::raft_state::LogStateReader;
use crate::LogId;
use crate::LogIdOptionExt;
use crate::RaftTypeConfig;

/// State of replication to a target node.
#[derive(Clone, Copy, Debug)]
#[derive(PartialEq, Eq)]
pub(crate) struct ProgressEntry<C>
where C: RaftTypeConfig
{
    /// The id of the last matching log on the target following node.
    pub(crate) matching: Option<LogId<C::NodeId>>,

    /// The data being transmitted in flight.
    ///
    /// A non-none inflight expects a response when the data was successfully sent or failed.
    pub(crate) inflight: Inflight<C>,

    /// One plus the max log index on the following node that might match the leader log.
    pub(crate) searching_end: u64,
}

impl<C> ProgressEntry<C>
where C: RaftTypeConfig
{
    #[allow(dead_code)]
    pub(crate) fn new(matching: Option<LogId<C::NodeId>>) -> Self {
        Self {
            matching,
            inflight: Inflight::None,
            searching_end: matching.next_index(),
        }
    }

    /// Create a progress entry that does not have any matching log id.
    ///
    /// It's going to initiate a binary search to find the minimal matching log id.
    pub(crate) fn empty(end: u64) -> Self {
        Self {
            matching: None,
            inflight: Inflight::None,
            searching_end: end,
        }
    }

    // This method is only used by tests.
    #[allow(dead_code)]
    pub(crate) fn with_inflight(mut self, inflight: Inflight<C>) -> Self {
        debug_assert_eq!(self.inflight, Inflight::None);

        self.inflight = inflight;
        self
    }

    /// Return if a range of log id `..=log_id` is inflight sending.
    ///
    /// `prev_log_id` is never inflight.
    pub(crate) fn is_log_range_inflight(&self, upto: &LogId<C::NodeId>) -> bool {
        match &self.inflight {
            Inflight::None => false,
            Inflight::Logs { log_id_range, .. } => {
                let lid = Some(*upto);
                lid > log_id_range.prev
            }
            Inflight::Snapshot { last_log_id: _, .. } => false,
        }
    }

    pub(crate) fn update_matching(&mut self, matching: Option<LogId<C::NodeId>>) {
        tracing::debug!(
            self = display(&self),
            matching = display(matching.display()),
            "update_matching"
        );

        self.inflight.ack(matching);

        debug_assert!(matching >= self.matching);
        self.matching = matching;

        let matching_next = self.matching.next_index();
        self.searching_end = std::cmp::max(self.searching_end, matching_next);
    }

    /// Update conflicting log index.
    ///
    /// Conflicting log index is the last found log index on a follower that is not matching the
    /// leader log.
    ///
    /// Usually if follower's data is lost, `conflict` is always greater than or equal `matching`.
    /// But for testing purpose, a follower is allowed to clean its data and wait for leader to
    /// replicate all data to it.
    ///
    /// To allow a follower to clean its data, enable feature flag [`loosen-follower-log-revert`] .
    ///
    /// [`loosen-follower-log-revert`]: crate::docs::feature_flags#feature_flag_loosen_follower_log_revert
    pub(crate) fn update_conflicting(&mut self, conflict: u64) {
        tracing::debug!(self = debug(&self), conflict = display(conflict), "update_conflict");

        self.inflight.conflict(conflict);

        debug_assert!(conflict < self.searching_end);
        self.searching_end = conflict;

        // An already matching log id is found lost:
        //
        // - If log reversion is allowed, just restart the binary search from the beginning.
        // - Otherwise, panic it.
        {
            #[cfg(feature = "loosen-follower-log-revert")]
            if conflict < self.matching.next_index() {
                tracing::warn!(
                    "conflict {} < last matching {}: follower log is reverted; with 'loosen-follower-log-revert' enabled, this is allowed.",
                    conflict,
                    self.matching.display(),
                );

                self.matching = None;
            }

            debug_assert!(
                conflict >= self.matching.next_index(),
                "follower log reversion is not allowed \
                without `--features loosen-follower-log-revert`; \
                matching: {}; conflict: {}",
                self.matching.display(),
                conflict
            );
        }
    }

    /// Initialize a replication action: sending log entries or sending snapshot.
    ///
    /// If there is an action in progress, i.e., `inflight` is not None, it returns an `Err`
    /// containing the current `inflight` data.
    ///
    /// See: [Algorithm to find the last matching log id on a Follower][algo].
    ///
    /// [algo]: crate::docs::protocol::replication::log_replication#algorithm-to-find-the-last-matching-log-id-on-a-follower
    pub(crate) fn next_send(
        &mut self,
        log_state: &impl LogStateReader<C>,
        max_entries: u64,
    ) -> Result<&Inflight<C>, &Inflight<C>> {
        if !self.inflight.is_none() {
            return Err(&self.inflight);
        }

        let last_next = log_state.last_log_id().next_index();
        debug_assert!(
            self.searching_end <= last_next,
            "expect: searching_end: {} <= last_log_id.next_index: {}",
            self.searching_end,
            last_next
        );

        let purge_upto_next = {
            let purge_upto = log_state.purge_upto();
            purge_upto.next_index()
        };

        // `searching_end` is the max value for `start`.

        // The log the follower needs is purged.
        // Replicate by snapshot.
        if self.searching_end < purge_upto_next {
            let snapshot_last = log_state.snapshot_last_log_id();
            self.inflight = Inflight::snapshot(snapshot_last.copied());
            return Ok(&self.inflight);
        }

        // Replicate by logs.
        // Run a binary search to find the matching log id, if matching log id is not determined.
        let mut start = Self::calc_mid(self.matching.next_index(), self.searching_end);
        if start < purge_upto_next {
            start = purge_upto_next;
        }

        let end = std::cmp::min(start + max_entries, last_next);

        if start == end {
            self.inflight = Inflight::None;
            return Err(&self.inflight);
        }

        let prev = log_state.prev_log_id(start);
        let last = log_state.prev_log_id(end);

        self.inflight = Inflight::logs(prev, last);

        Ok(&self.inflight)
    }

    /// Return the index range(`[start,end]`) of the first log in the next AppendEntries.
    ///
    /// The returned range is left close and right close.
    #[allow(dead_code)]
    pub(crate) fn sending_start(&self) -> (u64, u64) {
        let mid = Self::calc_mid(self.matching.next_index(), self.searching_end);
        (mid, self.searching_end)
    }

    fn calc_mid(matching_next: u64, end: u64) -> u64 {
        debug_assert!(matching_next <= end);
        let d = end - matching_next;
        let offset = d / 16 * 8;
        matching_next + offset
    }
}

impl<C> Borrow<Option<LogId<C::NodeId>>> for ProgressEntry<C>
where C: RaftTypeConfig
{
    fn borrow(&self) -> &Option<LogId<C::NodeId>> {
        &self.matching
    }
}

impl<C> Display for ProgressEntry<C>
where C: RaftTypeConfig
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{[{}, {}), inflight:{}}}",
            self.matching.display(),
            self.searching_end,
            self.inflight
        )
    }
}

impl<C> Validate for ProgressEntry<C>
where C: RaftTypeConfig
{
    fn validate(&self) -> Result<(), Box<dyn Error>> {
        validit::less_equal!(self.matching.next_index(), self.searching_end);

        self.inflight.validate()?;

        match self.inflight {
            Inflight::None => {}
            Inflight::Logs { log_id_range, .. } => {
                // matching <= prev_log_id              <= last_log_id
                //             prev_log_id.next_index() <= searching_end
                validit::less_equal!(self.matching, log_id_range.prev);
                validit::less_equal!(log_id_range.prev.next_index(), self.searching_end);
            }
            Inflight::Snapshot { last_log_id, .. } => {
                // There is no need to send a snapshot smaller than last matching.
                validit::less!(self.matching, last_log_id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
