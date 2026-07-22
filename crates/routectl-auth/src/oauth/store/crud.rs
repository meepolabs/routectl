//! Token-record CRUD and cloud-project-id cache adapter.

use crate::oauth::file_io;
use crate::oauth::types::{TokenRecord, unix_now};
use crate::oauth::{OAuthError, OAuthResult};

use super::{LocalProbe, OAuthStore};

impl OAuthStore {
    /// Read the stable account id recorded for `provider`, if any.
    /// Read-only: no expiry check, no refresh, no network. Returns
    /// `None` when the provider has no stored record (not logged in)
    /// OR when the record carries no `account_id` (some token-endpoint
    /// responses omit it). The `chatgpt_account_id` is stable across
    /// token rotations, so the openai-responses factory reads it once
    /// at build time to populate `OpenAiResponsesConfig.account_id`
    /// when the operator omits `account_id_ref` on an `oauth://` ref.
    pub async fn peek_account_id(&self, provider: &str) -> Option<String> {
        self.inner
            .file
            .read()
            .await
            .get(provider)
            .and_then(|rec| rec.account.account_id.clone())
    }

    /// Read the per-credential `session_id` recorded for `provider`
    /// (a seat key), if any. Read-only: no expiry check, no refresh, no
    /// network. Returns `None` when the provider has no stored record
    /// (not logged in) OR when the record carries no `session_id` (a
    /// pre-existing credential minted before session-id support, or one
    /// that has only ever been refreshed). The anthropic-api factory
    /// reads this once at build time to populate
    /// `AnthropicApiConfig.session_id` for the Claude-Code session-id
    /// header.
    pub async fn peek_session_id(&self, provider: &str) -> Option<String> {
        self.inner
            .file
            .read()
            .await
            .get(provider)
            .and_then(|rec| rec.session_id.clone())
    }

    /// Read the Cloud Code project id recorded for `provider` (a seat
    /// key), if any. Read-only: no expiry check, no refresh, no network.
    /// Returns `None` when the provider has no stored record (not logged
    /// in) OR when the record carries no `cloud_project_id` (a
    /// credential minted before Cloud Code support, or one that has not
    /// yet resolved a project id). The Gemini provider reads this at
    /// startup to skip the project-id resolution round trip on warm
    /// restarts.
    pub async fn peek_cloud_project_id(&self, provider: &str) -> Option<String> {
        self.inner
            .file
            .read()
            .await
            .get(provider)
            .and_then(|rec| rec.cloud_project_id.clone())
    }

    /// Persist a resolved Cloud Code project id for `provider` (a seat
    /// key). Looks up the existing record, sets `cloud_project_id`, and
    /// writes back atomically using the same disk-first ordering as
    /// `write_record`. Returns `OAuthError::NotLoggedIn` when no record
    /// exists for `provider` -- the Gemini provider must be logged in
    /// before a project id can be cached.
    pub async fn set_cloud_project_id(&self, provider: &str, project_id: &str) -> OAuthResult<()> {
        // A degraded store must never overwrite a file it could not read
        // (a schema-mismatched or momentarily unreadable file would be
        // lost). Refuse before the lock helper and surface the cause.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
        let mut guard = self.inner.file.write().await;
        let provider = provider.to_string();
        let project_id = project_id.to_string();
        let (merged, found) = file_io::update_under_lock(&self.inner.path, {
            let provider = provider.clone();
            move |cf| match cf.get(&provider).cloned() {
                Some(mut rec) => {
                    rec.cloud_project_id = Some(project_id);
                    cf.upsert(&provider, rec);
                    file_io::Mutation {
                        directive: file_io::WriteDirective::Write,
                        report: true,
                    }
                }
                // Absent from the disk-fresh state (never logged in, or a
                // sibling logged out): do not create a seat -- report it so
                // the caller surfaces NotLoggedIn.
                None => file_io::Mutation {
                    directive: file_io::WriteDirective::Skip,
                    report: false,
                },
            }
        })
        .await?;
        // Commit the merged disk-fresh state to the cache even on the
        // not-found path: a sibling's logout observed on disk must clear
        // the stale in-memory seat immediately, not at the next reload.
        *guard = merged;
        if !found {
            return Err(OAuthError::NotLoggedIn(provider));
        }
        Ok(())
    }

