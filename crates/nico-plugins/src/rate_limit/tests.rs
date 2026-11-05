//! Tests for the rate-limit plugin: which bucket a request is charged to, what a refusal carries,
//! and what a reload does to the buckets.
//!
//! The clock is paused, so a refill is driven by `advance` rather than by sleeping. Every test builds
//! its context from the fixture's route id, because the route is part of the bucket key.

use std::{sync::Arc, time::Duration};

use nico_core::{Document, ErrorCategory};
use nico_http::{HttpError, Outcome, Plugin, PluginFactory, PluginRegistry};
use tokio::time::advance;

use crate::{
    PluginRuntime,
    rate_limit::{Limiter, RateLimitConfig, RateLimitFactory, RateLimitPlugin},
    spec::RateLimitSpec,
    test_support::{ROUTE, SECOND_ROUTE, TestContext, document, plugin},
};

/// A `rate_limit` entry named `name`: `burst` requests, refilled at `per_second`.
fn entry_named(name: &str, burst: u32, per_second: u32) -> String {
    format!(
        concat!(
            "  - name: {name}\n",
            "    kind: rate_limit\n",
            "    config:\n",
            "      burst: {burst}\n",
            "      per_second: {per_second}\n",
        ),
        name = name,
        burst = burst,
        per_second = per_second
    )
}

/// The default entry: `burst` requests, refilled at `per_second`, named `quota`.
fn entry(burst: u32, per_second: u32) -> String {
    entry_named("quota", burst, per_second)
}

/// Compiles `name`'s entry of an already-parsed document.
fn spec_in(document: &Document, name: &str) -> RateLimitSpec {
    let plugin = plugin(document, name);
    let config = RateLimitConfig::from_spec(plugin).expect("the fixture configuration compiles");
    RateLimitSpec::compile(plugin, &config).expect("the fixture compiles")
}

/// Compiles the `quota` entry of a document holding `plugins`.
fn spec(plugins: &str) -> RateLimitSpec {
    spec_in(&document(plugins, &["quota"]), "quota")
}

/// Compiles `plugins`'s `quota` entry and builds the table it asks for.
///
/// The table is derived from the compiled parameters rather than invented, because that equality is
/// what lets a reload keep the buckets: a test that built its own numbers could not see the coupling.
fn compiled(plugins: &str) -> (Arc<Limiter>, RateLimitPlugin) {
    let spec = spec(plugins);
    let limiter = Arc::new(Limiter::new(spec.parameters()));
    let plugin = RateLimitPlugin::new(&spec, Arc::clone(&limiter));
    (limiter, plugin)
}

/// Runs the request stage, mapping a refusal the way the data plane does.
async fn call(plugin: &dyn Plugin, ctx: &mut TestContext) -> Result<(), HttpError> {
    match plugin.on_request(ctx).await {
        Ok(Outcome::Continue) => Ok(()),
        Ok(other) => panic!("the rate_limit kind never answers instead of forwarding: {other:?}"),
        Err(error) => Err(HttpError::from(error)),
    }
}

/// Builds a context for `route` whose caller is already authenticated as `subject`.
fn authenticated(route: &str, subject: &str) -> TestContext {
    TestContext::new("quota")
        .for_route(route)
        .with_identity(subject)
}

/// Builds a context for `route` whose caller hashes to `hash` and has no identity.
fn from_hash(route: &str, hash: u64) -> TestContext {
    TestContext::new("quota")
        .for_route(route)
        .with_client_ip_hash(hash)
}

mod buckets {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_burst_is_served_and_the_next_request_is_refused_with_a_wait() {
        let (_table, filter) = compiled(&entry(2, 1));
        let mut ctx = authenticated(ROUTE, "alice");

        assert!(call(&filter, &mut ctx).await.is_ok());
        assert!(call(&filter, &mut ctx).await.is_ok());

