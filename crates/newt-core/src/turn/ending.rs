use super::{Compaction, TurnSettings};

const END_TURN_REMINDER: &str =
    "Every turn ends with a call to end_turn. Call it now if you are done, or keep working.";
const CHECKPOINT_REMINDER: &str = "Compaction ends with a call to checkpoint. Call it now.";
const EMPTY_RESPONSE_REMINDER: &str =
    "Your last response had no text and no tool calls. Reply, or call a tool.";

#[derive(Debug, Clone, Copy)]
pub(crate) enum Ending<'a> {
    Reply,
    EndTurn,
    Checkpoint(Compaction<'a>),
}

impl<'a> Ending<'a> {
    pub(crate) const fn of(settings: &TurnSettings<'a>) -> Self {
        match settings.compaction {
            Some(compaction) => Self::Checkpoint(compaction),
            None if settings.require_end_turn => Self::EndTurn,
            None => Self::Reply,
        }
    }

    pub(crate) const fn reply_ends_turn(self) -> bool {
        matches!(self, Self::Reply)
    }

    pub(crate) const fn end_turn_must_record(self) -> bool {
        matches!(self, Self::EndTurn)
    }

    pub(crate) const fn compaction(self) -> Option<Compaction<'a>> {
        match self {
            Self::Checkpoint(compaction) => Some(compaction),
            Self::Reply | Self::EndTurn => None,
        }
    }

    pub(crate) const fn is_compaction(self) -> bool {
        matches!(self, Self::Checkpoint(_))
    }

    pub(crate) const fn reminder(self) -> &'static str {
        match self {
            Self::Checkpoint(_) => CHECKPOINT_REMINDER,
            Self::EndTurn => END_TURN_REMINDER,
            Self::Reply => EMPTY_RESPONSE_REMINDER,
        }
    }
}
