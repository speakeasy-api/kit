//! Capability abstractions shared by tools, MCP servers, and agentkit hosts.
//!
//! This crate defines the [`Invocable`] trait and its supporting types, which
//! let you expose arbitrary functionality (tools, resources, prompts) through a
//! uniform interface that the agentkit loop can discover and call during a
//! session.
//!
//! # Overview
//!
//! The core abstraction is [`Invocable`]: anything the model can call.  Each
//! invocable carries an [`InvocableSpec`] (name, description, JSON-schema for
//! its input) and an async `invoke` method that receives an
//! [`InvocableRequest`] and returns an [`InvocableResult`].
//!
//! Beyond direct invocation the crate also provides:
//!
//! * [`ResourceProvider`] -- lists and reads named data blobs (files, database
//!   rows, API responses) that the model can reference.
//! * [`PromptProvider`] -- lists and renders parameterised prompt templates.
//! * [`CapabilityProvider`] -- a bundle that groups invocables, resources, and
//!   prompts from a single source (e.g. an MCP server).
//!
//! All provider traits share a common [`CapabilityContext`] that carries the
//! current session and turn identifiers, plus an open-ended metadata map.
//!
//! # Example
//!
//! ```rust
//! use agentkit_capabilities::{
//!     CapabilityContext, CapabilityError, CapabilityName, Invocable,
//!     InvocableOutput, InvocableRequest, InvocableResult, InvocableSpec,
//! };
//! use async_trait::async_trait;
//! use serde_json::json;
//!
//! /// A simple capability that echoes its input back to the model.
//! struct Echo {
//!     spec: InvocableSpec,
//! }
//!
//! impl Echo {
//!     fn new() -> Self {
//!         Self {
//!             spec: InvocableSpec::new(
//!                 CapabilityName::new("echo"),
//!                 "Return the input unchanged",
//!                 json!({
//!                     "type": "object",
//!                     "properties": {
//!                         "message": { "type": "string" }
//!                     }
//!                 }),
//!             ),
//!         }
//!     }
//! }
//!
//! #[async_trait]
//! impl Invocable for Echo {
//!     fn spec(&self) -> &InvocableSpec {
//!         &self.spec
//!     }
//!
//!     async fn invoke(
//!         &self,
//!         request: InvocableRequest,
//!         _ctx: &mut CapabilityContext<'_>,
//!     ) -> Result<InvocableResult, CapabilityError> {
//!         Ok(InvocableResult::structured(request.input.clone()))
//!     }
//! }
//!
//! let echo = Echo::new();
//! assert_eq!(echo.spec().name.as_str(), "echo");
//! ```

use std::fmt;
use std::sync::Arc;