        let error = call(&filter, &mut ctx)
            .await
            .expect_err("the burst is spent");
        let HttpError::Rejected {
            rejection,
            detail,
            retry_after,
        } = error
        else {
            panic!("expected a compiled rejection, got {error:?}");
        };
        assert_eq!(retry_after, Some(Duration::from_secs(1)));
        assert_eq!(rejection.status().as_u16(), 429);
        assert_eq!(rejection.code(), "rate_limited");
        assert_eq!(rejection.challenge(), None);
        assert!(
            detail.contains("rate limit"),
            "the log needs a reason: {detail}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refill_after_the_wait_serves_the_next_request() {
        let (_table, filter) = compiled(&entry(1, 1));
        let mut ctx = authenticated(ROUTE, "alice");
        assert!(call(&filter, &mut ctx).await.is_ok());

        let error = call(&filter, &mut ctx).await.expect_err("spent");
        let HttpError::Rejected {
            retry_after: Some(wait),
            ..
        } = error
        else {
            panic!("a limit always knows when to come back: {error:?}");
        };

        advance(wait).await;
        assert!(
            call(&filter, &mut ctx).await.is_ok(),
            "waiting exactly `Retry-After` must be enough"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn subjects_are_charged_separately() {
        let (_table, filter) = compiled(&entry(1, 1));
        let mut alice = authenticated(ROUTE, "alice");
        let mut bob = authenticated(ROUTE, "bob");

        assert!(call(&filter, &mut alice).await.is_ok());
        assert!(call(&filter, &mut alice).await.is_err());
        assert!(
            call(&filter, &mut bob).await.is_ok(),
            "one subject's burst must not charge another's"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_subject_on_two_routes_gets_two_buckets() {
        let (_table, filter) = compiled(&entry(1, 1));
        let mut first = authenticated(ROUTE, "alice");
        let mut second = authenticated(SECOND_ROUTE, "alice");

        assert!(call(&filter, &mut first).await.is_ok());
        assert!(call(&filter, &mut first).await.is_err());
        assert!(
            call(&filter, &mut second).await.is_ok(),
            "the route is part of the key, so a quota is per route"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn two_rate_limit_instances_sharing_a_table_are_charged_separately() {
        // Two instances of the same kind with the same numbers, over one table: the instance name is
        // part of the digest, so a subject's burst in one instance is not spent in the other.
        let document = document(
            &format!("{}{}", entry(1, 1), entry_named("quota-b", 1, 1)),
            &["quota"],
        );
        let first = spec_in(&document, "quota");
        let second = spec_in(&document, "quota-b");
        let table = Arc::new(Limiter::new(first.parameters()));
        let first = RateLimitPlugin::new(&first, Arc::clone(&table));
        let second = RateLimitPlugin::new(&second, table);

        let mut ctx = authenticated(ROUTE, "alice");
        assert!(call(&first, &mut ctx).await.is_ok());
        assert!(call(&first, &mut ctx).await.is_err());
        assert!(
            call(&second, &mut ctx).await.is_ok(),
            "another instance's quota is another bucket"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_without_an_identity_falls_back_to_the_client_address() {
        let (_table, filter) = compiled(&entry(1, 1));
        let mut first = from_hash(ROUTE, 1);
        let mut second = from_hash(ROUTE, 2);

        assert!(
            call(&filter, &mut first).await.is_ok(),
            "an unauthenticated caller is still limited"
        );
        assert!(call(&filter, &mut first).await.is_err());
        assert!(
            call(&filter, &mut second).await.is_ok(),
            "two addresses are two callers"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_entry_that_keys_on_the_address_ignores_the_identity() {
        let (_table, filter) = compiled(&format!("{}      identity: ip\n", entry(1, 1)));
        let mut alice = from_hash(ROUTE, 1).with_identity("alice");
        let mut bob = from_hash(ROUTE, 1).with_identity("bob");

        assert!(call(&filter, &mut alice).await.is_ok());
        assert!(
            call(&filter, &mut bob).await.is_err(),
            "`identity: ip` charges the connection, whatever the subject is"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_without_a_peer_address_shares_one_bucket() {
        let (_table, filter) = compiled(&entry(1, 1));
        let mut first = TestContext::new("quota").for_route(ROUTE);
        let mut second = TestContext::new("quota").for_route(ROUTE);

        assert!(call(&filter, &mut first).await.is_ok());
        assert!(
            call(&filter, &mut second).await.is_err(),
            "with no identity and no address every request shares the route's bucket, which \
             over-limits rather than bypasses"
        );
    }
}

mod reloads {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn unchanged_parameters_keep_the_buckets() {
        let (limiter, filter) = compiled(&entry(1, 1));
        let mut ctx = authenticated(ROUTE, "alice");
        assert!(call(&filter, &mut ctx).await.is_ok());

        // A new generation compiles the same entry again; the compiler keeps the limiter because the
        // parameters did not change.
        let after = spec(&entry(1, 1));
        assert_eq!(
            after.parameters(),
            limiter.parameters(),
            "equal parameters are what lets the compiler reuse the table"
        );
        let rebuilt = RateLimitPlugin::new(&after, limiter);

        assert!(
            call(&rebuilt, &mut ctx).await.is_err(),
            "a reload must not hand every caller a fresh quota"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn changed_parameters_start_new_buckets() {
        let (_table, filter) = compiled(&entry(1, 1));
        let mut ctx = authenticated(ROUTE, "alice");
        assert!(call(&filter, &mut ctx).await.is_ok());

        let (_, rebuilt) = compiled(&entry(2, 1));
        assert!(
            call(&rebuilt, &mut ctx).await.is_ok(),
            "a changed quota is a new budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_refusal_carries_the_configured_status_and_keeps_the_limit_category() {
        let (_table, filter) = compiled(&format!(
            "{}{}",
            entry(1, 1),
            "    error_response:\n      status: 403\n"
        ));
        let mut ctx = authenticated(ROUTE, "alice");
        assert!(call(&filter, &mut ctx).await.is_ok());

        let error = call(&filter, &mut ctx).await.expect_err("spent");
        assert_eq!(error.category(), ErrorCategory::Limit);
        assert_eq!(
            error.respond().map(|respond| respond.status().as_u16()),
            Some(403)
        );
    }
}

/// The table a second publication must find again — and must not keep forever.
mod tables {
    use super::*;

    /// Builds `plugins`'s `quota` entry through `factory`, as a publication does.
    fn built(factory: &RateLimitFactory, plugins: &str) -> Arc<dyn Plugin> {
        let document = document(plugins, &["quota"]);
        factory
            .build(plugin(&document, "quota"))
            .expect("the fixture builds")
    }

    #[tokio::test(start_paused = true)]
    async fn a_reload_reuses_the_buckets_of_an_unchanged_quota() {
        let factory = RateLimitFactory::new();
        let first = built(&factory, &entry(1, 1));
        let mut ctx = authenticated(ROUTE, "alice");
        assert!(call(first.as_ref(), &mut ctx).await.is_ok());
        assert!(call(first.as_ref(), &mut ctx).await.is_err());

        let rebuilt = built(&factory, &entry(1, 1));
        assert!(
            call(rebuilt.as_ref(), &mut ctx).await.is_err(),
            "the factory must hand the rebuilt instance the table the old generation is charging"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_changed_quota_starts_over() {
        let factory = RateLimitFactory::new();
        let first = built(&factory, &entry(1, 1));
        let mut ctx = authenticated(ROUTE, "alice");
        assert!(call(first.as_ref(), &mut ctx).await.is_ok());
        assert!(call(first.as_ref(), &mut ctx).await.is_err());

        let changed = built(&factory, &entry(2, 1));
        assert!(
            call(changed.as_ref(), &mut ctx).await.is_ok(),
            "a changed quota is a new budget"
        );
    }

    #[test]
    fn a_table_no_generation_holds_is_released() {
        let factory = RateLimitFactory::new();
        let instance = built(&factory, &entry(1, 1));
        let table = factory
            .current("quota")
            .expect("the build registered a table");
        assert!(
            Arc::ptr_eq(
                &table,
                &factory
                    .current("quota")
                    .expect("the table is still in service")
            ),
            "the record points at the table the instance holds"
        );

        drop(instance);
        drop(table);
        assert!(
            factory.current("quota").is_none(),
            "a table no published plugin can reach must not outlive its generation"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_publication_keeps_the_published_table() {
        // The registry holds the one kind under test, so the second document fails on its own
        // configuration rather than on a kind this build does not have.
        let factory = Arc::new(RateLimitFactory::new());
        let mut registry = PluginRegistry::new();
        registry
            .register(Arc::clone(&factory) as Arc<dyn PluginFactory>)
            .expect("one kind is registered once");
        let runtime = PluginRuntime::with_registry(registry);

        let published = document(&entry(1, 1), &["quota"]);
        let plan = runtime
            .compile(&published)
            .expect("the first document compiles");
        let table = factory
            .current("quota")
            .expect("the first document registered a table");

        // The first entry rebuilds the live table; the second cannot be built at all, so the plan is
        // refused. The generation in service must keep the quota it was charging.
        let refused = document(
            &format!("{}{}", entry(1, 1), entry_named("quota-broken", 0, 1)),
            &["quota"],
        );
        assert!(
            runtime.compile(&refused).is_err(),
            "a zero burst is refused at publication"
        );

        let kept = factory
            .current("quota")
            .expect("the published table is still registered");
        assert!(
            Arc::ptr_eq(&kept, &table),
            "a refused document must not reset a live quota"
        );
        drop(plan);
    }
}
