#[cfg(all(test, feature = "unit-eval"))]
use futures::future::LocalBoxFuture;
#[cfg(all(test, feature = "unit-eval"))]
use gpui::TestAppContext;
#[cfg(all(test, feature = "unit-eval"))]
use std::{fmt::Display, time::Duration};

#[cfg(all(test, feature = "unit-eval"))]
mod edit_file;
#[cfg(all(test, feature = "unit-eval"))]
mod terminal_tool;
#[cfg(all(test, feature = "unit-eval"))]
mod write_file;

#[cfg(all(test, feature = "unit-eval"))]
fn completion_retry_delay(error: &anyhow::Error, retry_attempt: usize) -> Option<Duration> {
    error
        .downcast_ref::<language_model::LanguageModelCompletionError>()?
        .retry_delay(retry_attempt)
        .map(crate::jitter_retry_delay)
}

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
    const MAX_ATTEMPTS: usize = 20;
    let mut completed_attempts = 0;

    loop {
        let response = request().await;
        completed_attempts += 1;

        if completed_attempts >= MAX_ATTEMPTS {
            return response;
        }

        let retry_attempt = completed_attempts;
        let retry_delay = response
            .as_ref()
            .err()
            .and_then(|error| completion_retry_delay(error, retry_attempt));

        if let Some(retry_delay) = retry_delay {
            eprintln!("Retry attempt #{retry_attempt}: Retry after {retry_delay:?}");
            #[allow(clippy::disallowed_methods)]
            async_io::Timer::after(retry_delay).await;
        } else {
            return response;
        }
    }
}
