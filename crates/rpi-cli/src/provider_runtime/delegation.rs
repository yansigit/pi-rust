use super::*;
use rpi_ai::{AssistantMessage, AssistantMessageEvent, ErrorReason, StopReason};

/// Keep the host binding active for creation AND consumption. The extra hop is
/// bounded by the standard 64-event/4 MiB stream and every push is awaited.
pub(super) fn forward_scoped(
    runtime: Arc<ProviderRuntime>,
    provider: Arc<dyn Provider>,
    binding: DelegationBinding,
    epoch: Arc<DelegationEpoch>,
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let (mut producer, stream) =
        rpi_ai::event_stream::create_assistant_message_event_stream_with_cancellation(
            &options.signal,
        );
    let mut options = options.clone();
    options.signal = producer.cancellation_token();
    let model = model.clone();
    let context = context.clone();
    let scope = rpi_agent::tool_execution_scope::current_scope();
    let closing = scope
        .as_ref()
        .map(|scope| scope.runtime_closing().clone())
        .unwrap_or_default();
    tokio::spawn(async move {
        let work = async move {
            let _runtime = runtime;
            let signal = options.signal.clone();
            let _cancel = signal.clone().drop_guard();
            let revoked = epoch.signal();
            let operation = async {
                if let Err(error) = provider.preflight(&model, signal.clone()).await {
                    producer.push(failure(&model, error, false)).await;
                    return;
                }
                let mut source =
                    rpi_ai::provider::forward_stream(provider.as_ref(), &model, &context, &options)
                        .await;
                while let Some(event) = source.next().await {
                    let terminal = event.is_terminal();
                    if !producer.push(event).await || terminal {
                        return;
                    }
                }
                producer
                    .push(failure(
                        &model,
                        "Provider stream ended without a terminal event".into(),
                        false,
                    ))
                    .await;
            };
            tokio::select! { biased;
                _ = signal.cancelled() => {},
                _ = revoked.cancelled() => { signal.cancel(); },
                _ = closing.cancelled() => { signal.cancel(); },
                _ = operation => return,
            }
            producer
                .push(failure(
                    &model,
                    "provider request was cancelled".into(),
                    true,
                ))
                .await;
        };
        rpi_agent::tool_execution_scope::inherit(scope, binding.scope(work)).await;
    });
    stream
}
fn failure(model: &Model, message: String, aborted: bool) -> AssistantMessageEvent {
    AssistantMessageEvent::Error {
        reason: if aborted {
            ErrorReason::Aborted
        } else {
            ErrorReason::Error
        },
        error: AssistantMessage::terminal(
            model.api.clone(),
            &model.provider,
            &model.id,
            if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
            message,
            0,
        ),
    }
}
