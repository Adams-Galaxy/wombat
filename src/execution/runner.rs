//! Validated traversal ranges shared by materialisation and deployment.

use crate::execution::ladder::{CoreRung, ExecutionLadder, RungId};
use crate::{Result, WombatError};

/// An owned execution cursor whose limits were validated against one ladder.
///
/// Owning the rung identifiers keeps callers from coupling execution to index
/// arithmetic or to the lifetime of a deserialized plan. Phase-specific code
/// still dispatches its own core operations while sharing traversal semantics.
pub(crate) struct ExecutionRange {
    rungs: std::vec::IntoIter<RungId>,
}

impl ExecutionRange {
    /// Traverse from the beginning of the ladder through `end`, inclusively.
    pub(crate) fn through(ladder: &ExecutionLadder, end: CoreRung) -> Result<Self> {
        Self::bounded(ladder, None, end, true)
    }

    /// Traverse from `start` through `end`, with independently controlled ends.
    pub(crate) fn between(
        ladder: &ExecutionLadder,
        start: CoreRung,
        end: CoreRung,
        include_start: bool,
        include_end: bool,
    ) -> Result<Self> {
        Self::bounded(ladder, Some((start, include_start)), end, include_end)
    }

    fn bounded(
        ladder: &ExecutionLadder,
        start: Option<(CoreRung, bool)>,
        end: CoreRung,
        include_end: bool,
    ) -> Result<Self> {
        let mut leaves = ladder.leaf_ids().cloned().collect::<Vec<_>>();
        let end_id = RungId::from(end);
        let end_index = leaves
            .iter()
            .position(|rung| rung == &end_id)
            .ok_or_else(|| {
                WombatError::invariant(format!(
                    "validated ladder `{}` has no `{end}` boundary",
                    ladder.name,
                    end = end.id()
                ))
            })?;
        let (start_index, include_start) = match start {
            Some((start, include)) => {
                let start_id = RungId::from(start);
                let index = leaves
                    .iter()
                    .position(|rung| rung == &start_id)
                    .ok_or_else(|| {
                        WombatError::invariant(format!(
                            "validated ladder `{}` has no `{start}` boundary",
                            ladder.name,
                            start = start.id()
                        ))
                    })?;
                (index, include)
            }
            None => (0, true),
        };
        if start_index > end_index {
            return Err(WombatError::invariant(format!(
                "execution range `{}` through `{}` is reversed",
                leaves[start_index], end_id
            )));
        }

        let first = start_index + usize::from(!include_start);
        let past_last = end_index + usize::from(include_end);
        leaves.truncate(past_last);
        leaves.drain(..first);
        Ok(Self {
            rungs: leaves.into_iter(),
        })
    }
}

impl Iterator for ExecutionRange {
    type Item = RungId;

    fn next(&mut self) -> Option<Self::Item> {
        self.rungs.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_respect_inclusive_phase_boundaries() {
        let ladder = ExecutionLadder::default();
        let materialise = ExecutionRange::through(&ladder, CoreRung::MaterialiseAfter)
            .expect("fixed ladder is valid")
            .collect::<Vec<_>>();
        assert_eq!(materialise.len(), 5);
        assert_eq!(
            materialise.last().and_then(RungId::core),
            Some(CoreRung::MaterialiseAfter)
        );

        let before_apply = ExecutionRange::between(
            &ladder,
            CoreRung::DeployBefore,
            CoreRung::DeployApply,
            true,
            false,
        )
        .expect("fixed ladder is valid")
        .collect::<Vec<_>>();
        assert_eq!(before_apply, vec![RungId::from(CoreRung::DeployBefore)]);
    }
}
