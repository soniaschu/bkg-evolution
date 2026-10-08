//! Tools and their risk classification.
//!
//! Every tool carries one of three risk levels, and the approval policy is
//! driven by that level rather than by a per-tool allowlist nobody maintains.
//! An agent therefore cannot escalate its own permissions by asking for a
//! different tool: `read_file` stays `read_file`.
//!
//! `Destructive` is the class that matters. It is not "scary" — it is
//! irreversible, so the default policy denies it without an explicit grant.

use serde::{Deserialize, Serialize};

/// How much damage a tool can do that cannot be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    /// Reads state, changes nothing.
    ReadOnly,
    /// Changes state, reversibly.
    Mutating,
    /// Changes state irreversibly, or reaches outside the machine.
    Destructive,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::ReadOnly => "read-only",
            Risk::Mutating => "mutating",
            Risk::Destructive => "destructive",
        }
    }

    /// Parse from config, defaulting to the safest reading of anything
    /// unrecognised. A typo in a policy must never widen permissions.
    pub fn parse(raw: &str) -> Risk {
        match raw {
            "destructive" => Risk::Destructive,
            "mutating" => Risk::Mutating,
            "read-only" | "readonly" => Risk::ReadOnly,
            _ => Risk::Destructive,
        }
    }
}

/// What the operator decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// Needs a human. The loop pauses and waits.
    Ask,
}

/// The gate every tool call passes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Nothing runs. The safe default before setup.
    DenyAll,
    AllowReadOnly,
    /// Anything below destructive runs.
    AllowMutating,
    AllowAll,
}

impl Policy {
    pub fn parse(raw: &str) -> Policy {
        match raw {
            "allow-read-only" | "allow_read_only" | "read-only" => Policy::AllowReadOnly,
            "allow-mutating" | "allow_mutating" => Policy::AllowMutating,
            "allow-all" | "allow_all" => Policy::AllowAll,
            _ => Policy::DenyAll,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Policy::DenyAll => "deny-all",
            Policy::AllowReadOnly => "allow-read-only",
            Policy::AllowMutating => "allow-mutating",
            Policy::AllowAll => "allow-all",
        }
    }

    /// Whether this policy admits a tool of the given risk. Public because
    /// `bkgclaw tools` has to report which tools are allowed, not just which
    /// decisions the gate would reach.
    pub fn permits(self, risk: Risk) -> bool {
        match self {
            Policy::DenyAll => false,
            Policy::AllowReadOnly => risk == Risk::ReadOnly,
            Policy::AllowMutating => risk < Risk::Destructive,
            Policy::AllowAll => true,
        }
    }
}

/// A per-tool override, so an operator can grant one specific tool without
/// opening a whole class.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    entries: Vec<(String, Decision)>,
}

impl Overrides {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `"read_file=allow,bash=ask"` into decisions.
    pub fn parse(spec: &str) -> Self {
        let entries = spec
            .split(',')
            .filter_map(|pair| {
                let (name, decision) = pair.split_once('=')?;
                let decision = match decision.trim() {
                    "allow" => Decision::Allow,
                    "deny" => Decision::Deny,
                    "ask" => Decision::Ask,
                    // An unrecognised decision is a denial, not a pass.
                    _ => Decision::Deny,
                };
                Some((name.trim().to_string(), decision))
            })
            .collect();
        Overrides { entries }
    }

    pub fn get(&self, tool: &str) -> Option<Decision> {
        self.entries
            .iter()
            .find(|(name, _)| name == tool)
            .map(|(_, decision)| *decision)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The resolved answer, plus why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub decision: Decision,
    pub reason: String,
}

impl Gate {
    fn allow(reason: impl Into<String>) -> Self {
        Gate {
            decision: Decision::Allow,
            reason: reason.into(),
        }
    }
    fn deny(reason: impl Into<String>) -> Self {
        Gate {
            decision: Decision::Deny,
            reason: reason.into(),
        }
    }
    fn ask(reason: impl Into<String>) -> Self {
        Gate {
            decision: Decision::Ask,
            reason: reason.into(),
        }
    }
}

/// A registered tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub risk: Risk,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
}

