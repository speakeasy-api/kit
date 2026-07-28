#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderStep {
    Chunk(String),
    ToolCall { effect: String, argument: String },
    SecretBlocked { destination: String },
    Error { code: String, message: String },
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ScriptStep {
    Emit(ProviderStep),
    SecretAttempt { canary: String, destination: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderScript {
    steps: Vec<ScriptStep>,
}

impl ProviderScript {
    pub fn streaming(chunks: &[&str]) -> Self {
        let mut steps = chunks
            .iter()
            .map(|chunk| ScriptStep::Emit(ProviderStep::Chunk((*chunk).to_owned())))
            .collect::<Vec<_>>();
        steps.push(ScriptStep::Emit(ProviderStep::Complete));
        Self { steps }
    }

    pub fn error_after(chunks: &[&str], after: usize, code: &str) -> Self {
        let mut steps = chunks
            .iter()
            .take(after)
            .map(|chunk| ScriptStep::Emit(ProviderStep::Chunk((*chunk).to_owned())))
            .collect::<Vec<_>>();
        steps.push(ScriptStep::Emit(ProviderStep::Error {
            code: code.to_owned(),
            message: "injected provider failure".to_owned(),
        }));
        Self { steps }
    }

    pub fn prompt_injection(instruction: &str, effect: &str) -> Self {
        Self {
            steps: vec![ScriptStep::Emit(ProviderStep::ToolCall {
                effect: effect.to_owned(),
                argument: instruction.to_owned(),
            })],
        }
    }

    pub fn secret_exfiltration(secret: &str) -> Self {
        Self {
            steps: vec![ScriptStep::SecretAttempt {
                canary: secret.to_owned(),
                destination: "event_payload".to_owned(),
            }],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderEvent {
    pub request_id: u64,
    pub sequence: usize,
    pub step: ProviderStep,
}

#[derive(Clone, Debug)]
pub struct FakeProvider {
    seed: u64,
    script: ProviderScript,
}

impl FakeProvider {
    pub fn new(seed: u64, script: ProviderScript) -> Self {
        Self { seed, script }
    }

    pub fn replay(&self) -> Vec<ProviderEvent> {
        let request_id = mix(self.seed);
        self.script
            .steps
            .iter()
            .enumerate()
            .map(|(sequence, step)| {
                let step = match step {
                    ScriptStep::Emit(step) => step.clone(),
                    ScriptStep::SecretAttempt {
                        canary,
                        destination,
                    } => {
                        assert!(!canary.is_empty(), "fixture canary must be non-empty");
                        ProviderStep::SecretBlocked {
                            destination: destination.clone(),
                        }
                    }
                };
                ProviderEvent {
                    request_id,
                    sequence,
                    step,
                }
            })
            .collect()
    }

    pub fn persist(events: &[ProviderEvent]) -> Vec<u8> {
        events
            .iter()
            .map(|event| format!("{}|{}|{:?}\n", event.request_id, event.sequence, event.step))
            .collect::<String>()
            .into_bytes()
    }
}

fn mix(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}
