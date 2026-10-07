pub mod engine;
pub(crate) mod handlers;

pub(crate) const FORWARD_HEADERS_CONTEXT_KEY: &str = "__astra_forward_headers";
pub(crate) const REQUEST_ALLOWED_TOOLS_CONTEXT_KEY: &str = "__astra_request_allowed_tools";
pub(crate) const REQUEST_ENABLED_TOOLS_CONTEXT_KEY: &str = "__astra_request_enabled_tools";
pub(crate) const REQUEST_ALLOWED_SKILLS_CONTEXT_KEY: &str = "__astra_request_allowed_skills";
pub(crate) const REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY: &str =
    "__astra_request_allowed_skill_sources";