impl Tool {
    pub fn new(name: &str, risk: Risk, description: &str) -> Self {
        Tool {
            name: name.to_string(),
            description: description.to_string(),
            risk,
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    pub fn with_params(mut self, parameters: serde_json::Value) -> Self {
        self.parameters = parameters;
        self
    }

    /// Render as a model-facing tool spec.
    pub fn to_spec(&self) -> crate::models::ToolSpec {
        crate::models::ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

/// The registry plus the policy that governs it.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: Vec<Tool>,
}

impl ToolRegistry {
    /// Construct directly. NOT `Self::default()` — `default()` delegates
    /// here, and the reverse delegation recurses until the stack dies.
    pub fn new() -> Self {
        ToolRegistry { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Tool) -> &mut Self {
        self.tools.push(tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn all(&self) -> &[Tool] {
        &self.tools
    }

    pub fn specs(&self) -> Vec<crate::models::ToolSpec> {
        self.tools.iter().map(Tool::to_spec).collect()
    }

    pub fn by_risk(&self, risk: Risk) -> Vec<&Tool> {
        self.tools.iter().filter(|t| t.risk == risk).collect()
    }

    /// Decide whether a tool may run. Order matters: a specific override beats
    /// the class policy, because an operator who grants one `write_file` did
    /// not mean to open every mutating tool.
    pub fn gate(&self, tool_name: &str, policy: Policy, overrides: &Overrides) -> Gate {
        let Some(tool) = self.get(tool_name) else {
            // An unknown tool is denied, not ignored. Silently skipping would
            // let an agent believe a write succeeded.
            return Gate::deny(format!("no tool named `{tool_name}` is registered"));
        };

        if let Some(decision) = overrides.get(tool_name) {
            return match decision {
                Decision::Allow => Gate::allow(format!(
                    "`{tool_name}` allowed by override (risk: {})",
                    tool.risk.as_str()
                )),
                Decision::Deny => Gate::deny(format!("`{tool_name}` denied by override")),
                Decision::Ask => Gate::ask(format!(
                    "`{tool_name}` requires approval (risk: {})",
                    tool.risk.as_str()
                )),
            };
        }

        if policy.permits(tool.risk) {
            Gate::allow(format!(
                "`{tool_name}` permitted by {} (risk: {})",
                policy.as_str(),
                tool.risk.as_str()
            ))
        } else if policy == Policy::DenyAll {
            Gate::deny(format!("policy is deny-all; `{tool_name}` did not run"))
        } else {
            Gate::deny(format!(
                "`{tool_name}` is {} and policy is {}",
                tool.risk.as_str(),
                policy.as_str()
            ))
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The standard tool set. Risk levels are the contract, not decoration:
/// `execute_command` is destructive because a shell can `rm -rf` and nothing
/// downstream can undo that.
pub fn builtin_tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    let string_param = |name: &str| {
        serde_json::json!({
            "type": "object",
            "properties": { name: { "type": "string" } },
            "required": [name]
        })
    };

    // Files
    registry.register(
        Tool::new("read_file", Risk::ReadOnly, "Read a file's contents")
            .with_params(string_param("path")),
    );
    registry.register(
        Tool::new("list_directory", Risk::ReadOnly, "List a directory")
            .with_params(string_param("path")),
    );
    registry.register(
        Tool::new("search_files", Risk::ReadOnly, "Search files by pattern").with_params(
            serde_json::json!({
                "type": "object",
                "properties": { "pattern": {"type":"string"}, "path": {"type":"string"} },
                "required": ["pattern"]
            }),
        ),
    );
    registry.register(
        Tool::new("write_file", Risk::Mutating, "Write a file, overwriting it").with_params(
            serde_json::json!({
                "type": "object",
                "properties": { "path": {"type":"string"}, "content": {"type":"string"} },
                "required": ["path","content"]
            }),
        ),
    );
    registry.register(Tool::new("edit_file", Risk::Mutating, "Replace an exact string in a file").with_params(serde_json::json!({
        "type": "object",
        "properties": { "path": {"type":"string"}, "old": {"type":"string"}, "new": {"type":"string"} },
        "required": ["path","old","new"]
    })));

    // Execution — destructive because a shell can do anything irreversible.
    registry.register(
        Tool::new("execute_command", Risk::Destructive, "Run a shell command")
            .with_params(string_param("command")),
    );

    // Web
    registry.register(
        Tool::new(
            "web_fetch",
            Risk::ReadOnly,
            "Fetch a URL's content (SSRF-guarded)",
        )
        .with_params(string_param("url")),
    );
    registry.register(
        Tool::new("web_search", Risk::ReadOnly, "Search the web")
            .with_params(string_param("query")),
    );
    registry.register(Tool::new("http_request", Risk::Mutating, "Make an HTTP request with a method and optional JSON body").with_params(serde_json::json!({
        "type": "object",
        "properties": { "url": {"type":"string"}, "method": {"type":"string"}, "body": {"type":"string"} },
        "required": ["url"]
    })));

    // Memory
    registry.register(
        Tool::new(
            "memory_search",
            Risk::ReadOnly,
            "Search stored long-term memory",
        )
        .with_params(string_param("query")),
    );
    registry.register(
        Tool::new("memory_get", Risk::ReadOnly, "Read a memory entry by key")
            .with_params(string_param("key")),
    );
    registry.register(Tool::new("memory_set", Risk::Mutating, "Store a memory entry (upsert by key)").with_params(serde_json::json!({
        "type": "object",
        "properties": { "key": {"type":"string"}, "value": {"type":"string"}, "tags": {"type":"array", "items": {"type":"string"}} },
        "required": ["key","value"]
    })));

    // Skills — progressive disclosure: the index is in the system prompt,
    // the full text is one read away.
    registry.register(
        Tool::new(
            "skill_list",
            Risk::ReadOnly,
            "List installed skills with their descriptions",
        )
        .with_params(serde_json::json!({ "type": "object", "properties": {} })),
    );
    registry.register(
        Tool::new(
            "skill_read",
            Risk::ReadOnly,
            "Read a skill's full instructions",
        )
        .with_params(string_param("name")),
    );

    // Tasks — project-scoped markdown files.
    registry.register(
        Tool::new("task_list", Risk::ReadOnly, "List tasks with their status")
            .with_params(serde_json::json!({ "type": "object", "properties": {} })),
    );
    registry.register(Tool::new("task_add", Risk::Mutating, "Create a task").with_params(serde_json::json!({
        "type": "object",
        "properties": { "slug": {"type":"string"}, "title": {"type":"string"}, "body": {"type":"string"} },
        "required": ["slug","title"]
    })));
    registry.register(
        Tool::new(
            "task_update",
            Risk::Mutating,
            "Change a task's status (pending, in-progress, done, deferred)",
        )
        .with_params(serde_json::json!({
            "type": "object",
            "properties": { "slug": {"type":"string"}, "status": {"type":"string"} },
            "required": ["slug","status"]
        })),
    );

    // Multi-agent
    registry.register(
        Tool::new(
            "sessions_spawn",
            Risk::Mutating,
            "Spawn a sub-agent session",
        )
        .with_params(serde_json::json!({
            "type": "object",
            "properties": { "task": {"type":"string"}, "agent": {"type":"string"} },
            "required": ["task"]
        })),
    );
    registry.register(
        Tool::new(
            "sessions_send",
            Risk::Mutating,
            "Send a message to a session",
        )
        .with_params(serde_json::json!({
            "type": "object",
            "properties": { "session": {"type":"string"}, "message": {"type":"string"} },
            "required": ["session","message"]
        })),
    );
    registry.register(
        Tool::new("sessions_steer", Risk::Mutating, "Steer a running session").with_params(
            serde_json::json!({
                "type": "object",
                "properties": { "session": {"type":"string"}, "instruction": {"type":"string"} },
                "required": ["session","instruction"]
            }),
        ),
    );

    // Scheduling
    registry.register(
        Tool::new("cron_add", Risk::Mutating, "Schedule a recurring job").with_params(
            serde_json::json!({
                "type": "object",
                "properties": { "expr": {"type":"string"}, "task": {"type":"string"} },
                "required": ["expr","task"]
            }),
        ),
    );
    registry.register(
        Tool::new("cron_remove", Risk::Destructive, "Remove a scheduled job")
            .with_params(string_param("id")),
    );

    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_all_blocks_everything_including_reads() {
        let registry = builtin_tools();
        let gate = registry.gate("read_file", Policy::DenyAll, &Overrides::new());
        assert_eq!(gate.decision, Decision::Deny);
    }

    #[test]
    fn allow_read_only_permits_reads_and_blocks_writes() {
        let registry = builtin_tools();
        let overrides = Overrides::new();
        assert_eq!(
            registry
                .gate("read_file", Policy::AllowReadOnly, &overrides)
                .decision,
            Decision::Allow
        );
        assert_eq!(
            registry
                .gate("write_file", Policy::AllowReadOnly, &overrides)
                .decision,
            Decision::Deny
        );
        assert_eq!(
            registry
                .gate("execute_command", Policy::AllowReadOnly, &overrides)
                .decision,
            Decision::Deny
        );
    }

    #[test]
    fn allow_mutating_still_blocks_destructive() {
        // The whole reason for three levels rather than two.
        let registry = builtin_tools();
        let overrides = Overrides::new();
        assert_eq!(
            registry
                .gate("write_file", Policy::AllowMutating, &overrides)
                .decision,
            Decision::Allow
        );
        assert_eq!(
            registry
                .gate("execute_command", Policy::AllowMutating, &overrides)
                .decision,
            Decision::Deny
        );
        assert_eq!(
            registry
                .gate("cron_remove", Policy::AllowMutating, &overrides)
                .decision,
            Decision::Deny
        );
    }

    #[test]
    fn allow_all_permits_everything() {
        let registry = builtin_tools();
        for tool in registry.names() {
            assert_eq!(
                registry
                    .gate(tool, Policy::AllowAll, &Overrides::new())
                    .decision,
                Decision::Allow
            );
        }
    }

    #[test]
    fn an_unknown_tool_is_denied_not_ignored() {
        // Ignoring it would let an agent believe a write happened.
        let gate = builtin_tools().gate("no_such_tool", Policy::AllowAll, &Overrides::new());
        assert_eq!(gate.decision, Decision::Deny);
        assert!(gate.reason.contains("no tool named"));
    }

    #[test]
    fn an_override_beats_the_class_policy() {
        let registry = builtin_tools();
        let overrides = Overrides::parse("read_file=allow");
        // deny-all, but this one tool is granted.
        assert_eq!(
            registry
                .gate("read_file", Policy::DenyAll, &overrides)
                .decision,
            Decision::Allow
        );
        assert_eq!(
            registry
                .gate("list_directory", Policy::DenyAll, &overrides)
                .decision,
            Decision::Deny
        );
    }

    #[test]
    fn an_override_can_deny_one_tool_under_allow_all() {
        let registry = builtin_tools();
        let overrides = Overrides::parse("execute_command=deny");
        assert_eq!(
            registry
                .gate("execute_command", Policy::AllowAll, &overrides)
                .decision,
            Decision::Deny
        );
        assert_eq!(
            registry
                .gate("write_file", Policy::AllowAll, &overrides)
                .decision,
            Decision::Allow
        );
    }

    #[test]
    fn an_ask_override_surfaces_for_a_human() {
        let registry = builtin_tools();
        let overrides = Overrides::parse("write_file=ask");
        assert_eq!(
            registry
                .gate("write_file", Policy::AllowAll, &overrides)
                .decision,
            Decision::Ask
        );
    }

    #[test]
    fn an_unrecognised_override_value_denies() {
        // A typo must never widen permissions.
        let overrides = Overrides::parse("write_file=allowl");
        assert_eq!(overrides.get("write_file"), Some(Decision::Deny));
    }

    #[test]
    fn an_unrecognised_policy_is_the_safest_reading() {
        assert_eq!(Policy::parse("allow-everything"), Policy::DenyAll);
        assert_eq!(Risk::parse("scary"), Risk::Destructive);
    }

    #[test]
    fn risk_levels_are_ordered_by_damage() {
        assert!(Risk::ReadOnly < Risk::Mutating);
        assert!(Risk::Mutating < Risk::Destructive);
    }

    #[test]
    fn the_builtin_set_covers_every_category() {
        let registry = builtin_tools();
        assert!(
            registry.len() >= 17,
            "expected the full tool set, got {}",
            registry.len()
        );
        assert!(!registry.by_risk(Risk::ReadOnly).is_empty());
        assert!(!registry.by_risk(Risk::Mutating).is_empty());
        assert!(!registry.by_risk(Risk::Destructive).is_empty());
    }

    #[test]
    fn execute_command_is_destructive_because_a_shell_can_delete() {
        let registry = builtin_tools();
        let tool = registry.get("execute_command").unwrap();
        assert_eq!(
            tool.risk,
            Risk::Destructive,
            "a shell is irreversible by nature"
        );
    }

    #[test]
    fn every_tool_has_a_description_for_the_model() {
        for tool in builtin_tools().all() {
            assert!(
                !tool.description.is_empty(),
                "{} needs a description",
                tool.name
            );
            assert!(
                !tool.parameters.is_null(),
                "{} needs a parameter schema",
                tool.name
            );
        }
    }

    #[test]
    fn specs_carry_the_name_and_schema_to_the_model() {
        let registry = builtin_tools();
        let spec = registry.get("read_file").unwrap().to_spec();
        assert_eq!(spec.name, "read_file");
        assert_eq!(spec.parameters["properties"]["path"]["type"], "string");
    }

    #[test]
    fn the_denial_reason_explains_itself() {
        let gate =
            builtin_tools().gate("execute_command", Policy::AllowReadOnly, &Overrides::new());
        // An operator reading a denial must learn what to change.
        assert!(gate.reason.contains("destructive"));
        assert!(gate.reason.contains("allow-read-only"));
    }
}
