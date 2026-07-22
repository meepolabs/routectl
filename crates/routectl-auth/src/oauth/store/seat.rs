//! Seat bookkeeping + SecretStore adapter.

use async_trait::async_trait;

use routectl_core::{Error, Result};

use crate::oauth::providers;
use crate::oauth::types::{seat_key, unix_now};
use crate::{SecretRef, SecretStore};

use super::{OAuthStore, REFRESH_LEAD_SECS};

#[async_trait]
impl SecretStore for OAuthStore {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        // Validate provider is known. The lookup also gives operators
        // the authoritative "unknown oauth provider" message rather
        // than a silent miss. Validation keys on the provider id (the
        // flow registry is per-provider), independent of the seat.
        providers::lookup(provider).map_err(Error::from)?;

        // Resolve the credentials-map record by SEAT KEY: a bare ref
        // (label None) keys as the unlabeled provider record exactly as
        // before; a labeled ref keys this seat's record.
        let seat = seat_key(provider, label.as_deref());
        let rec = self.read_record(&seat).await.map_err(Error::from)?;

        if rec.near_expiry(REFRESH_LEAD_SECS, unix_now()) {
            tracing::debug!(
                provider = %provider,
                seat = %seat,
                expires_at_unix = rec.expires_at_unix,
                "oauth access token near expiry; entering refresh single-flight"
            );
            let refreshed = self
                .refresh_under_lock(provider, &seat, &rec, false, false)
                .await?;
            return Ok(refreshed.access_token.expose().to_string());
        }
        Ok(rec.access_token.expose().to_string())
    }

    async fn set(&self, _secret_ref: &SecretRef, _value: &str) -> Result<()> {
        // OAuth tokens are minted by `routectl login`, not by manual
        // assignment. Refuse loudly so a typo (e.g. config builder
        // calling `set` with a static string) does not silently
        // overwrite real credentials.
        Err(Error::Auth(
            "oauth tokens are managed via `routectl login <provider>`; \
             direct `set` is not supported"
                .into(),
        ))
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<()> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        // Delete targets only the named seat: a labeled ref removes that
        // seat's record and leaves sibling seats untouched; a bare ref
        // removes the unlabeled record exactly as before.
        self.remove_provider(&seat_key(provider, label.as_deref()))
            .await
            .map(|_| ())
            .map_err(Error::from)
    }

    async fn on_auth_failure(&self, secret_ref: &SecretRef) -> Result<()> {
        // The router calls this after an upstream 401 against a
        // credential resolved from this store. Force a refresh -- the
        // upstream said the access token is dead regardless of what
        // `expires_at_unix` claims (clock skew, server-side rotation,
        // revocation). The single-flight gate inside `force_refresh_seat`
        // collapses a 401 storm into one POST. Targets only the named
        // seat: a 401 on a labeled seat force-refreshes that seat's
        // record and leaves sibling seats untouched.
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        self.force_refresh_seat(provider, &seat_key(provider, label.as_deref()), false)
            .await
            .map(|_| ())
    }

    async fn account_id(&self, secret_ref: &SecretRef) -> Result<Option<String>> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        Ok(self
            .peek_account_id(&seat_key(provider, label.as_deref()))
            .await)
    }

    async fn peek_session_id(&self, secret_ref: &SecretRef) -> Option<String> {
        // Non-oauth refs carry no session metadata. Unlike `account_id`,
        // the trait signature returns `Option` (not `Result`), so a
        // non-oauth ref maps to `None` rather than an error -- the
        // caller treats "no session id" identically to "not an oauth
        // ref".
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            _ => return None,
        };
        Self::peek_session_id(self, &seat_key(provider, label.as_deref())).await
    }

    async fn peek_cloud_project_id(&self, secret_ref: &SecretRef) -> Option<String> {
        // Non-oauth refs carry no project-id metadata; map to None
        // rather than an error (same pattern as peek_session_id).
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            _ => return None,
        };
        Self::peek_cloud_project_id(self, &seat_key(provider, label.as_deref())).await
    }

    async fn set_cloud_project_id(&self, secret_ref: &SecretRef, project_id: &str) -> Result<()> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            _ => return Ok(()),
        };
        Self::set_cloud_project_id(self, &seat_key(provider, label.as_deref()), project_id)
            .await
            .map_err(Error::from)
    }

    async fn clear_cloud_project_id_if_matches(
        &self,
        secret_ref: &SecretRef,
        expected: &str,
    ) -> Result<bool> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            _ => return Ok(false),
        };
        Self::clear_cloud_project_id_if_matches(
            self,
            &seat_key(provider, label.as_deref()),
            expected,
        )
        .await
        .map_err(Error::from)
    }

    async fn list_seats(&self, secret_ref: &SecretRef) -> Result<Vec<SecretRef>> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            // Non-oauth refs are single-ref by definition; mirror the
            // trait default rather than erroring (the composite store
            // only routes oauth:// refs here, but a direct caller that
            // hands a non-oauth ref to OAuthStore should still get the
            // single-ref answer, not a hard failure).
            other => return Ok(vec![other.clone()]),
        };
        // A labeled ref pins one seat: the operator already selected it,
        // so enumeration returns just that ref.
        if label.is_some() {
            return Ok(vec![secret_ref.clone()]);
        }
        // A bare pool ref expands to one ref per stored seat (default
        // first, then sorted labels). Each seat key is parsed back into
        // a provider + optional label so the returned refs round-trip
        // through `Display`/`parse`.
        let seat_keys = {
            let guard = self.inner.file.read().await;
            guard.seats_for_provider(provider)
        };
        // No stored seats yet (not logged in): fall back to the single
        // bare ref so the caller's downstream "not logged in" guidance
        // fires instead of an empty pool that silently resolves to
        // nothing.
        if seat_keys.is_empty() {
            return Ok(vec![secret_ref.clone()]);
        }
        Ok(seat_keys
            .into_iter()
            .map(|key| seat_ref_from_key(provider, &key))
            .collect())
    }
}

/// Reconstruct a `SecretRef::OAuth` from a credentials-map seat key.
/// The unlabeled/default seat keys as the bare provider (label None);
/// a labeled seat keys as `provider#label` (the text after the first
/// `#` is the label). Inverse of `seat_key`.
fn seat_ref_from_key(provider: &str, seat_key: &str) -> SecretRef {
    let label = seat_key
        .strip_prefix(provider)
        .and_then(|rest| rest.strip_prefix('#'))
        .map(str::to_string);
    SecretRef::OAuth {
        provider: provider.to_string(),
        label,
    }
}

#[cfg(test)]
#[path = "seat_tests.rs"]
mod seat_tests;
