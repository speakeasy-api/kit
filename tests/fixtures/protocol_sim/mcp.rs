#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolBinding {
    pub name: String,
    pub schema_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvocationDecision {
    Accepted { trace_id: u64 },
    RefusedSchemaDrift { discovered: String, invoked: String },
    RefusedUnknownTool,
}

#[derive(Clone, Debug)]
pub struct McpSimulator {
    seed: u64,
    discovered: Vec<ToolBinding>,
}

impl McpSimulator {
    pub fn new(seed: u64, discovered: Vec<ToolBinding>) -> Self {
        Self { seed, discovered }
    }

    pub fn invoke(&self, invocation: &ToolBinding) -> InvocationDecision {
        let Some(discovered) = self
            .discovered
            .iter()
            .find(|binding| binding.name == invocation.name)
        else {
            return InvocationDecision::RefusedUnknownTool;
        };

        if discovered.schema_digest != invocation.schema_digest {
            InvocationDecision::RefusedSchemaDrift {
                discovered: discovered.schema_digest.clone(),
                invoked: invocation.schema_digest.clone(),
            }
        } else {
            InvocationDecision::Accepted {
                trace_id: self.seed.rotate_left(23) ^ 0x4d43_5000,
            }
        }
    }
}
