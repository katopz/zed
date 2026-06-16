# OpenAI Compatible: Dual API Key with Random Load Balancing

## Goal
Add a second API key input for `openai_compatible` providers so requests randomly pick
between the two keys for load balancing.

## Tasks

- [x] Add `api_key_state_2` field to `State` struct in `open_ai_compatible.rs`
- [x] Add secondary key URL helper (`{api_url}#secondary` as keychain identifier)
- [x] Update `State::new` to initialize both `ApiKeyState` instances with derived env vars
- [x] Update `State::is_authenticated` to return true if either key is present
- [x] Update `State::authenticate` to load both keys
- [x] Add `State::set_api_key_2` for saving/resetting secondary key
- [x] Update `State::set_api_key` URL-change observer to handle both keys
- [x] Add key selection helper that collects available keys and picks randomly
- [x] Update `stream_completion` to use random key selection
- [x] Update `stream_response` to use random key selection
- [x] Update `reset_credentials` to clear both keys
- [x] Update `ConfigurationView` with second input field + independent save/reset

## Design

- Primary key: stored at `{api_url}` in keychain, env var `{PROVIDER}_API_KEY`
- Secondary key: stored at `{api_url}#secondary` in keychain, env var `{PROVIDER}_API_KEY_2`
- At request time: collect available keys, randomly pick one via `rand::seq::IndexedRandom`
- Both keys are optional independently; provider is authenticated if at least one is present
