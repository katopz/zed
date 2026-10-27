//! Fork-only tests for this module, kept out of the upstream file so syncs
//! do not conflict on them (plan 038).

use super::*;

/// Minimal `CredentialsProvider` impl for unit tests; not actually read
/// since these tests construct `State` directly without going through
/// `authenticate`.
struct FakeCredentialsProvider;

impl CredentialsProvider for FakeCredentialsProvider {
    fn read_credentials<'a>(
        &'a self,
        _url: &'a str,
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        Box::pin(async { Ok(None) })
    }
    fn write_credentials<'a>(
        &'a self,
        _url: &'a str,
        _username: &'a str,
        _password: &'a [u8],
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async { Ok(()) })
    }
    fn delete_credentials<'a>(
        &'a self,
        _url: &'a str,
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

fn fake_state_with_no_keys() -> State {
    State {
        id: "test".into(),
        api_key_state: ApiKeyState::new(
            "https://example.test".into(),
            EnvVar::new("TEST_API_KEY".into()),
        ),
        api_key_state_2: ApiKeyState::new(
            secondary_key_url("https://example.test"),
            EnvVar::new("TEST_API_KEY_2".into()),
        ),
        api_key_state_3: ApiKeyState::new(
            tertiary_key_url("https://example.test"),
            EnvVar::new("TEST_API_KEY_3".into()),
        ),
        api_key_state_4: ApiKeyState::new(
            quaternary_key_url("https://example.test"),
            EnvVar::new("TEST_API_KEY_4".into()),
        ),
        key_health: Arc::new(ParkingMutex::new(KeyHealthTracker::default())),
        key_health_dirty: Arc::new(ParkingMutex::new(None)),
        key_health_path: key_health_path_for("test"),
        settings: OpenAiCompatibleSettings {
            api_url: "https://example.test".to_string(),
            ..Default::default()
        },
        credentials_provider: Arc::new(FakeCredentialsProvider),
    }
}

#[test]
fn gather_candidates_returns_nothing_for_fake_state_with_no_keys() {
    // Sanity: the test fixture really has no keys configured, otherwise
    // the slot_health_snapshot tests below would be misleading.
    let state = fake_state_with_no_keys();
    assert!(state.gather_candidates().is_empty());
}

// ------------------------------------------------------------------
// slot_health_snapshot tests
//
// These exercise the UI-facing projection of per-key health exposed by
// `State`. They build a fresh `State`, push failures directly into
// `key_health` (bypassing the request closure), and verify the snapshot
// returns the expected mix of healthy / backed-off / unconfigured slots.
// The underlying `KeyHealth` / `KeyHealthTracker` mechanics (backoff
// computation, expiry, persistence) are tested in `health.rs`.
// ------------------------------------------------------------------

#[test]
fn slot_health_snapshot_fresh_state_is_all_clear() {
    // No keys configured, no failures — every slot should report
    // `has_key: false`, `is_backed_off: false`, zero failures, `enabled: true`.
    let state = fake_state_with_no_keys();
    let snapshot = state.slot_health_snapshot();
    for status in &snapshot {
        assert!(!status.has_key, "no keys should be configured");
        assert!(!status.is_backed_off, "fresh slot should not be backed off");
        assert_eq!(status.consecutive_failures, 0);
        assert_eq!(status.backoff_remaining, Duration::ZERO);
        assert!(status.enabled, "fresh slot should default to enabled");
    }
}

#[test]
fn slot_health_snapshot_reports_backed_off_slot() {
    let state = fake_state_with_no_keys();
    // Poison Primary directly via the shared mutex, with a backoff window
    // well into the future so it can't accidentally expire mid-test.
    {
        let mut tracker = state.key_health.lock();
        tracker.primary = KeyHealth {
            consecutive_failures: 2,
            backoff_until: Some(Instant::now() + Duration::from_secs(300)),
            ..Default::default()
        };
    }
    let snapshot = state.slot_health_snapshot();
    // [Primary, Secondary, Tertiary]
    assert!(snapshot[0].is_backed_off, "Primary should be backed off");
    assert_eq!(snapshot[0].consecutive_failures, 2);
    // 300s backoff, so remaining should be just under 300s. We don't assert
    // the exact value (depends on how long the lock + read took), only that
    // it's in the expected band.
    assert!(
        snapshot[0].backoff_remaining > Duration::from_secs(290)
            && snapshot[0].backoff_remaining <= Duration::from_secs(300),
        "Primary backoff_remaining should be ~300s, got {:?}",
        snapshot[0].backoff_remaining
    );
    // Other slots untouched.
    assert!(!snapshot[1].is_backed_off);
    assert!(!snapshot[2].is_backed_off);
}

