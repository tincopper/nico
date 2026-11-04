//! Stage ordering and the final document construction.
//!
//! [`assemble`] is the only place the stages are sequenced, and the order is part of the contract:
//!
//! 1. the document envelope — `apiVersion` and the `limits` ranges;
//! 2. the typed entries — raw provider, upstream and route values become model values;
//! 3. reference validation ([`super::reference`]) — exactly one target per route, unique ids, every
//!    reference resolves;
//! 4. semantic validation ([`super::semantic`]) — names, hosts, paths, ports and value ranges;
//! 5. collection count bounds from [`ParseLimits::schema`];
//! 6. construction, which is only reachable when everything above passed.
//!
//! Count bounds run last on purpose. A per-entry defect is more actionable than "too many routes",
//! so a document that has both reports the defect; each stage stops at its first violation, which
//! keeps the error a caller sees stable from one release to the next.

use super::{
    bounds::{Bound, ParseLimits},
    document::{API_VERSION, Document},
    error::DocumentError,
    limits::Limits,
    provider::Provider,
    raw::RawDocument,
    reference, semantic,
    upstream::Upstream,
};

/// Assembles a [`Document`] from the raw shape, applying every validation stage in order.
///
/// # Errors
/// Returns the first violation in stage order:
/// - [`DocumentError::WrongApiVersion`] or [`DocumentError::LimitOutOfRange`] for the envelope;
/// - [`DocumentError::DuplicateId`], [`DocumentError::RouteTargetMissing`],
///   [`DocumentError::RouteTargetAmbiguous`] or [`DocumentError::ReferenceUnknown`] from reference
///   validation;
/// - [`DocumentError::InvalidField`] or [`DocumentError::LimitOutOfRange`] from semantic validation;
/// - [`DocumentError::TooMany`] when a collection exceeds its count bound.
pub(super) fn assemble(raw: RawDocument, limits: ParseLimits) -> Result<Document, DocumentError> {
    let RawDocument {
        api_version,
        limits: raw_limits,
        providers: raw_providers,
        upstreams: raw_upstreams,
        routes: raw_routes,
    } = raw;

    if api_version != API_VERSION {
        return Err(DocumentError::WrongApiVersion { found: api_version });
    }

    let document_limits = Limits::new(
        raw_limits.max_request_body_bytes,
        raw_limits.request_timeout_ms,
    );
    document_limits.validate()?;

    let providers = raw_providers
        .into_iter()
        .map(|raw| Provider {
            name: raw.name,
            kind: raw.kind,
            base_url: raw.base_url,
            credential_ref: raw.credential_ref,
            model_allowlist: raw.model_allowlist.unwrap_or_default(),
            timeout_ms: raw.timeout_ms,
        })
        .collect::<Vec<_>>();
    let upstreams = raw_upstreams
        .into_iter()
        .map(|raw| Upstream {
            name: raw.name,
            scheme: raw.scheme,
            host: raw.host,
            port: raw.port,
        })
        .collect::<Vec<_>>();

    let routes = reference::validate(raw_routes, &upstreams, &providers)?;
    semantic::validate(&routes, &upstreams, &providers, document_limits)?;

    let schema = limits.schema();
    check_count("providers", providers.len(), schema.get(Bound::Providers))?;
    check_count("upstreams", upstreams.len(), schema.get(Bound::Upstreams))?;
    check_count("routes", routes.len(), schema.get(Bound::Routes))?;

    Ok(Document::new(
        api_version,
        document_limits,
        providers,
        upstreams,
        routes,
    ))
}

