//! Tests for the contract itself: the stage defaults, the registry, and the published info.
//!
//! The phase matrix and the response cleaning have their own test modules next to the code they
//! exercise ([`context`](super::context) and [`response`](super::response)). What lives here is the
//! question a plugin implementor asks first: "what do I have to implement, and how does my kind get
//! loaded?"

use std::sync::Arc;

use async_trait::async_trait;
use nico_core::{ErrorCategory, PluginSpec};

use super::{
    Outcome, Plugin, PluginBuildError, PluginContext, PluginFactory, PluginInfo, PluginPhase,
    PluginRegistry, PluginResult, RegistryError,
};
use crate::{
    error::HttpError,
    plugin::context::Phase,
    test_support::{PLUGIN_ROUTE, PluginFixture, snapshot_with_plugins},
};

/// A plugin that overrides no stage: the proof that every stage has a default.
struct Silent;

#[async_trait]
impl Plugin for Silent {
    fn name(&self) -> &str {
        "silent"
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("1.2.3", "does nothing at all", r#"{"enabled":true}"#)
    }
}

/// A plugin that implements one stage, as a real kind does: only the stage it needs.
struct BodyOnly;

#[async_trait]
impl Plugin for BodyOnly {
    fn name(&self) -> &str {
        "body-only"
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("0.1.0", "observes the request body", "{}")
            .with_readme("Deliberately implements one stage.")
    }

    async fn on_request_body(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        let _body = ctx.request_body()?;
        Ok(Outcome::Continue)
    }
}

struct SilentFactory;

impl PluginFactory for SilentFactory {
    fn kind(&self) -> &'static str {
        "silent"
    }

    fn phase(&self) -> PluginPhase {
        PluginPhase::Other
    }

    fn default_config(&self) -> &'static str {
        "{}"
    }

    fn build(&self, _spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError> {
        Ok(Arc::new(Silent))
    }
}

struct FailingFactory;

impl PluginFactory for FailingFactory {
    fn kind(&self) -> &'static str {
        "failing"
    }

    fn phase(&self) -> PluginPhase {
        PluginPhase::Other
    }

    fn default_config(&self) -> &'static str {
        "{}"
    }

    fn build(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError> {
        Err(PluginBuildError::new(spec.name(), "unknown field `burst`"))
    }
}

/// Returns the document entry named `name`, as a factory receives it.
fn entry(name: &str) -> PluginSpec {
    let snapshot = snapshot_with_plugins(
        &format!("  - name: {name}\n    kind: silent\n"),
        PLUGIN_ROUTE,
    );
    snapshot
        .document()
        .plugins()
        .first()
        .cloned()
        .expect("the fixture declares the plugin")
}

#[tokio::test]
async fn a_plugin_that_implements_no_phase_continues_everywhere() {
    let plugin = Silent;
    let mut fixture = PluginFixture::started();
    let mut context = fixture.state(Phase::Request);

    assert!(matches!(
        plugin.on_request(&mut context).await,
        Ok(Outcome::Continue)
    ));
    assert!(matches!(
        plugin.on_request_body(&mut context).await,
        Ok(Outcome::Continue)
    ));
    assert!(matches!(
        plugin.on_response(&mut context).await,
        Ok(Outcome::Continue)
    ));
    assert!(matches!(
        plugin.on_response_body(&mut context).await,
        Ok(Outcome::Continue)
    ));
    plugin.on_logging(&mut context).await;
}

#[tokio::test]
async fn a_plugin_may_implement_only_the_phase_it_uses() {
    let plugin = BodyOnly;
    let mut fixture = PluginFixture::started();
    let mut context = fixture
        .state(Phase::RequestBody)
        .with_body(b"abc".to_vec(), true);

    assert!(matches!(
        plugin.on_request_body(&mut context).await,
        Ok(Outcome::Continue)
    ));

    // The stages it did not implement are the trait's defaults, so a stage the plugin did not ask for
    // still continues — it does not silently read a body it has no phase for.
    let mut fixture = PluginFixture::started();
    let mut request_stage = fixture.state(Phase::Request);
    assert!(matches!(
        plugin.on_request(&mut request_stage).await,
        Ok(Outcome::Continue)
    ));
}

