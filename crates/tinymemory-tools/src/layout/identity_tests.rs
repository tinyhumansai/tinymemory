//! Identity pin precedence and plain namespace identity helpers.

use super::*;

#[test]
fn host_pins_precede_definition_team_and_default_values() {
    let identity = Identity::team_member("team", "writer");
    let mut pins = IdentityPins {
        agent_id: Some(" host-memory ".to_string()),
        root: Some(" project:host ".to_string()),
        recall: true,
        v3: true,
        ..IdentityPins::default()
    };
    pins.agents.insert(
        "writer".to_string(),
        AgentPin {
            agent_id: Some("definition-memory".to_string()),
            root: Some("project:definition".to_string()),
            recall: Some(false),
        },
    );

    let resolved = identity.resolve(&pins);

    assert_eq!(resolved.agent_id, "host-memory");
    assert_eq!(resolved.layout.root().to_string(), "project:host");
    assert_eq!(resolved.root(), &ns("project:host"));
    assert!(!resolved.recall, "definition recall overrides the host pin");
    let chat_node = ns("project:host/ws:main");
    assert_eq!(
        resolved.layout.pooled_conversations_node(),
        Some(&chat_node)
    );
    assert_eq!(
        resolved.layout.conversations("host-memory").unwrap(),
        chat_node
    );
}

#[test]
fn definition_pins_precede_team_and_definition_defaults() {
    let identity = Identity::team_member("acme", "writer");
    let mut pins = IdentityPins {
        recall: true,
        ..IdentityPins::default()
    };
    pins.agents.insert(
        "writer".to_string(),
        AgentPin {
            agent_id: Some("writer-memory".to_string()),
            root: Some(" team:private ".to_string()),
            recall: None,
        },
    );

    let resolved = identity.resolve(&pins);

    assert_eq!(resolved.agent_id, "writer-memory");
    assert_eq!(resolved.layout.root(), &ns("team:private"));
    assert!(resolved.recall);
}

#[test]
fn team_and_default_identity_fallbacks_are_preserved() {
    let team = Identity::team_member("acme", "writer").resolve(&IdentityPins::default());
    assert_eq!(team.agent_id, "writer");
    assert_eq!(team.layout.root(), &ns("team:acme"));

    let root = Identity::root().resolve(&IdentityPins::default());
    assert_eq!(root.agent_id, DEFAULT_AGENT_ID);
    assert_eq!(root.layout.root(), &Namespace::ROOT);

    let blank = Identity::agent("  ").resolve(&IdentityPins::default());
    assert_eq!(blank.agent_id, DEFAULT_AGENT_ID);
}

#[test]
fn child_identity_keeps_its_parent_team() {
    let parent = Identity::team_member("acme", "lead");
    let child = parent.child("scout").resolve(&IdentityPins::default());

    assert_eq!(child.agent_id, "scout");
    assert_eq!(child.root(), &ns("team:acme"));
}

#[test]
fn recall_defaults_to_enabled_like_the_host_config() {
    assert!(Identity::root().resolve(&IdentityPins::default()).recall);
}

#[test]
fn invalid_or_blank_roots_fall_through_to_the_next_pin() {
    let identity = Identity::team_member("acme", "writer");
    let mut pins = IdentityPins {
        root: Some("   ".to_string()),
        ..IdentityPins::default()
    };
    pins.agents.insert(
        "writer".to_string(),
        AgentPin {
            root: Some("not a namespace".to_string()),
            ..AgentPin::default()
        },
    );
    assert_eq!(identity.resolve(&pins).layout.root(), &ns("team:acme"));

    pins.agents.get_mut("writer").unwrap().root = Some(" team:private ".to_string());
    assert_eq!(identity.resolve(&pins).layout.root(), &ns("team:private"));
}

#[test]
fn chat_node_and_pooling_use_the_same_absolute_namespace() {
    let mut pins = IdentityPins {
        v3: true,
        ..IdentityPins::default()
    };
    pins.root = Some("team:acme".to_string());
    let resolved = Identity::agent("writer").resolve(&pins);
    let expected = ns("team:acme/ws:main");

    assert_eq!(chat_node(&resolved.layout), expected);
    assert_eq!(resolved.layout.pooled_conversations_node(), Some(&expected));
    assert_eq!(resolved.layout.conversations("writer").unwrap(), expected);
}

#[test]
fn validates_trimmed_roots_and_rejects_invalid_or_too_deep_roots() {
    assert_eq!(validate_root(" team:acme "), Ok(()));
    assert!(validate_root("not a namespace").is_err());
    let deep = ["team:a"; 8].join("/");
    assert!(validate_root(&deep).is_err());
}

#[test]
fn account_and_actor_roots_keep_org_user_spellings() {
    let account = "ABCDEF0123456789ABCDEF01";
    assert_eq!(
        account_root(account).as_deref(),
        Some("org:abcdef0123456789abcdef01")
    );
    assert_eq!(account_root("short"), None);
    assert_eq!(org_of_recorded("user:local-123"), "org:local-123");
    assert_eq!(org_of_recorded("org:already"), "org:already");
    assert_eq!(actor_of_root("org:local-123"), "user:local-123");
    assert_eq!(actor_of_root("user:local-123"), "user:local-123");
}

fn ns(value: &str) -> Namespace {
    value.parse().unwrap()
}