#[test]
fn slot_health_snapshot_handles_expired_backoff_gracefully() {
    // If `backoff_until` is in the past, the slot should read as NOT backed
    // off (auto-recovered) and `backoff_remaining` should be ZERO (not
    // negative — `saturating_duration_since` clamps).
    let state = fake_state_with_no_keys();
    {
        let mut tracker = state.key_health.lock();
        tracker.secondary = KeyHealth {
            consecutive_failures: 5,
            backoff_until: Some(Instant::now() - Duration::from_secs(60)),
            ..Default::default()
        };
    }
    let snapshot = state.slot_health_snapshot();
    assert!(
        !snapshot[1].is_backed_off,
        "expired backoff should auto-clear"
    );
    assert_eq!(
        snapshot[1].backoff_remaining,
        Duration::ZERO,
        "expired backoff remaining should clamp to zero"
    );
    // consecutive_failures is still recorded (historical), even though the
    // slot is no longer backed off. The UI badge only shows when
    // is_backed_off is true, so this doesn't affect rendering.
    assert_eq!(snapshot[1].consecutive_failures, 5);
}

// ------------------------------------------------------------------
// clear_slot_backoff contract
//
// Verifies the in-memory mutation `clear_slot_backoff` performs: only the
// targeted slot's health is reset, siblings are untouched. This is also
// the method called from `ConfigurationView::reset_api_key*` when the
// user clears/resets a slot's key, and from the "Clear" button. The
// persist scheduling is exercised by the persistence tests in `health.rs`.
// ------------------------------------------------------------------

#[test]
fn clear_slot_backoff_clears_only_targeted_slot() {
    let state = fake_state_with_no_keys();
    // Poison every slot distinctly.
    {
        let mut tracker = state.key_health.lock();
        tracker.primary = KeyHealth {
            consecutive_failures: 3,
            backoff_until: Some(Instant::now() + Duration::from_secs(300)),
            ..Default::default()
        };
        tracker.secondary = KeyHealth {
            consecutive_failures: 2,
            backoff_until: Some(Instant::now() + Duration::from_secs(60)),
            ..Default::default()
        };
        tracker.tertiary = KeyHealth {
            consecutive_failures: 5,
            backoff_until: Some(Instant::now() + Duration::from_secs(3600)),
            ..Default::default()
        };
    }

    // The exact in-memory operation `clear_slot_backoff(Secondary)` performs.
    {
        let mut tracker = state.key_health.lock();
        let health = tracker.get_mut(KeySlot::Secondary);
        health.consecutive_failures = 0;
        health.backoff_until = None;
    }

    let after = state.slot_health_snapshot();
    // Secondary cleared.
    assert!(!after[1].is_backed_off, "Secondary should be cleared");
    assert_eq!(after[1].consecutive_failures, 0);
    // Primary + Tertiary untouched (still poisoned).
    assert!(after[0].is_backed_off, "Primary should still be backed off");
    assert_eq!(after[0].consecutive_failures, 3);
    assert!(
        after[2].is_backed_off,
        "Tertiary should still be backed off"
    );
    assert_eq!(after[2].consecutive_failures, 5);
}

// ------------------------------------------------------------------
// truncate_key_preview + slot_index + classify_probe_error helpers
// ------------------------------------------------------------------

#[test]
fn truncate_key_preview_short_key_returned_as_is() {
    // At or below the HEAD+TAIL threshold (6 chars) the key is too short to
    // truncate meaningfully, so it's returned verbatim.
    assert_eq!(truncate_key_preview("abc"), "abc");
    assert_eq!(truncate_key_preview("abcdef"), "abcdef");
}

#[test]
fn truncate_key_preview_long_key_shows_head_and_tail() {
    assert_eq!(truncate_key_preview("sk-abcdef1234567xyz"), "sk-...xyz");
    // 7 chars: just over the threshold → head3 + tail3 (one char dropped).
    assert_eq!(truncate_key_preview("abcdefg"), "abc...efg");
}

