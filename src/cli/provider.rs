use std::env;

use serde_json::json;

use crate::{
    agent::providers::config::{ProviderProfile, ProviderRegistry, config_path},
    cli::core::{
        ClientError, ClientErrorKind, EXIT_OK, Output, OutputFormat, render_exec_response,
    },
};

#[derive(Clone, Debug)]
pub enum ProviderCommand {
    Path,
    List,
    Add(Box<ProviderAdd>),
    Use { name: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    OpenRouter,
    Ollama,
}

#[derive(Clone, Debug)]
pub struct ProviderAdd {
    pub name: String,
    pub provider: ProviderKind,
    pub replace: bool,
    pub api_key_env: Option<String>,
    pub auth_token_env: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub version: Option<String>,
    pub beta: Option<String>,
    pub app_name: Option<String>,
    pub site_url: Option<String>,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<String>,
}

pub fn execute(command: ProviderCommand, format: OutputFormat) -> Result<Output, ClientError> {
    let path = config_path().map_err(invalid)?;
    match command {
        ProviderCommand::Path => {
            if format == OutputFormat::Human {
                Ok(human(format!("{}\n", path.display())))
            } else {
                render_exec_response(json!({ "path": path }), format)
            }
        }
        ProviderCommand::List => {
            let registry = ProviderRegistry::load_from(&path).map_err(invalid)?;
            let items = registry
                .as_ref()
                .map(|registry| {
                    let (current, _) = registry.current();
                    registry
                        .profiles()
                        .map(|(name, profile)| {
                            json!({
                                "name": name,
                                "provider": profile.provider_name(),
                                "current": name == current,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if format == OutputFormat::Human {
                let output = if items.is_empty() {
                    "No provider profiles configured.\n".to_owned()
                } else {
                    items
                        .iter()
                        .map(|item| {
                            format!(
                                "{} {} {}",
                                if item["current"] == true { "*" } else { " " },
                                item["name"].as_str().expect("string"),
                                item["provider"].as_str().expect("string")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n"
                };
                Ok(human(output))
            } else {
                render_exec_response(json!({ "items": items }), format)
            }
        }
        ProviderCommand::Add(add) => {
            let profile = add.profile()?;
            let provider = profile.provider_name();
            let registry = match ProviderRegistry::load_from(&path).map_err(invalid)? {
                Some(mut registry) => {
                    registry
                        .add(add.name.clone(), profile, add.replace)
                        .map_err(invalid)?;
                    registry
                }
                None => ProviderRegistry::new(add.name.clone(), profile).map_err(invalid)?,
            };
            let current = registry.current().0 == add.name;
            registry.write_to(&path).map_err(invalid)?;
            if format == OutputFormat::Human {
                Ok(human(format!(
                    "Added provider profile {} ({provider}){}.\n",
                    add.name,
                    if current { " and selected it" } else { "" }
                )))
            } else {
                render_exec_response(
                    json!({ "name": add.name, "provider": provider, "current": current }),
                    format,
                )
            }
        }
        ProviderCommand::Use { name } => {
            let mut registry = ProviderRegistry::load_from(&path)
                .map_err(invalid)?
                .ok_or_else(|| invalid("no provider profiles are configured"))?;
            registry.use_profile(&name).map_err(invalid)?;
            registry.write_to(&path).map_err(invalid)?;
            if format == OutputFormat::Human {
                Ok(human(format!(
                    "Selected provider profile {name}. Daemon restart required.\n"
                )))
            } else {
                render_exec_response(
                    json!({ "name": name, "current": true, "daemon_restart_required": true }),
                    format,
                )
            }
        }
    }
}

impl ProviderAdd {
    fn profile(&self) -> Result<ProviderProfile, ClientError> {
        self.validate_options()?;
        let credential = |variable: &str| {
            let value = env::var(variable).map_err(|_| {
                invalid(format!(
                    "credential environment variable {variable:?} is missing or not valid UTF-8"
                ))
            })?;
            if value.is_empty() {
                return Err(invalid(format!(
                    "credential environment variable {variable:?} is empty"
                )));
            }
            Ok(value)
        };
        Ok(match self.provider {
            ProviderKind::OpenAi => ProviderProfile::openai(
                credential(self.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY"))?,
                self.model.clone(),
                self.base_url.clone(),
            )
            .map_err(invalid)?,
            ProviderKind::Anthropic => {
                let (api_key, auth_token) = if let Some(variable) = &self.auth_token_env {
                    (None, Some(credential(variable)?))
                } else if let Some(variable) = &self.api_key_env {
                    (Some(credential(variable)?), None)
                } else if env::var_os("ANTHROPIC_AUTH_TOKEN").is_some() {
                    (None, Some(credential("ANTHROPIC_AUTH_TOKEN")?))
                } else if env::var_os("ANTHROPIC_API_KEY").is_some() {
                    (Some(credential("ANTHROPIC_API_KEY")?), None)
                } else {
                    return Err(invalid(
                        "set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY, or pass an explicit credential environment override",
                    ));
                };
                ProviderProfile::anthropic(
                    api_key,
                    auth_token,
                    self.model.clone().expect("validated"),
                    self.max_tokens.expect("validated"),
                    self.base_url.clone(),
                    self.version.clone(),
                    self.beta.clone(),
                )
                .map_err(invalid)?
            }
            ProviderKind::OpenRouter => ProviderProfile::openrouter(
                credential(self.api_key_env.as_deref().unwrap_or("OPENROUTER_API_KEY"))?,
                self.model.clone(),
                self.base_url.clone(),
                self.app_name.clone(),
                self.site_url.clone(),
                self.max_completion_tokens,
                self.temperature,
                self.reasoning_effort.clone(),
            )
            .map_err(invalid)?,
            ProviderKind::Ollama => ProviderProfile::ollama(
                self.model.clone().expect("validated"),
                self.base_url.clone(),
            ),
        })
    }

    fn validate_options(&self) -> Result<(), ClientError> {
        let reject = |present: bool, option: &str, provider: ProviderKind| {
            if present && self.provider != provider {
                Err(invalid(format!(
                    "{option} is only valid for {}",
                    provider.as_str()
                )))
            } else {
                Ok(())
            }
        };
        reject(
            self.auth_token_env.is_some(),
            "--auth-token-env",
            ProviderKind::Anthropic,
        )?;
        for (present, option) in [
            (self.max_tokens.is_some(), "--max-tokens"),
            (self.version.is_some(), "--version"),
            (self.beta.is_some(), "--beta"),
        ] {
            reject(present, option, ProviderKind::Anthropic)?;
        }
        for (present, option) in [
            (self.app_name.is_some(), "--app-name"),
            (self.site_url.is_some(), "--site-url"),
            (
                self.max_completion_tokens.is_some(),
                "--max-completion-tokens",
            ),
            (self.temperature.is_some(), "--temperature"),
            (self.reasoning_effort.is_some(), "--reasoning-effort"),
        ] {
            reject(present, option, ProviderKind::OpenRouter)?;
        }
        match self.provider {
            ProviderKind::OpenAi | ProviderKind::OpenRouter => {
                if self.auth_token_env.is_some() {
                    return Err(invalid("--auth-token-env is not valid for this provider"));
                }
            }
            ProviderKind::Anthropic => {
                if self.api_key_env.is_some() && self.auth_token_env.is_some() {
                    return Err(invalid(
                        "--api-key-env and --auth-token-env cannot both be specified for anthropic",
                    ));
                }
                if self.model.as_deref().is_none_or(str::is_empty) {
                    return Err(invalid("--model is required for anthropic"));
                }
                if self.max_tokens.is_none() {
                    return Err(invalid("--max-tokens is required for anthropic"));
                }
            }
            ProviderKind::Ollama => {
                if self.api_key_env.is_some() || self.auth_token_env.is_some() {
                    return Err(invalid("credential options are not valid for ollama"));
                }
                if self.model.as_deref().is_none_or(str::is_empty) {
                    return Err(invalid("--model is required for ollama"));
                }
            }
        }
        Ok(())
    }
}

impl ProviderKind {
    pub fn parse(value: &str) -> Self {
        match value {
            "openai" => Self::OpenAi,
            "anthropic" => Self::Anthropic,
            "openrouter" => Self::OpenRouter,
            "ollama" => Self::Ollama,
            _ => unreachable!("Clap validates provider names"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
            Self::Ollama => "ollama",
        }
    }
}

fn invalid(error: impl ToString) -> ClientError {
    ClientError::new(ClientErrorKind::Invalid, error.to_string())
}

fn human(stdout: String) -> Output {
    Output {
        exit_code: EXIT_OK,
        stdout,
        stderr: String::new(),
    }
}
