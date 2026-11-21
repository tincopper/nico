//! The credentials one mock instance demands, and the two halves that enforce them.
//!
//! Real etcd authentication is a token exchange, not a header on every call: a client presents a
//! user name and password to `Authenticate`, receives a token, and carries that token as
//! `authorization` on every later request. `etcd-client` implements exactly that, which is why a
//! mock that only inspected an `authorization: Basic ...` header would refuse a *correct* password
//! — its `Authenticate` call carries no such header. The mock therefore speaks the same exchange:
//!
//! - **minting** — `MockAuth::authenticate` answers with a token only when the presented pair is the
//!   configured one; anything else is `unauthenticated`, which the client maps to `EtcdError::Auth`;
//! - **checking** — [`interceptor`] is the token check in front of the KV and Watch services, and it
//!   answers `unauthenticated` for a missing or wrong token, so a client that skipped the exchange
//!   never reaches the store.
//!
//! The token is derived from the credential pair, so it can only be minted by presenting the exact
//! password and it never carries one. A harness does not need an unguessable secret; it needs the
//! token to be proof that the exchange happened.

use std::{
    collections::hash_map::DefaultHasher,
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

use crate::client::proto::pb::etcdserverpb::{
    AuthDisableRequest, AuthDisableResponse, AuthEnableRequest, AuthEnableResponse,
    AuthRoleAddRequest, AuthRoleAddResponse, AuthRoleDeleteRequest, AuthRoleDeleteResponse,
    AuthRoleGetRequest, AuthRoleGetResponse, AuthRoleGrantPermissionRequest,
    AuthRoleGrantPermissionResponse, AuthRoleListRequest, AuthRoleListResponse,
    AuthRoleRevokePermissionRequest, AuthRoleRevokePermissionResponse, AuthUserAddRequest,
    AuthUserAddResponse, AuthUserChangePasswordRequest, AuthUserChangePasswordResponse,
    AuthUserDeleteRequest, AuthUserDeleteResponse, AuthUserGetRequest, AuthUserGetResponse,
    AuthUserGrantRoleRequest, AuthUserGrantRoleResponse, AuthUserListRequest, AuthUserListResponse,
    AuthUserRevokeRoleRequest, AuthUserRevokeRoleResponse, AuthenticateRequest,
    AuthenticateResponse, ResponseHeader, auth_server::Auth,
};

/// The header a client carries its token in.
const AUTHORIZATION: &str = "authorization";

/// The credential pair one mock instance requires, and the token it mints for that pair.
pub(super) struct RequiredAuth {
    user: String,
    password: String,
    token: String,
}

impl RequiredAuth {
    /// Records the pair and the token derived from it.
    pub(super) fn new(user: impl Into<String>, password: impl Into<String>) -> Self {
        let (user, password) = (user.into(), password.into());
        let token = mint(&user, &password);
        Self {
            user,
            password,
            token,
        }
    }

    /// Returns the token a client may present.
    fn token(&self) -> &str {
        &self.token
    }

    /// Reports whether the presented pair is the configured one.
    fn accepts(&self, user: &str, password: &str) -> bool {
        self.user == user && self.password == password
    }
}

impl fmt::Debug for RequiredAuth {
    /// Names the user and never the password: a credential in a log line is a credential leaked.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequiredAuth")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Derives the token a successful authentication answers with.
///
/// A `DefaultHasher` is enough: the token's job is to prove that the exchange happened with the
/// configured password, not to resist an attacker who already owns the test process.
fn mint(user: &str, password: &str) -> String {
    let mut hasher = DefaultHasher::new();
    user.hash(&mut hasher);
    password.hash(&mut hasher);
    format!("mock-token-{:016x}", hasher.finish())
}

/// The service that mints a token, and refuses one for any other pair.
#[derive(Debug)]
pub(super) struct MockAuth {
    pub(super) required: Option<Arc<RequiredAuth>>,
}

#[tonic::async_trait]
impl Auth for MockAuth {
    async fn authenticate(
        &self,
        request: tonic::Request<AuthenticateRequest>,
    ) -> Result<tonic::Response<AuthenticateResponse>, tonic::Status> {
        let Some(required) = self.required.as_deref() else {
            return Err(tonic::Status::unimplemented(
                "the mock etcd requires no authentication",
            ));
        };
        let request = request.into_inner();
        if !required.accepts(&request.name, &request.password) {
            return Err(tonic::Status::unauthenticated(
                "the credentials are not accepted",
            ));
        }
        Ok(tonic::Response::new(AuthenticateResponse {
            header: Some(ResponseHeader::default()),
            token: required.token().to_owned(),
        }))
    }

    // Authentication is the one RPC the mock serves. The user and role administration a real cluster
    // also exposes is refused loudly rather than silently succeeding: a test that reached one of
    // these would be a test of something this harness does not model.
    async fn auth_enable(
        &self,
        _request: tonic::Request<AuthEnableRequest>,
    ) -> Result<tonic::Response<AuthEnableResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn auth_disable(
        &self,
        _request: tonic::Request<AuthDisableRequest>,
    ) -> Result<tonic::Response<AuthDisableResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn user_add(
        &self,
        _request: tonic::Request<AuthUserAddRequest>,
    ) -> Result<tonic::Response<AuthUserAddResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn user_get(
        &self,
        _request: tonic::Request<AuthUserGetRequest>,
    ) -> Result<tonic::Response<AuthUserGetResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn user_list(
        &self,
        _request: tonic::Request<AuthUserListRequest>,
    ) -> Result<tonic::Response<AuthUserListResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn user_delete(
        &self,
        _request: tonic::Request<AuthUserDeleteRequest>,
    ) -> Result<tonic::Response<AuthUserDeleteResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn user_change_password(
        &self,
        _request: tonic::Request<AuthUserChangePasswordRequest>,
    ) -> Result<tonic::Response<AuthUserChangePasswordResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn user_grant_role(
        &self,
        _request: tonic::Request<AuthUserGrantRoleRequest>,
    ) -> Result<tonic::Response<AuthUserGrantRoleResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn user_revoke_role(
        &self,
        _request: tonic::Request<AuthUserRevokeRoleRequest>,
    ) -> Result<tonic::Response<AuthUserRevokeRoleResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn role_add(
        &self,
        _request: tonic::Request<AuthRoleAddRequest>,
    ) -> Result<tonic::Response<AuthRoleAddResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn role_get(
        &self,
        _request: tonic::Request<AuthRoleGetRequest>,
    ) -> Result<tonic::Response<AuthRoleGetResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn role_list(
        &self,
        _request: tonic::Request<AuthRoleListRequest>,
    ) -> Result<tonic::Response<AuthRoleListResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn role_delete(
        &self,
        _request: tonic::Request<AuthRoleDeleteRequest>,
    ) -> Result<tonic::Response<AuthRoleDeleteResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn role_grant_permission(
        &self,
        _request: tonic::Request<AuthRoleGrantPermissionRequest>,
    ) -> Result<tonic::Response<AuthRoleGrantPermissionResponse>, tonic::Status> {
        unimplemented_auth()
    }

    async fn role_revoke_permission(
        &self,
        _request: tonic::Request<AuthRoleRevokePermissionRequest>,
    ) -> Result<tonic::Response<AuthRoleRevokePermissionResponse>, tonic::Status> {
        unimplemented_auth()
    }
}

/// The refusal every auth RPC but `Authenticate` answers with.
// A gRPC handler's error is tonic's `Status` — the generated service contract's own error type,
// which tonic itself never boxes — so the size is the wire's, not a choice made here.
#[expect(
    clippy::result_large_err,
    reason = "a gRPC handler answers with tonic's status, the generated contract's own error type"
)]
fn unimplemented_auth<T>() -> Result<tonic::Response<T>, tonic::Status> {
    Err(tonic::Status::unimplemented(
        "the mock etcd only serves authenticate",
    ))
}

/// Builds the token check that sits in front of the KV and Watch services.
///
/// A server started without credentials accepts every request: the interceptor is only as strict as
/// the instance it belongs to.
#[expect(
    clippy::result_large_err,
    reason = "an `Interceptor` answers with tonic's status; boxing it would not be the trait"
)]
pub(super) fn interceptor(
    required: Option<Arc<RequiredAuth>>,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |request| {
        let Some(required) = required.as_deref() else {
            return Ok(request);
        };
        let presented = request
            .metadata()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if presented == Some(required.token()) {
            Ok(request)
        } else {
            Err(tonic::Status::unauthenticated(
                "the request presents no accepted token",
            ))
        }
    }
}