#[test]
fn info_carries_the_three_facts_and_an_optional_readme() {
    let info = BodyOnly.info();

    assert_eq!(info.version(), "0.1.0");
    assert_eq!(info.description(), "observes the request body");
    assert_eq!(info.default_config(), "{}");
    assert_eq!(info.readme(), Some("Deliberately implements one stage."));
    assert_eq!(PluginInfo::new("1", "d", "{}").readme(), None);
}

#[test]
fn a_registered_kind_resolves_to_its_factory() {
    let mut registry = PluginRegistry::new();
    registry
        .register(Arc::new(SilentFactory))
        .expect("the first registration of a kind succeeds");

    let factory = registry.factory("silent").expect("the kind is registered");
    assert_eq!(factory.kind(), "silent");
    assert_eq!(factory.default_config(), "{}");
    assert_eq!(registry.len(), 1);
    assert!(!registry.is_empty());
}

#[test]
fn a_kind_is_matched_exactly_and_never_by_another_spelling() {
    let mut registry = PluginRegistry::new();
    registry
        .register(Arc::new(SilentFactory))
        .expect("a kind is registered");

    assert!(registry.factory("Silent").is_none());
    assert!(registry.factory("jwt").is_none());
}

#[test]
fn a_duplicate_kind_is_refused_rather_than_replaced() {
    let mut registry = PluginRegistry::new();
    registry
        .register(Arc::new(SilentFactory))
        .expect("the first registration succeeds");

    let error = registry
        .register(Arc::new(SilentFactory))
        .expect_err("a second factory for one kind is a configuration error");
    assert_eq!(
        error,
        RegistryError::DuplicateKind {
            kind: "silent".to_owned()
        }
    );
    assert_eq!(
        registry.len(),
        1,
        "the refused registration changed nothing"
    );
}

#[test]
fn an_empty_registry_answers_no_kind() {
    let registry = PluginRegistry::new();

    assert!(registry.factory("jwt").is_none());
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
}

#[test]
fn a_factory_builds_from_the_documents_own_entry() {
    let spec = entry("edge-firewall");

    let plugin = SilentFactory
        .build(&spec)
        .expect("the silent kind accepts any configuration");
    assert_eq!(plugin.name(), "silent");
    assert_eq!(plugin.info().version(), "1.2.3");
}

#[test]
fn a_build_failure_names_the_plugin_and_the_field() {
    let spec = entry("edge-firewall");

    let error = match FailingFactory.build(&spec) {
        Ok(_) => panic!("the failing kind refuses every entry"),
        Err(error) => error,
    };
    assert_eq!(error.plugin(), "edge-firewall");
    assert_eq!(error.reason(), "unknown field `burst`");
    assert_eq!(
        error.to_string(),
        "plugin `edge-firewall`: unknown field `burst`"
    );
}

#[test]
fn the_registry_prints_its_kinds_rather_than_its_factories() {
    let mut registry = PluginRegistry::new();
    registry
        .register(Arc::new(SilentFactory))
        .expect("a kind is registered");

    let rendered = format!("{registry:?}");
    assert!(rendered.contains("silent"), "got {rendered}");
}

#[test]
fn a_kind_declares_the_class_it_runs_in() {
    // The ordering a document cannot express: authentication is a class, and a limit belongs after
    // it whatever order the document wrote.
    assert_eq!(SilentFactory.phase(), PluginPhase::Other);
    assert!(PluginPhase::Authenticate < PluginPhase::Limit);
    assert!(PluginPhase::Limit < PluginPhase::Other);
}

#[test]
fn a_build_failure_is_ours_unless_the_factory_says_otherwise() {
    let unclassified = PluginBuildError::new("edge-firewall", "no module was loaded");
    assert_eq!(
        unclassified.category(),
        ErrorCategory::Internal,
        "a factory that cannot classify its own failure is reporting its own defect"
    );

    let refused_field = PluginBuildError::new("edge-firewall", "unknown field `burst`")
        .with_category(ErrorCategory::Client);
    assert_eq!(refused_field.category(), ErrorCategory::Client);
    assert_eq!(refused_field.reason(), "unknown field `burst`");
    assert_eq!(refused_field.plugin(), "edge-firewall");
}

#[test]
fn every_error_type_crosses_an_await_and_a_spawn_boundary() {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}

    assert_send_sync_static::<super::PluginError>();
    assert_send_sync_static::<super::ResponseError>();
    assert_send_sync_static::<PluginBuildError>();
    assert_send_sync_static::<RegistryError>();
    assert_send_sync_static::<HttpError>();
}
