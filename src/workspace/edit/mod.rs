pub mod diff;
pub mod format;
pub mod ir;
pub mod normalize;
pub mod recovery;
pub mod stage;
pub mod validate;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditTraceId {
    Normalize,
    EditIrNew,
    Validate,
    Stage,
    Verify,
    Recovery,
}

impl EditTraceId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normalize => "edit.normalize.v1",
            Self::EditIrNew => "edit.ir.new.v1",
            Self::Validate => "edit.validate.v1",
            Self::Stage => "edit.stage.v1",
            Self::Verify => "edit.verify.v1",
            Self::Recovery => "edit.recovery.v1",
        }
    }
}

pub trait EditTrace {
    fn emit(&mut self, id: EditTraceId);
}

impl EditTrace for () {
    fn emit(&mut self, _id: EditTraceId) {}
}