    /// Compare-and-clear the persisted Cloud Code project id for
    /// `provider` (a seat key). Clears the `cloud_project_id` field only
    /// when it equals `expected`, using the same disk-first
    /// `update_under_lock` discipline as `set_cloud_project_id`. Returns
    /// `Ok(true)` when it matched and was cleared (persisted to disk),
    /// `Ok(false)` when the stored id differed, was absent, or the record
    /// itself was missing (no write in any of those cases).
    ///
    /// The equality guard is the whole point: a late failure carrying a
    /// stale id must not wipe a fresh id a concurrent request already
    /// re-resolved. The durable copy is what survives restarts, so the
    /// clear persists rather than only dropping the in-memory value.
    /// A missing record is not an error -- an un-onboarded seat has
    /// nothing to clear.
    pub async fn clear_cloud_project_id_if_matches(
        &self,
        provider: &str,
        expected: &str,
    ) -> OAuthResult<bool> {
        // A degraded store must never overwrite a file it could not read.
        // Refuse before the lock helper and surface the cause.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
        let mut guard = self.inner.file.write().await;
        let provider = provider.to_string();
        let expected = expected.to_string();
        let (merged, cleared) = file_io::update_under_lock(&self.inner.path, {
            let provider = provider.clone();
            move |cf| match cf.get(&provider).cloned() {
                Some(mut rec) if rec.cloud_project_id.as_deref() == Some(expected.as_str()) => {
                    rec.cloud_project_id = None;
                    cf.upsert(&provider, rec);
                    file_io::Mutation {
                        directive: file_io::WriteDirective::Write,
                        report: true,
                    }
                }
                // Record present but the id differs or is absent, or no
                // record at all: nothing to clear. Leave the file
                // byte-identical.
                _ => file_io::Mutation {
                    directive: file_io::WriteDirective::Skip,
                    report: false,
                },
            }
        })
        .await?;
        // Commit the merged disk-fresh state to the cache on every path so
        // a sibling's concurrent change observed on disk is not lost.
        *guard = merged;
        Ok(cleared)
    }

    /// Persist a token record. Takes the in-process write guard first, then
    /// merges the one-seat upsert onto the disk-fresh state under the
    /// cross-process advisory lock (`file_io::update_under_lock`), and
    /// commits the returned merged file to the in-memory cache. Re-reading
    /// under the lock is what stops a stale cache from clobbering a seat a
    /// sibling process wrote concurrently; a failed disk write leaves both
    /// halves consistent (the cache is committed only after the write
    /// succeeds). Login upserts UNCONDITIONALLY -- it is the one mutation
    /// that establishes a seat regardless of the prior on-disk state.
    pub(crate) async fn write_record(&self, provider: &str, rec: TokenRecord) -> OAuthResult<()> {
        // A degraded store must never overwrite a file it could not read.
        // Refuse before the lock helper and surface the cause.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
        let mut guard = self.inner.file.write().await;
        let provider = provider.to_string();
        let seat_key = provider.clone();
        let (merged, ()) = file_io::update_under_lock(&self.inner.path, move |cf| {
            cf.upsert(&provider, rec);
            file_io::Mutation {
                directive: file_io::WriteDirective::Write,
                report: (),
            }
        })
        .await?;
        *guard = merged;
        // Reset trigger: a login/writeback for this seat supersedes any
        // stale transient cooldown -- the credential state just changed.
        self.clear_cooldown(&seat_key);
        Ok(())
    }

