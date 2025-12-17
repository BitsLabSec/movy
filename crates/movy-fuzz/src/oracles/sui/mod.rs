mod bool_judgement;
mod common;
mod infinite_loop;
mod overflow;
mod precision_loss;
mod proceeds;
mod type_conversion;
mod typed_bug;

pub use bool_judgement::BoolJudgementOracle;
pub use infinite_loop::InfiniteLoopOracle;
pub use overflow::OverflowOracle;
pub use precision_loss::PrecisionLossOracle;
pub use proceeds::ProceedsOracle;
pub use type_conversion::TypeConversionOracle;
pub use typed_bug::TypedBugOracle;
