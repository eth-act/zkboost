use serde::{Deserialize, Serialize};
use ere_zkvm_interface::Input;


#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(transparent)]
pub struct ProgramInput {
    pub input: Vec<u8>,
}

impl From<ProgramInput> for Input {
    fn from(value: ProgramInput) -> Self {
        let input = Input::new();
        input.with_prefixed_stdin(value.input)
    }
}