    /// Remove a provider's tokens (used by `routectl logout`). Same
    /// re-read-under-lock merge as `write_record`: the removal targets the
    /// DISK-FRESH state, so a sibling seat written since the cache loaded
    /// survives. Reports whether the seat was present in the disk-fresh
    /// state (`Ok(false)` when absent, preserving first-time-logout
    /// semantics), and writes nothing when there was nothing to remove.
    pub(crate) async fn remove_provider(&self, provider: &str) -> OAuthResult<bool> {
        // A degraded store must never overwrite a file it could not read.
        // Refuse before the lock helper and surface the cause.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
        let mut guard = self.inner.file.write().await;
        let provider = provider.to_string();
        let seat_key = provider.clone();
        let (merged, was_present) = file_io::update_under_lock(&self.inner.path, move |cf| {
            let was_present = cf.remove(&provider).is_some();
            let directive = if was_present {
                file_io::WriteDirective::Write
            } else {
                file_io::WriteDirective::Skip
            };
            file_io::Mutation {
                directive,
                report: was_present,
            }
        })
        .await?;
        *guard = merged;
        // Reset trigger: a logout for this seat clears any stale
        // cooldown so a subsequent re-login starts from a clean slate.
        self.clear_cooldown(&seat_key);
        Ok(was_present)
    }

    /// Snapshot the set of credential (seat) keys currently in the
    /// in-memory cache -- every key across the `providers` map, including
    /// labeled seats (`provider#label`). The reload coordinator snapshots
    /// this before and after `reload_from_disk` to decide whether the seat
    /// set actually changed (a login/logout adds or removes a key) versus a
    /// routine token-value-only refresh (same keys), gating an expensive
    /// Router rebuild on the former. Read under the same file `RwLock` as
    /// `list`, so the snapshot is consistent with the cache it reflects.
    pub async fn credential_keys(&self) -> std::collections::BTreeSet<String> {
        self.inner
            .file
            .read()
            .await
            .providers
            .keys()
            .cloned()
            .collect()
    }

    /// Read-only credential probe for one provider. Reports token
    /// presence from the in-memory cache WITHOUT any network I/O -- never
    /// calls `get`/`refresh_under_lock`, never touches the token endpoint.
    /// Consumed by the activation compute; the resolution semantics are
    /// deliberately more lenient than `get`'s near-expiry refresh trigger.
    ///
    /// Any seat of the provider (bare or labeled) resolving counts as
    /// `Present`. `Present` when a seat's access token is unexpired
    /// (raw `expires_at_unix > now`, NOT the 300s near-expiry lead) OR a
    /// refresh token is stored. `Expired` when a record exists but every
    /// seat's access token is expired AND carries no refresh token.
    /// `Missing` when no record exists for the provider. Never returns
    /// `StoreUnavailable` -- that is a caller-side value for when no oauth
    /// store exists at all.
    pub async fn probe_local(&self, provider_id: &str) -> LocalProbe {
        let guard = self.inner.file.read().await;
        let seats = guard.seats_for_provider(provider_id);
        if seats.is_empty() {
            return LocalProbe::Missing;
        }
        let now = unix_now();
        for seat in &seats {
            let Some(rec) = guard.get(seat) else {
                continue;
            };
            if rec.is_locally_usable(now) {
                return LocalProbe::Present;
            }
        }
        LocalProbe::Expired
    }

    /// Snapshot all known provider records (for `routectl whoami`).
    pub async fn list(&self) -> Vec<(String, TokenRecord)> {
        self.inner
            .file
            .read()
            .await
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Remove a provider's tokens by name (used by `routectl logout`).
    /// Returns `Ok(true)` when a record existed and was removed,
    /// `Ok(false)` when no record was present (first-time logout is not
    /// an error). Named `logout` rather than `delete(&str)` to avoid
    /// shadowing the `SecretStore::delete(&SecretRef)` trait method.
    pub async fn logout(&self, provider: &str) -> OAuthResult<bool> {
        self.remove_provider(provider).await
    }
}

#[cfg(test)]
#[path = "crud_tests.rs"]
mod crud_tests;