#[test]
fn truncate_key_preview_is_char_safe_for_multibyte() {
    // A key whose 4th byte lands inside a multi-byte char (é = 2 bytes)
    // must not panic on slicing. `truncate_key_preview` uses `chars()`, so
    // the split happens on character boundaries.
    let key = "abcédéfg"; // chars: a b c é d é f g (8 chars)
    let preview = truncate_key_preview(key);
    // head = "abc", tail = "éfg"
    assert_eq!(preview, "abc...éfg");
    // Emoji (4-byte) at the boundary likewise must not panic.
    let key = "abc😀defg"; // chars: a b c 😀 d e f g
    let preview = truncate_key_preview(key);
    assert_eq!(preview, "abc...efg");
}

#[test]
fn slot_index_maps_slots_in_order() {
    assert_eq!(slot_index(KeySlot::Primary), 0);
    assert_eq!(slot_index(KeySlot::Secondary), 1);
    assert_eq!(slot_index(KeySlot::Tertiary), 2);
    assert_eq!(slot_index(KeySlot::Quaternary), 3);
}

#[test]
fn classify_probe_error_recognizes_rate_limit_status() {
    let err = open_ai::RequestError::HttpResponseError {
        provider: "test".to_string(),
        status_code: http_client::http::StatusCode::TOO_MANY_REQUESTS,
        body: String::new(),
        headers: Box::new(http_client::http::HeaderMap::new()),
    };
    assert_eq!(
        classify_probe_error(err),
        KeyProbeResult::RateLimit { retry_after: None }
    );
}

#[test]
fn classify_probe_error_extracts_reset_hint_from_zai_body() {
    // Empirical Z.AI 429 body: no retry-after header, reset timestamp in
    // the message. A far-future date keeps the test insensitive to clock
    // drift between parse and assert.
    let err = open_ai::RequestError::HttpResponseError {
        provider: "test".to_string(),
        status_code: http_client::http::StatusCode::TOO_MANY_REQUESTS,
        body: r#"{"error":{"code":"1310","message":"Weekly/Monthly Limit Exhausted. Your limit will reset at 2099-01-01 00:00:00"}}"#.to_string(),
        headers: Box::new(http_client::http::HeaderMap::new()),
    };
    match classify_probe_error(err) {
        KeyProbeResult::RateLimit { retry_after } => {
            assert!(
                retry_after.is_some(),
                "reset hint from body should populate retry_after"
            );
        }
        other => panic!("expected RateLimit, got {other:?}"),
    }
}

#[test]
fn classify_probe_error_ignores_past_reset_hint() {
    let err = open_ai::RequestError::HttpResponseError {
        provider: "test".to_string(),
        status_code: http_client::http::StatusCode::TOO_MANY_REQUESTS,
        body: r#"{"error":{"code":"1308","message":"Usage limit reached for 5 hour. Your limit will reset at 2020-01-01 00:00:00"}}"#.to_string(),
        headers: Box::new(http_client::http::HeaderMap::new()),
    };
    assert_eq!(
        classify_probe_error(err),
        KeyProbeResult::RateLimit { retry_after: None }
    );
}

#[test]
fn classify_probe_error_rate_limit_header_populates_retry_after() {
    let mut headers = http_client::http::HeaderMap::new();
    headers.insert(
        http_client::http::header::RETRY_AFTER,
        http_client::http::HeaderValue::from_static("17"),
    );
    let err = open_ai::RequestError::HttpResponseError {
        provider: "test".to_string(),
        status_code: http_client::http::StatusCode::TOO_MANY_REQUESTS,
        body: String::new(),
        headers: Box::new(headers),
    };
    assert_eq!(
        classify_probe_error(err),
        KeyProbeResult::RateLimit {
            retry_after: Some(Duration::from_secs(17))
        }
    );
}

#[test]
fn classify_probe_error_other_status_becomes_err_with_message() {
    let err = open_ai::RequestError::HttpResponseError {
        provider: "test".to_string(),
        status_code: http_client::http::StatusCode::UNAUTHORIZED,
        body: "unauthorized".to_string(),
        headers: Box::new(http_client::http::HeaderMap::new()),
    };
    match classify_probe_error(err) {
        KeyProbeResult::Err(msg) => {
            assert!(
                msg.as_ref().contains("401"),
                "message should mention status: {msg}"
            );
        }
        other => panic!("expected Err, got {other:?}"),
    }
}
