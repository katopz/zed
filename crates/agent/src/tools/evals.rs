#[cfg(all(test, feature = "unit-eval"))]
use futures::future::LocalBoxFuture;
#[cfg(all(test, feature = "unit-eval"))]
use gpui::TestAppContext;
#[cfg(all(test, feature = "unit-eval"))]
use http_client::StatusCode;
#[cfg(all(test, feature = "unit-eval"))]
use language_model::LanguageModelCompletionError;
#[cfg(all(test, feature = "unit-eval"))]
use std::fmt::Display;
#[cfg(all(test, feature = "unit-eval"))]
use std::time::Duration;

#[cfg(all(test, feature = "unit-eval"))]
mod edit_file;
#[cfg(all(test, feature = "unit-eval"))]
mod terminal_tool;
#[cfg(all(test, feature = "unit-eval"))]
mod write_file;

#[cfg(all(test, feature = "unit-eval"))]
fn run_gpui_eval<T>(
    eval: impl for<'a> FnOnce(&'a mut TestAppContext) -> LocalBoxFuture<'a, anyhow::Result<T>>,
    outcome: impl FnOnce(&T) -> eval_utils::OutcomeKind,
) -> eval_utils::EvalOutput<()>
where
    T: Display,
{
    let dispatcher = gpui::TestDispatcher::new(rand::random());
    let mut cx = TestAppContext::build(dispatcher.clone(), None);
    let entity_refcounts = cx.app.borrow().ref_counts_drop_handle();
    let foreground_executor = cx.foreground_executor().clone();
    let result = foreground_executor.block_test(eval(&mut cx));

    cx.run_until_parked();
    cx.update(|cx| {
        cx.background_executor().forbid_parking();
        cx.quit();
    });
    cx.run_until_parked();
    drop(cx);
    dispatcher.drain_tasks();
    drop(dispatcher);
    drop(entity_refcounts);

    match result {
        Ok(output) => eval_utils::EvalOutput {
            data: output.to_string(),
            outcome: outcome(&output),
            metadata: (),
        },
        Err(err) => eval_utils::EvalOutput {
            data: format!("{err:?}"),
            outcome: eval_utils::OutcomeKind::Error,
            metadata: (),
        },
    }
}

/// Retries a completion request on rate-limit / overload errors so unattended
/// evals survive provider throttling. Shared by all tool evals.
#[cfg(all(test, feature = "unit-eval"))]
async fn retry_on_rate_limit<R>(
    mut request: impl AsyncFnMut() -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    const MAX_RETRIES: usize = 20;
    let mut attempt = 0;

    loop {
        attempt += 1;
        let response = request().await;

        if attempt >= MAX_RETRIES {
            return response;
        }

        let retry_delay = match &response {
            Ok(_) => None,
            Err(err) => match err.downcast_ref::<LanguageModelCompletionError>() {
                Some(err) => match &err {
                    LanguageModelCompletionError::RateLimitExceeded { retry_after, .. }
                    | LanguageModelCompletionError::ServerOverloaded { retry_after, .. } => {
                        Some(retry_after.unwrap_or(Duration::from_secs(5)))
                    }
                    LanguageModelCompletionError::UpstreamProviderError {
                        status,
                        retry_after,
                        ..
                    } => {
                        let should_retry = matches!(
                            *status,
                            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                        ) || status.as_u16() == 529;

                        if should_retry {
                            Some(retry_after.unwrap_or(Duration::from_secs(5)))
                        } else {
                            None
                        }
                    }
                    LanguageModelCompletionError::ApiReadResponseError { .. }
                    | LanguageModelCompletionError::ApiInternalServerError { .. }
                    | LanguageModelCompletionError::HttpSend { .. } => {
                        Some(Duration::from_secs(2_u64.pow((attempt - 1) as u32).min(30)))
                    }
                    _ => None,
                },
                _ => None,
            },
        };

        if let Some(retry_after) = retry_delay {
            let jitter = retry_after.mul_f64(rand::random_range(0.0..1.0));
            eprintln!("Attempt #{attempt}: Retry after {retry_after:?} + jitter of {jitter:?}");
            #[allow(clippy::disallowed_methods)]
            async_io::Timer::after(retry_after + jitter).await;
        } else {
            return response;
        }
    }
}
