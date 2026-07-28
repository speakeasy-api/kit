//! Sandboxed Lua execution backend (the default).

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentkit_core::TurnCancellation;
use agentkit_tools_core::{ToolError, ToolInterruption, ToolName, ToolSpec};
use async_trait::async_trait;
use mlua::{HookTriggers, Lua, LuaSerdeExt, Value as LuaValue, VmState};
use serde_json::Value;

use crate::{
    BackendRun, CallKey, ComposeBackend, ComposeOutcome, DispatchError, render_catalog_shapes,
};

/// Executes compose scripts as sandboxed Lua with a synchronous-looking
/// `tool(name, input)` helper.
#[derive(Clone, Copy, Debug, Default)]
pub struct LuaBackend;

#[async_trait]
impl ComposeBackend for LuaBackend {
    fn name(&self) -> &'static str {
        "lua"
    }

    fn script_description(&self) -> &'static str {
        "Lua script to execute. Return a value to make it the compose result."
    }

    fn description(&self, catalog: Option<&[ToolSpec]>) -> String {
        let mut description = String::from(
            "Run a sandboxed Lua script that composes available tools through tool(name, input). \
             Prefer this tool whenever a task takes more than two tool calls: iterating over \
             list results, paginating, fetching details per item, filtering or aggregating tool \
             output, or chaining reads into writes. The whole script executes in a single \
             round-trip — one compose call replaces N individual calls — and only the script's \
             return value enters the conversation, so intermediate results never consume \
             context. The script sees a global `input` (the JSON value passed alongside the \
             script) and may call `tools()` to enumerate the visible tool catalog at runtime. \
             Return any Lua value to make it the compose result.\n\n\
             Example — scan every page, drill into matches, return only the summary:\n\
             local page, hits = 1, {}\n\
             repeat\n\
             \x20 local r = tool('list_items', { page = page })\n\
             \x20 for _, it in ipairs(r.items) do\n\
             \x20   if it.status == 'open' then hits[#hits + 1] = tool('get_item', { id = it.id }) end\n\
             \x20 end\n\
             \x20 page = page + 1\n\
             until page > r.total_pages\n\
             return { count = #hits, items = hits }",
        );
        if let Some(catalog) = catalog {
            if !catalog.is_empty() {
                description.push_str(&render_catalog_shapes(catalog));
            }
        }
        description
    }

    async fn execute(&self, run: BackendRun) -> Result<Value, ComposeOutcome> {
        let lua = Lua::new();
        install_instruction_limit(
            &lua,
            run.config.max_instruction_count,
            run.cancellation.clone(),
        )
        .map_err(lua_error_to_outcome)?;
        install_sandbox(&lua).map_err(lua_error_to_outcome)?;
        let globals = lua.globals();
        globals
            .set(
                "input",
                lua.to_value(&run.input).map_err(lua_error_to_outcome)?,
            )
            .map_err(lua_error_to_outcome)?;

        let specs_value = serde_json::to_value(&run.visible_specs)
            .map_err(|error| ComposeOutcome::Failed(ToolError::Internal(error.to_string())))?;
        globals
            .set(
                "tools",
                lua.create_function(move |lua, ()| lua.to_value(&specs_value))
                    .map_err(lua_error_to_outcome)?,
            )
            .map_err(lua_error_to_outcome)?;

        let dispatcher = run.dispatcher.clone();
        let tool_fn = lua
            .create_async_function(move |lua, (name, lua_input): (String, LuaValue)| {
                let dispatcher = dispatcher.clone();
                async move {
                    let child_input: Value = lua.from_value(lua_input)?;
                    match dispatcher
                        .call(CallKey::Sequential, ToolName::new(name), child_input)
                        .await
                    {
                        Ok(output) => lua.to_value(&output),
                        Err(DispatchError::Interrupted(interruption)) => {
                            Err(mlua::Error::external(ComposeInterrupt(interruption)))
                        }
                        Err(DispatchError::Failed(error)) => {
                            Err(mlua::Error::external(ComposeFailure(error)))
                        }
                    }
                }
            })
            .map_err(lua_error_to_outcome)?;
        globals.set("tool", tool_fn).map_err(lua_error_to_outcome)?;

        let result = match lua
            .load(run.script.as_str())
            .set_name("compose")
            .eval_async::<LuaValue>()
            .await
        {
            Ok(value) => value,
            Err(error) => return Err(lua_error_to_outcome(error)),
        };
        lua.from_value(result).map_err(lua_error_to_outcome)
    }
}

#[derive(Debug)]
struct ComposeInterrupt(ToolInterruption);

impl fmt::Display for ComposeInterrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "compose interrupted")
    }
}

impl Error for ComposeInterrupt {}

#[derive(Debug)]
struct ComposeFailure(ToolError);

impl fmt::Display for ComposeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for ComposeFailure {}

fn install_sandbox(lua: &Lua) -> Result<(), mlua::Error> {
    let globals = lua.globals();
    for name in [
        "collectgarbage",
        "dofile",
        "load",
        "loadfile",
        "require",
        "io",
        "os",
        "package",
        "debug",
    ] {
        globals.set(name, LuaValue::Nil)?;
    }
    Ok(())
}

fn install_instruction_limit(
    lua: &Lua,
    max_instruction_count: u64,
    cancellation: Option<TurnCancellation>,
) -> Result<(), mlua::Error> {
    if max_instruction_count == 0 {
        return Ok(());
    }
    let step = max_instruction_count.min(1_000) as u32;
    let seen = Arc::new(AtomicU64::new(0));
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(step),
        move |_lua, _debug| {
            if cancellation
                .as_ref()
                .is_some_and(|cancellation| cancellation.is_cancelled())
            {
                return Err(mlua::Error::external(ComposeFailure(ToolError::Cancelled)));
            }
            let previous = seen.fetch_add(u64::from(step), Ordering::Relaxed);
            if previous.saturating_add(u64::from(step)) > max_instruction_count {
                return Err(mlua::Error::external(ComposeFailure(
                    ToolError::ExecutionFailed(format!(
                        "compose exceeded {max_instruction_count} Lua instructions"
                    )),
                )));
            }
            Ok(VmState::Continue)
        },
    )
}

fn lua_error_to_outcome(error: mlua::Error) -> ComposeOutcome {
    match &error {
        mlua::Error::CallbackError { cause, .. }
        | mlua::Error::BadArgument { cause, .. }
        | mlua::Error::WithContext { cause, .. } => {
            return lua_error_to_outcome((**cause).clone());
        }
        mlua::Error::ExternalError(inner) => {
            if let Some(interrupt) = inner.downcast_ref::<ComposeInterrupt>() {
                return ComposeOutcome::Interrupted(interrupt.0.clone());
            }
            if let Some(failure) = inner.downcast_ref::<ComposeFailure>() {
                return ComposeOutcome::Failed(failure.0.clone());
            }
        }
        _ => {}
    }
    ComposeOutcome::Failed(ToolError::ExecutionFailed(error.to_string()))
}