use agentkit_core::{DataRef, Item, MetadataMap, SessionId, TurnId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

macro_rules! capability_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        ///
        /// This is a newtype wrapper around [`String`] that provides
        /// type-safe identity within the capability system. It implements
        /// [`Display`](fmt::Display), serialisation, ordering, and hashing.
        ///
        /// # Example
        ///
        /// ```rust
        #[doc = concat!("use agentkit_capabilities::", stringify!($name), ";")]
        ///
        #[doc = concat!("let id = ", stringify!($name), "::new(\"my-id\");")]
        #[doc = concat!("assert_eq!(id.as_str(), \"my-id\");")]
        /// ```
        #[derive(
            Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(pub String);

        impl $name {
            /// Creates a new identifier from any value that can be converted
            /// into a [`String`].
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Returns the underlying string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

capability_id!(
    CapabilityName,
    "Unique name for an [`Invocable`] capability."
);
capability_id!(ResourceId, "Unique identifier for a resource.");
capability_id!(PromptId, "Unique identifier for a prompt template.");

/// Describes an [`Invocable`] capability so it can be advertised to the model.
///
/// The spec is presented to the model alongside other available tools so that
/// it can decide when to call the capability.  The `input_schema` field should
/// be a valid JSON Schema object describing the expected input shape.
///
/// # Example
///
/// ```rust
/// use agentkit_capabilities::{CapabilityName, InvocableSpec};
/// use serde_json::json;
///
/// let spec = InvocableSpec::new(
///     CapabilityName::new("search"),
///     "Search the codebase for a pattern",
///     json!({
///         "type": "object",
///         "properties": {
///             "query": { "type": "string" }
///         },
///         "required": ["query"]
///     }),
/// );
///
/// assert_eq!(spec.name.as_str(), "search");
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocableSpec {
    /// The capability name that the model uses to reference this invocable.
    pub name: CapabilityName,
    /// A human-readable description shown to the model so it can decide when
    /// to invoke this capability.
    pub description: String,
    /// A JSON Schema describing the expected shape of
    /// [`InvocableRequest::input`].
    pub input_schema: Value,
    /// Arbitrary key-value metadata attached to the spec.
    pub metadata: MetadataMap,
}

impl InvocableSpec {
    /// Builds an invocable spec with empty metadata.
    pub fn new(
        name: impl Into<CapabilityName>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the spec metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// A request to execute an [`Invocable`] capability.
///
/// Created by the agentkit loop when the model emits a tool-call that targets
/// a registered invocable. The `input` field contains the arguments the model
/// provided, validated against the capability's [`InvocableSpec::input_schema`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocableRequest {
    /// The JSON input arguments provided by the model.
    pub input: Value,
    /// The session in which this invocation occurs, if available.
    pub session_id: Option<SessionId>,
    /// The turn within the session, if available.
    pub turn_id: Option<TurnId>,
    /// Arbitrary key-value metadata attached to this request.
    pub metadata: MetadataMap,
}

impl InvocableRequest {
    /// Builds an invocable request with no session or turn ids.
    pub fn new(input: Value) -> Self {
        Self {
            input,
            session_id: None,
            turn_id: None,
            metadata: MetadataMap::new(),
        }
    }

    /// Sets the session id.
    pub fn with_session(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Sets the turn id.
    pub fn with_turn(mut self, turn_id: impl Into<TurnId>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    /// Replaces the request metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// The result of executing an [`Invocable`] capability.
///
/// Returned by [`Invocable::invoke`] on success.  The `output` field carries
/// the actual content while `metadata` can hold timing information, cache
/// headers, or any other sideband data the caller might need.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocableResult {
    /// The content produced by the invocable.
    pub output: InvocableOutput,
    /// Arbitrary key-value metadata about the execution (e.g. latency, cache
    /// status).
    pub metadata: MetadataMap,
}

impl InvocableResult {
    /// Builds an invocable result with empty metadata.
    pub fn new(output: InvocableOutput) -> Self {
        Self {
            output,
            metadata: MetadataMap::new(),
        }
    }

    /// Builds a plain-text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(InvocableOutput::Text(text.into()))
    }

    /// Builds a structured result.
    pub fn structured(value: Value) -> Self {
        Self::new(InvocableOutput::Structured(value))
    }

    /// Builds an items result.
    pub fn items(items: Vec<Item>) -> Self {
        Self::new(InvocableOutput::Items(items))
    }

    /// Builds a data-reference result.
    pub fn data(data: DataRef) -> Self {
        Self::new(InvocableOutput::Data(data))
    }

    /// Replaces the result metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// The output payload of an [`Invocable`] execution.
///
/// Capabilities may return plain text, structured JSON, a sequence of
/// conversation [`Item`]s, or a raw data reference depending on the nature of
/// the work they perform.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InvocableOutput {
    /// A plain-text response.
    Text(String),
    /// A structured JSON value.
    Structured(Value),
    /// One or more conversation items (messages, tool results, etc.).
    Items(Vec<Item>),
    /// A reference to binary or text data (inline bytes, a URI, etc.).
    Data(DataRef),
}

/// Describes a resource that a [`ResourceProvider`] can serve.
///
/// Resource descriptors are returned by
/// [`ResourceProvider::list_resources`] so that the host or model can
/// discover what data is available and request it by [`ResourceId`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDescriptor {
    /// Unique identifier used to request this resource.
    pub id: ResourceId,
    /// A short, human-readable name for the resource.
    pub name: String,
    /// An optional longer description of the resource contents.
    pub description: Option<String>,
    /// The MIME type of the resource data (e.g. `"text/plain"`,
    /// `"application/json"`).
    pub mime_type: Option<String>,
    /// Arbitrary key-value metadata attached to the descriptor.
    pub metadata: MetadataMap,
}

impl ResourceDescriptor {
    /// Builds a resource descriptor with no description or mime type.
    pub fn new(id: impl Into<ResourceId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: None,
            mime_type: None,
            metadata: MetadataMap::new(),
        }
    }

    /// Sets the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets the mime type.
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// Replaces the descriptor metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// The payload returned when a resource is read.
///
/// Obtained by calling [`ResourceProvider::read_resource`] with a
/// [`ResourceId`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceContents {
    /// The resource data, which may be inline text, inline bytes, a URI, or
    /// an artifact handle.
    pub data: DataRef,
    /// Arbitrary key-value metadata about the read (e.g. ETag, last-modified).
    pub metadata: MetadataMap,
}

impl ResourceContents {
    /// Builds resource contents with empty metadata.
    pub fn new(data: DataRef) -> Self {
        Self {
            data,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the contents metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Describes a prompt template that a [`PromptProvider`] can render.
///
/// Prompt descriptors are returned by [`PromptProvider::list_prompts`] so the
/// host can discover available templates and present them to the user or model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptDescriptor {
    /// Unique identifier used to request this prompt.
    pub id: PromptId,
    /// A short, human-readable name for the prompt.
    pub name: String,
    /// An optional longer description of when or why to use the prompt.
    pub description: Option<String>,
    /// A JSON Schema describing the arguments the prompt template accepts.
    pub input_schema: Value,
    /// Arbitrary key-value metadata attached to the descriptor.
    pub metadata: MetadataMap,
}

impl PromptDescriptor {
    /// Builds a prompt descriptor with no description and empty metadata.
    pub fn new(id: impl Into<PromptId>, name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: None,
            input_schema,
            metadata: MetadataMap::new(),
        }
    }

    /// Sets the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Replaces the descriptor metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// The rendered output of a prompt template.
///
/// Returned by [`PromptProvider::get_prompt`] after applying the provided
/// arguments to the template. The resulting items are typically prepended to
/// the conversation transcript before the next model turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptContents {
    /// The conversation items produced by rendering the prompt.
    pub items: Vec<Item>,
    /// Arbitrary key-value metadata about the rendering.
    pub metadata: MetadataMap,
}

impl PromptContents {
    /// Builds prompt contents with empty metadata.
    pub fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the contents metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Shared execution context passed to all capability invocations.
///
/// The context carries the current session and turn identifiers so that
/// capabilities can correlate their work with the broader conversation.
/// A mutable reference is passed to every [`Invocable::invoke`],
/// [`ResourceProvider::read_resource`], and [`PromptProvider::get_prompt`]
/// call.
///
/// # Example
///
/// ```rust
/// use agentkit_capabilities::CapabilityContext;
/// use agentkit_core::{MetadataMap, SessionId, TurnId};
///
/// let session = SessionId::new("sess-1");
/// let turn = TurnId::new("turn-1");
/// let meta = MetadataMap::new();
///
/// let mut ctx = CapabilityContext {
///     session_id: Some(&session),
///     turn_id: Some(&turn),
///     metadata: &meta,
/// };
///
/// assert_eq!(ctx.session_id.unwrap().0, "sess-1");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityContext<'a> {
    /// The active session identifier, if one has been established.
    pub session_id: Option<&'a SessionId>,
    /// The current turn identifier within the session, if available.
    pub turn_id: Option<&'a TurnId>,
    /// Ambient metadata shared across all capabilities for this invocation.
    pub metadata: &'a MetadataMap,
}

/// A capability that the model can invoke during a conversation turn.
///
/// Implement this trait to expose custom functionality -- database queries,
/// API calls, file operations, code execution -- to the agentic loop. Each
/// implementor provides a [`spec`](Invocable::spec) describing the capability
/// and an async [`invoke`](Invocable::invoke) method that performs the work.
///
/// The agentkit loop discovers invocables through a [`CapabilityProvider`]
/// and presents them to the model alongside regular tools.
///
/// # Example
///
/// ```rust
/// use agentkit_capabilities::{
///     CapabilityContext, CapabilityError, CapabilityName, Invocable,
///     InvocableOutput, InvocableRequest, InvocableResult, InvocableSpec,
/// };
/// use async_trait::async_trait;
/// use serde_json::json;
///
/// struct CurrentTime {
///     spec: InvocableSpec,
/// }
///
/// impl CurrentTime {
///     fn new() -> Self {
///         Self {
///             spec: InvocableSpec::new(
///                 CapabilityName::new("current_time"),
///                 "Return the current UTC time",
///                 json!({ "type": "object" }),
///             ),
///         }
///     }
/// }
///
/// #[async_trait]
/// impl Invocable for CurrentTime {
///     fn spec(&self) -> &InvocableSpec {
///         &self.spec
///     }
///
///     async fn invoke(
///         &self,
///         _request: InvocableRequest,
///         _ctx: &mut CapabilityContext<'_>,
///     ) -> Result<InvocableResult, CapabilityError> {
///         Ok(InvocableResult::text("2026-03-22T12:00:00Z"))
///     }
/// }
/// ```
#[async_trait]
pub trait Invocable: Send + Sync {
    /// Returns the specification that describes this capability to the model.
    fn spec(&self) -> &InvocableSpec;

    /// Executes the capability with the given request and context.
    ///
    /// # Arguments
    ///
    /// * `request` - The invocation request containing the model-provided input
    ///   and session identifiers.
    /// * `ctx` - The shared capability context for this turn.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityError`] if the capability is unavailable, the input
    /// is invalid, or execution fails.
    async fn invoke(
        &self,
        request: InvocableRequest,
        ctx: &mut CapabilityContext<'_>,
    ) -> Result<InvocableResult, CapabilityError>;
}

/// A provider of named data resources that can be listed and read.
///
/// Implement this trait to expose data sources -- files on disk, database
/// rows, API responses -- to the model. The agentkit MCP bridge uses this
/// trait to surface MCP-server resources into the agentic loop.
///
/// # Example
///
/// ```rust
/// use agentkit_capabilities::{
///     CapabilityContext, CapabilityError, ResourceContents,
///     ResourceDescriptor, ResourceId, ResourceProvider,
/// };
/// use agentkit_core::{DataRef, MetadataMap};
/// use async_trait::async_trait;
///
/// struct StaticFile;
///
/// #[async_trait]
/// impl ResourceProvider for StaticFile {
///     async fn list_resources(&self) -> Result<Vec<ResourceDescriptor>, CapabilityError> {
///         Ok(vec![ResourceDescriptor {
///             id: ResourceId::new("readme"),
///             name: "README.md".into(),
///             description: Some("Project readme".into()),
///             mime_type: Some("text/markdown".into()),
///             metadata: MetadataMap::new(),
///         }])
///     }
///
///     async fn read_resource(
///         &self,
///         id: &ResourceId,
///         _ctx: &mut CapabilityContext<'_>,
///     ) -> Result<ResourceContents, CapabilityError> {
///         if id.as_str() == "readme" {
///             Ok(ResourceContents {
///                 data: DataRef::InlineText("# Hello".into()),
///                 metadata: MetadataMap::new(),
///             })
///         } else {
///             Err(CapabilityError::Unavailable(format!(
///                 "unknown resource: {id}"
///             )))
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait ResourceProvider: Send + Sync {
    /// Lists all resources currently available from this provider.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityError`] if the provider cannot enumerate its
    /// resources (e.g. a network timeout).
    async fn list_resources(&self) -> Result<Vec<ResourceDescriptor>, CapabilityError>;

    /// Reads the contents of the resource identified by `id`.
    ///
    /// # Arguments
    ///
    /// * `id` - The unique resource identifier, as returned in a
    ///   [`ResourceDescriptor`].
    /// * `ctx` - The shared capability context for this turn.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityError::Unavailable`] if the resource does not exist
    /// or [`CapabilityError::ExecutionFailed`] if reading fails.
    async fn read_resource(
        &self,
        id: &ResourceId,
        ctx: &mut CapabilityContext<'_>,
    ) -> Result<ResourceContents, CapabilityError>;
}

/// A provider of parameterised prompt templates.
///
/// Implement this trait to offer reusable prompt templates that the host can
/// render with user-supplied arguments and inject into the conversation
/// transcript. The agentkit MCP bridge uses this trait to surface MCP-server
/// prompts into the agentic loop.
#[async_trait]
pub trait PromptProvider: Send + Sync {
    /// Lists all prompt templates currently available from this provider.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityError`] if the provider cannot enumerate its
    /// prompts.
    async fn list_prompts(&self) -> Result<Vec<PromptDescriptor>, CapabilityError>;

    /// Renders a prompt template with the given arguments.
    ///
    /// # Arguments
    ///
    /// * `id` - The unique prompt identifier, as returned in a
    ///   [`PromptDescriptor`].
    /// * `args` - A JSON value containing the template arguments, validated
    ///   against the prompt's [`PromptDescriptor::input_schema`].
    /// * `ctx` - The shared capability context for this turn.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityError::Unavailable`] if the prompt does not exist,
    /// [`CapabilityError::InvalidInput`] if the arguments are malformed, or
    /// [`CapabilityError::ExecutionFailed`] if rendering fails.
    async fn get_prompt(
        &self,
        id: &PromptId,
        args: Value,
        ctx: &mut CapabilityContext<'_>,
    ) -> Result<PromptContents, CapabilityError>;
}

/// A bundle of capabilities from a single source.
///
/// [`CapabilityProvider`] groups [`Invocable`]s, [`ResourceProvider`]s, and
/// [`PromptProvider`]s that originate from the same backend -- for example,
/// a single MCP server or a built-in tool collection.  The agentkit loop
/// collects providers and merges their contents into the unified tool list
/// presented to the model.
///
/// # Example
///
/// ```rust
/// use std::sync::Arc;
/// use agentkit_capabilities::{
///     CapabilityProvider, Invocable, PromptProvider, ResourceProvider,
/// };
///
/// struct EmptyProvider;
///
/// impl CapabilityProvider for EmptyProvider {
///     fn invocables(&self) -> Vec<Arc<dyn Invocable>> {
///         vec![]
///     }
///
///     fn resources(&self) -> Vec<Arc<dyn ResourceProvider>> {
///         vec![]
///     }
///
///     fn prompts(&self) -> Vec<Arc<dyn PromptProvider>> {
///         vec![]
///     }
/// }
/// ```
pub trait CapabilityProvider: Send + Sync {
    /// Returns all invocable capabilities offered by this provider.
    fn invocables(&self) -> Vec<Arc<dyn Invocable>>;

    /// Returns all resource providers offered by this provider.
    fn resources(&self) -> Vec<Arc<dyn ResourceProvider>>;

    /// Returns all prompt providers offered by this provider.
    fn prompts(&self) -> Vec<Arc<dyn PromptProvider>>;
}

/// Errors that can occur when interacting with capabilities.
///
/// This enum is used as the error type across all capability traits
/// ([`Invocable`], [`ResourceProvider`], [`PromptProvider`]).
#[derive(Debug, Error)]
pub enum CapabilityError {
    /// The requested capability, resource, or prompt is not available.
    ///
    /// Returned when the identifier does not match any registered item or
    /// when the provider is temporarily offline.
    #[error("capability unavailable: {0}")]
    Unavailable(String),

    /// The input provided to the capability is invalid.
    ///
    /// Returned when the model-supplied arguments fail schema validation or
    /// contain values outside the expected domain.
    #[error("invalid capability input: {0}")]
    InvalidInput(String),

    /// The capability encountered an error during execution.
    ///
    /// Returned for runtime failures such as network timeouts, I/O errors,
    /// or unexpected backend responses.
    #[error("capability execution failed: {0}")]
    ExecutionFailed(String),
}