/// Rejects a collection that holds more entries than its bound allows.
fn check_count(kind: &'static str, count: usize, limit: usize) -> Result<(), DocumentError> {
    if count > limit {
        return Err(DocumentError::TooMany { kind, count, limit });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{
            bounds::{Bound, SchemaBounds},
            test_support::{
                NO_ENTRIES, PROVIDER, SECOND_PROVIDER, SECOND_UPSTREAM, UPSTREAM, document,
            },
        },
        error::ErrorCategory,
    };

    fn parse(document: &str, schema: SchemaBounds) -> Result<Document, DocumentError> {
        Document::parse(
            document.as_bytes(),
            ParseLimits::new(16 * 1024).with_schema(schema),
        )
    }

    /// A valid route; `path_prefix` must differ between routes to stay semantically legal.
    fn route(id: &str, path_prefix: &str) -> String {
        format!(
            concat!(
                "  - id: {}\n",
                "    path_prefix: {}\n",
                "    upstream: api-backend\n",
            ),
            id, path_prefix
        )
    }

    mod counts {
        use super::*;

        #[test]
        fn routes_beyond_the_bound_are_rejected() {
            let routes = format!(
                "{}{}",
                route("api-one", "/api/one/"),
                route("api-two", "/api/two/")
            );
            let document = document(NO_ENTRIES, UPSTREAM, &routes);
            let schema = SchemaBounds::default()
                .with(Bound::Routes, 1)
                .expect("1 is a legal bound");
            let error = parse(&document, schema).expect_err("two routes, bound is one");
            assert!(
                matches!(
                    error,
                    DocumentError::TooMany {
                        kind: "routes",
                        count: 2,
                        limit: 1
                    }
                ),
                "expected TooMany for routes, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Limit);
        }

        #[test]
        fn upstreams_beyond_the_bound_are_rejected() {
            let upstreams = format!("{UPSTREAM}{SECOND_UPSTREAM}");
            let document = document(NO_ENTRIES, &upstreams, NO_ENTRIES);
            let schema = SchemaBounds::default()
                .with(Bound::Upstreams, 1)
                .expect("1 is a legal bound");
            let error = parse(&document, schema).expect_err("two upstreams, bound is one");
            assert!(
                matches!(
                    error,
                    DocumentError::TooMany {
                        kind: "upstreams",
                        count: 2,
                        limit: 1
                    }
                ),
                "expected TooMany for upstreams, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Limit);
        }

        #[test]
        fn providers_beyond_the_bound_are_rejected() {
            let providers = format!("{PROVIDER}{SECOND_PROVIDER}");
            let document = document(&providers, NO_ENTRIES, NO_ENTRIES);
            let schema = SchemaBounds::default()
                .with(Bound::Providers, 1)
                .expect("1 is a legal bound");
            let error = parse(&document, schema).expect_err("two providers, bound is one");
            assert!(
                matches!(
                    error,
                    DocumentError::TooMany {
                        kind: "providers",
                        count: 2,
                        limit: 1
                    }
                ),
                "expected TooMany for providers, got {error:?}"
            );
        }

        #[test]
        fn a_collection_at_the_bound_is_accepted() {
            let document = document(NO_ENTRIES, UPSTREAM, &route("api-one", "/api/"));
            let schema = SchemaBounds::default()
                .with(Bound::Routes, 1)
                .expect("1 is a legal bound");
            let document = parse(&document, schema).expect("one route, bound is one");
            assert_eq!(document.routes().len(), 1);
        }
    }

    mod ordering {
        use super::*;

        #[test]
        fn a_dangling_reference_is_reported_before_a_count_bound() {
            let routes = format!(
                "{}{}",
                route("api-one", "/api/one/"),
                concat!(
                    "  - id: api-two\n",
                    "    path_prefix: /api/two/\n",
                    "    upstream: missing-backend\n",
                )
            );
            let document = document(NO_ENTRIES, UPSTREAM, &routes);
            let schema = SchemaBounds::default()
                .with(Bound::Routes, 1)
                .expect("1 is a legal bound");

            let error = parse(&document, schema).expect_err("both a bad reference and too many");
            assert!(
                matches!(
                    error,
                    DocumentError::ReferenceUnknown {
                        kind: "upstream",
                        ref name
                    } if name == "missing-backend"
                ),
                "expected ReferenceUnknown before TooMany, got {error:?}"
            );
        }

        #[test]
        fn a_bad_field_is_reported_before_a_count_bound() {
            let routes = format!(
                "{}{}",
                route("api-one", "/api/one/"),
                concat!(
                    "  - id: api-two\n",
                    "    path_prefix: api/two/\n",
                    "    upstream: api-backend\n",
                )
            );
            let document = document(NO_ENTRIES, UPSTREAM, &routes);
            let schema = SchemaBounds::default()
                .with(Bound::Routes, 1)
                .expect("1 is a legal bound");

            let error = parse(&document, schema).expect_err("both a bad field and too many");
            assert!(
                matches!(
                    error,
                    DocumentError::InvalidField {
                        field: "routes[].path_prefix",
                        ..
                    }
                ),
                "expected InvalidField before TooMany, got {error:?}"
            );
        }
    }
}
