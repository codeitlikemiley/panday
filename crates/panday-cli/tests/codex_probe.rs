//! M25.10 — is a Codex login usable by the `openai_compat` adapter?
//!
//! `#[ignore]`d and live, per docs/25 §Testing contract: "Live provider calls in
//! CI. Mocks until an `#[ignore]` probe the operator runs." Run it yourself:
//!
//! ```sh
//! cargo test -p panday-cli --test codex_probe -- --ignored --nocapture
//! ```
//!
//! It answers one question and does not pretend to answer more: does the token
//! `panday creds add --from-codex` imports **authenticate** against
//! `api.openai.com`? Whether that account can then afford a completion is a
//! billing fact about the account, not a property of the importer.

use panday_cli::creds::{access_token_from_codex_json, codex_auth_path};
use panday_sdk::providers::openai_compat::OpenAiCompatClient;
use panday_sdk::{ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling};

fn one_token_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: ModelRef(model.into()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "ping".into(),
            }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        sampling: Sampling {
            max_tokens: Some(1),
            ..Default::default()
        },
        cache: Default::default(),
        stream: true,
        metadata: CallMeta {
            account: AccountId::new(),
            request: RequestId::new(),
            session: None,
            turn: None,
            task: None,
        },
    }
}

#[tokio::test]
#[ignore = "live: reads your Codex login and calls api.openai.com once"]
async fn a_codex_login_authenticates_against_the_openai_adapter() {
    let path = codex_auth_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        eprintln!("no Codex login at {} — nothing to probe", path.display());
        return;
    };
    let access = access_token_from_codex_json(&raw).expect("Codex auth.json parses");

    let client = OpenAiCompatClient::new("https://api.openai.com/v1", Some(access));
    let outcome = client.chat(one_token_request("gpt-4o-mini")).await;

    match &outcome {
        Ok(_) => eprintln!("PROVEN: the adapter served a completion on the Codex token"),
        Err(PandayError::RateLimited { .. }) => eprintln!(
            "PROVEN at the auth layer: the credential was accepted and refused on quota. \
             A rejected token returns HTTP 401, not 429 — see docs/25 M25.10."
        ),
        Err(e) => eprintln!("outcome: {e}"),
    }

    // The assertion is narrow on purpose. 401 means the adapter cannot use a
    // Codex login at all, which is the finding that would make M25.10 a "skip".
    // Anything else means the importer produced a credential the adapter can
    // authenticate with, whatever the account can afford afterwards.
    if let Err(PandayError::Provider { message, .. }) = &outcome {
        assert!(
            !message.contains("HTTP 401"),
            "Codex token was rejected outright — the importer cannot feed the \
             openai adapter and docs/25 M25.10 needs rewriting: {message}"
        );
    }
}
